#!/usr/bin/env bash
# Refresh an installed SideALSA without touching the device profile.
#
# This never opens the device/profile setup menus and refuses every option
# that would change device selection. Installed features (Qt GUI, Wine ASIO,
# PipeWire integration) are detected from the previous install manifest and
# repeated, so an update cannot silently retire them. Explicit flags still
# override detection and keep install.sh "complete feature set" semantics.
# The daemon restarts by default unless --no-start is given.

set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
PREFIX="${PREFIX:-/usr/local}"
DESTDIR="${DESTDIR:-}"

die() {
    printf 'error: %s\n' "$*" >&2
    exit 2
}

usage() {
    cat <<'EOF'
Usage: scripts/update.sh [options]

Rebuild and reinstall SideALSA while preserving the installed device profile.
Never dispatches to the setup menus, even with no arguments on a terminal.

Options (forwarded to scripts/install.sh):
  --prefix PATH           Binary and data prefix (default: /usr/local)
  --socket PATH           Daemon socket (default: installed socket)
  --alsa-plugin-dir PATH  ALSA external-plugin directory
  --no-build              Use existing target/release artifacts
  --with-asio             Force Wine ASIO installation (auto-detected otherwise)
  --force                 Replace files not owned by previous install
  --no-start              Enable service without starting it
  --no-pipewire           Force PipeWire integration removal
  --preserve-pipewire     Keep PipeWire files and user services untouched
  --no-gui                Force Qt control panel removal
  -h, --help              Show this help

Refused (use scripts/install.sh instead):
  --profile, --replace-profile, --interactive
EOF
}

[[ "$PREFIX" == /* ]] || die "prefix must be absolute"

args=()
force_asio=0
force_no_gui=0
force_no_pipewire=0
while (($# > 0)); do
    case "$1" in
        --profile|--replace-profile|--interactive)
            die "$1 changes device selection; use scripts/install.sh instead"
            ;;
        --prefix|--socket|--alsa-plugin-dir)
            (($# >= 2)) || die "$1 requires a value"
            args+=("$1" "$2")
            shift 2
            ;;
        --no-build|--force|--no-start|--preserve-pipewire)
            args+=("$1")
            shift
            ;;
        --with-asio)
            force_asio=1
            shift
            ;;
        --no-gui)
            force_no_gui=1
            shift
            ;;
        --no-pipewire)
            force_no_pipewire=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1 (update.sh never changes profiles)"
            ;;
    esac
done

# Repeat the previously installed feature set. A missing manifest means a
# fresh install, which follows install.sh defaults (GUI on, ASIO off).
manifest="$DESTDIR$PREFIX/share/sidealsa/install-manifest"
if ((force_asio == 1)); then
    args+=(--with-asio)
elif [[ -f "$manifest" ]] && grep -Fq 'sidealsa-asio64.dll' "$manifest"; then
    args+=(--with-asio)
fi
if ((force_no_gui == 1)); then
    args+=(--no-gui)
elif [[ -f "$manifest" ]] && ! grep -Fq 'bin/sidealsa-control' "$manifest"; then
    args+=(--no-gui)
fi
if ((force_no_pipewire == 1)); then
    args+=(--no-pipewire)
elif [[ -f "$manifest" ]] && ! grep -Fq 'pipewire.conf.d/99-sidealsa.conf' "$manifest"; then
    args+=(--no-pipewire)
fi

# Keep install.sh from dispatching to the setup menus when no arguments remain.
export SIDEALSA_UPDATE=1
exec bash "$ROOT/scripts/install.sh" "${args[@]}"
