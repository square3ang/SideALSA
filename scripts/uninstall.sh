#!/usr/bin/env bash

# don't execute this file as sudo.

set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
PREFIX="${PREFIX:-/usr/local}"
DESTDIR="${DESTDIR:-}"
FORCE=0

if [[ "$EUID" -eq 0 && -n "${SUDO_USER:-}" && "$SUDO_USER" != root \
    && -z "${SIDEALSA_UNINSTALL_REEXEC:-}" ]]; then
    exec sudo -u "$SUDO_USER" -H env \
        SIDEALSA_UNINSTALL_REEXEC=1 \
        PREFIX="$PREFIX" \
        DESTDIR="$DESTDIR" \
        "$ROOT/scripts/uninstall.sh" "$@"
fi

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

usage() {
    cat <<'EOF'
Usage: scripts/uninstall.sh [options]

Remove files recorded by SideALSA install manifest.

Options:
  --prefix PATH    Binary and data prefix (default: /usr/local)
  --force          Remove files changed after installation
  -h, --help       Show this help

DESTDIR may be set for staged package removal. System services are not changed
when DESTDIR is non-empty.
EOF
}

while (($# > 0)); do
    case "$1" in
        --prefix)
            (($# >= 2)) || die "--prefix requires a path"
            PREFIX=$2
            shift 2
            ;;
        --force)
            FORCE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

[[ "$PREFIX" == /* ]] || die "prefix must be absolute"
[[ "$DESTDIR" == /* || -z "$DESTDIR" ]] || die "DESTDIR must be absolute"
if [[ "$DESTDIR" == "/" ]]; then
    DESTDIR=
fi
DESTDIR="${DESTDIR%/}"

USE_SUDO=0
if [[ -z "$DESTDIR" && "$EUID" -ne 0 ]]; then
    command -v sudo >/dev/null 2>&1 || die "sudo is required for system removal"
    USE_SUDO=1
fi

destination() {
    printf '%s%s' "$DESTDIR" "$1"
}

file_hash() {
    local hash
    read -r hash _ < <(sha256sum "$1")
    printf '%s\n' "$hash"
}

SUDO_READY=0
run_privileged() {
    if ((USE_SUDO == 1)); then
        if ((SUDO_READY == 0)); then
            sudo -v
            SUDO_READY=1
        fi
        sudo "$@"
    else
        "$@"
    fi
}

MANIFEST_PATH="$PREFIX/share/sidealsa/install-manifest"
MANIFEST_ACTUAL="$(destination "$MANIFEST_PATH")"
[[ -f "$MANIFEST_ACTUAL" ]] || die "SideALSA install manifest not found: $MANIFEST_ACTUAL"

if [[ -z "$DESTDIR" ]] && command -v systemctl >/dev/null 2>&1; then
    run_privileged systemctl disable --now sidealsad.service 2>/dev/null || true
    run_privileged systemctl daemon-reload
fi

PATHS=()
declare -A HASHES=()
while IFS=$'\t' read -r hash path; do
    [[ -n "$path" && "$hash" != \#* ]] || continue
    PATHS+=("$path")
    HASHES["$path"]=$hash
done < "$MANIFEST_ACTUAL"

PRESERVED=()
for ((index = ${#PATHS[@]} - 1; index >= 0; index--)); do
    path="${PATHS[$index]}"
    actual="$(destination "$path")"
    [[ -e "$actual" ]] || continue
    if ((FORCE == 0)) && [[ "$(file_hash "$actual")" != "${HASHES[$path]}" ]]; then
        warn "preserving changed file: $actual (use --force to remove)"
        PRESERVED+=("$path")
        continue
    fi
    run_privileged rm -f -- "$actual"
done

if ((${#PRESERVED[@]} == 0)); then
    run_privileged rm -f -- "$MANIFEST_ACTUAL"
else
    temp="$(mktemp)"
    trap 'rm -f "$temp"' EXIT
    {
        printf '# SideALSA install manifest v1\n'
        for path in "${PRESERVED[@]}"; do
            actual="$(destination "$path")"
            printf '%s\t%s\n' "${HASHES[$path]}" "$path"
        done
    } > "$temp"
    run_privileged install -m 0644 "$temp" "$MANIFEST_ACTUAL"
fi

for directory in \
    "$(destination "$PREFIX/share/doc/sidealsa")" \
    "$(destination "$PREFIX/share/sidealsa")" \
    "$(destination /etc/sidealsa/profiles)" \
    "$(destination /etc/sidealsa)"; do
    run_privileged rmdir --ignore-fail-on-non-empty "$directory" 2>/dev/null || true
done

if [[ -z "$DESTDIR" ]]; then
    printf 'SideALSA uninstalled\n'
    printf 'restart user PipeWire session if it was running:\n'
    printf '  systemctl --user restart pipewire.service pipewire-pulse.service wireplumber.service\n'
else
    printf 'SideALSA staged files removed\n'
fi
