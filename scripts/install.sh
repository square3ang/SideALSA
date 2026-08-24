#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
PREFIX="${PREFIX:-/usr/local}"
DESTDIR="${DESTDIR:-}"
PROFILE_SOURCE="$ROOT/profiles/topping-e1x2.toml"
SOCKET_PATH="${SIDEALSA_SOCKET:-/tmp/sidealsad.sock}"
ALSA_PLUGIN_DIR="${ALSA_PLUGIN_DIR:-}"
NO_BUILD=0
WITH_ASIO=0
FORCE=0
REPLACE_PROFILE=0
NO_START=0
INSTALL_PIPEWIRE=1
USER_AUDIO_WAS_STOPPED=0
TMP_DIR=

if [[ "$EUID" -eq 0 && -n "${SUDO_USER:-}" && "$SUDO_USER" != root \
    && -z "${SIDEALSA_INSTALL_REEXEC:-}" ]]; then
    exec sudo -u "$SUDO_USER" -H env \
        SIDEALSA_INSTALL_REEXEC=1 \
        PREFIX="$PREFIX" \
        DESTDIR="$DESTDIR" \
        SIDEALSA_SOCKET="$SOCKET_PATH" \
        ALSA_PLUGIN_DIR="$ALSA_PLUGIN_DIR" \
        "$ROOT/scripts/install.sh" "$@"
fi

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

info() {
    printf '%s\n' "$*"
}

wait_for_socket() {
    local attempts=50
    while ((attempts > 0)); do
        if [[ -S "$SOCKET_PATH" ]]; then
            return 0
        fi
        sleep 0.1
        ((attempts--))
    done
    die "sidealsad socket did not appear: $SOCKET_PATH"
}

stop_user_audio() {
    if [[ -n "$DESTDIR" || "$EUID" -eq 0 || "$NO_START" -eq 1 || "$INSTALL_PIPEWIRE" -eq 0 ]]; then
        return
    fi
    command -v systemctl >/dev/null 2>&1 || return
    if ! systemctl --user is-active --quiet pipewire.service \
        && ! systemctl --user is-active --quiet pipewire-pulse.service \
        && ! systemctl --user is-active --quiet wireplumber.service \
        && ! systemctl --user is-active --quiet pipewire.socket \
        && ! systemctl --user is-active --quiet pipewire-pulse.socket; then
        return
    fi
    info "stopping user PipeWire session before replacing SideALSA daemon"
    systemctl --user stop \
        pipewire-pulse.socket pipewire.socket pipewire-pulse.service \
        wireplumber.service pipewire.service
    USER_AUDIO_WAS_STOPPED=1
}

restore_user_audio() {
    if ((USER_AUDIO_WAS_STOPPED == 0)); then
        return
    fi
    systemctl --user reset-failed \
        pipewire.socket pipewire.service pipewire-pulse.socket \
        pipewire-pulse.service wireplumber.service || true
    systemctl --user start pipewire.socket pipewire-pulse.socket
    systemctl --user start pipewire.service pipewire-pulse.service wireplumber.service
    USER_AUDIO_WAS_STOPPED=0
}

cleanup() {
    local status=$?
    trap - EXIT
    if [[ -n "$TMP_DIR" ]]; then
        rm -rf -- "$TMP_DIR"
    fi
    restore_user_audio || true
    exit "$status"
}

trap cleanup EXIT

usage() {
    cat <<'EOF'
Usage: scripts/install.sh [options]

Build and install SideALSA system files.

Options:
  --prefix PATH             Binary and data prefix (default: /usr/local)
  --profile PATH            Profile seed for first install; existing config is preserved
  --socket PATH             Daemon socket (default: /tmp/sidealsad.sock)
  --alsa-plugin-dir PATH    ALSA external-plugin directory
  --no-build                Use existing target/release artifacts
  --with-asio               Build and install Wine ASIO binaries
  --force                   Replace files not owned by previous install
  --replace-profile         Replace existing device profile
  --no-start                Enable service without starting it
  --no-pipewire             Skip PipeWire adapter configuration
  -h, --help                Show this help

DESTDIR may be set for staged package installation. System services are not
changed when DESTDIR is non-empty.
EOF
}

while (($# > 0)); do
    case "$1" in
        --prefix)
            (($# >= 2)) || die "--prefix requires a path"
            PREFIX=$2
            shift 2
            ;;
        --profile)
            (($# >= 2)) || die "--profile requires a path"
            PROFILE_SOURCE=$2
            shift 2
            ;;
        --socket)
            (($# >= 2)) || die "--socket requires a path"
            SOCKET_PATH=$2
            shift 2
            ;;
        --alsa-plugin-dir)
            (($# >= 2)) || die "--alsa-plugin-dir requires a path"
            ALSA_PLUGIN_DIR=$2
            shift 2
            ;;
        --no-build)
            NO_BUILD=1
            shift
            ;;
        --with-asio)
            WITH_ASIO=1
            shift
            ;;
        --force)
            FORCE=1
            shift
            ;;
        --replace-profile)
            REPLACE_PROFILE=1
            shift
            ;;
        --no-start)
            NO_START=1
            shift
            ;;
        --no-pipewire)
            INSTALL_PIPEWIRE=0
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
[[ "$SOCKET_PATH" == /* ]] || die "socket path must be absolute"
[[ "$DESTDIR" == /* || -z "$DESTDIR" ]] || die "DESTDIR must be absolute"
[[ "$ALSA_PLUGIN_DIR" == /* || -z "$ALSA_PLUGIN_DIR" ]] || die "ALSA plugin path must be absolute"

if [[ "$DESTDIR" == "/" ]]; then
    DESTDIR=
fi
DESTDIR="${DESTDIR%/}"

if [[ "$PROFILE_SOURCE" != /* ]]; then
    PROFILE_SOURCE="$ROOT/$PROFILE_SOURCE"
fi
[[ -f "$PROFILE_SOURCE" ]] || die "profile not found: $PROFILE_SOURCE"
PROFILE_NAME="$(basename -- "$PROFILE_SOURCE")"
[[ "$PROFILE_NAME" != "." && "$PROFILE_NAME" != ".." ]] || die "invalid profile name"

USE_SUDO=0
if [[ -z "$DESTDIR" && "$EUID" -ne 0 ]]; then
    command -v sudo >/dev/null 2>&1 || die "sudo is required for system installation"
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

sed_escape() {
    printf '%s' "$1" | sed 's/[&|\\]/\\&/g'
}

if [[ -z "$ALSA_PLUGIN_DIR" ]]; then
    alsa_libdir=
    if command -v pkg-config >/dev/null 2>&1; then
        alsa_libdir="$(pkg-config --variable=libdir alsa 2>/dev/null || true)"
    fi
    if [[ -n "$alsa_libdir" ]]; then
        ALSA_PLUGIN_DIR="$alsa_libdir/alsa-lib"
    else
        for candidate in \
            "$PREFIX/lib/alsa-lib" \
            "/usr/lib/alsa-lib" \
            "/usr/lib64/alsa-lib" \
            "/usr/lib/x86_64-linux-gnu/alsa-lib" \
            "/usr/lib/aarch64-linux-gnu/alsa-lib"; do
            if [[ -d "$candidate" || -d "$(destination "$candidate")" ]]; then
                ALSA_PLUGIN_DIR=$candidate
                break
            fi
        done
    fi
    ALSA_PLUGIN_DIR="${ALSA_PLUGIN_DIR:-$PREFIX/lib/alsa-lib}"
fi

[[ "$ALSA_PLUGIN_DIR" == /* ]] || die "ALSA plugin path must be absolute"

if ((NO_BUILD == 0)); then
    info "building release artifacts"
    cargo build --release --workspace --manifest-path "$ROOT/Cargo.toml"
fi

if ((WITH_ASIO == 1)); then
    command -v cmake >/dev/null 2>&1 || die "--with-asio requires cmake"
    command -v winegcc >/dev/null 2>&1 || die "--with-asio requires winegcc"
    command -v winebuild >/dev/null 2>&1 || die "--with-asio requires winebuild"
    if ((NO_BUILD == 0)); then
        info "building Wine ASIO artifacts"
        cmake -S "$ROOT/crates/sidealsa-asio" -B "$ROOT/build-asio" -DCMAKE_BUILD_TYPE=Release
        cmake --build "$ROOT/build-asio"
    fi
fi

BINARIES=(
    sidealsad
    sidealsa-hw-test
    sidealsa-pro-test
    sidealsa-stats
    sidealsa-pro-client-test
    sidealsa-shared-test
)
for binary in "${BINARIES[@]}"; do
    [[ -x "$ROOT/target/release/$binary" ]] || die "missing release binary: $binary"
done
PLUGIN_SOURCE="$ROOT/target/release/libasound_module_pcm_sidealsa.so"
[[ -f "$PLUGIN_SOURCE" ]] || die "missing ALSA plugin: $PLUGIN_SOURCE"

ALSA_CONFIG_PATH=/etc/alsa/conf.d/99-sidealsa.conf
PIPEWIRE_CONFIG_PATH=/etc/pipewire/pipewire.conf.d/99-sidealsa.conf
PIPEWIRE_PULSE_CONFIG_PATH=/etc/pipewire/pipewire-pulse.conf.d/99-sidealsa.conf
SERVICE_PATH=/etc/systemd/system/sidealsad.service
PROFILE_PATH=/etc/sidealsa/profiles/$PROFILE_NAME
LICENSE_PATH="$PREFIX/share/sidealsa/LICENSE"
DOC_PREFIX="$PREFIX/share/doc/sidealsa"
MANIFEST_PATH="$PREFIX/share/sidealsa/install-manifest"
RETIRED_MANAGED_PATHS=(
    /etc/wireplumber/wireplumber.conf.d/99-sidealsa.conf
)

MANAGED_PATHS=()
for binary in "${BINARIES[@]}"; do
    MANAGED_PATHS+=("$PREFIX/bin/$binary")
done
MANAGED_PATHS+=(
    "$ALSA_PLUGIN_DIR/libasound_module_pcm_sidealsa.so"
    "$ALSA_CONFIG_PATH"
    "$SERVICE_PATH"
    "$LICENSE_PATH"
)
if ((INSTALL_PIPEWIRE == 1)); then
    MANAGED_PATHS+=(
        "$PIPEWIRE_CONFIG_PATH"
        "$PIPEWIRE_PULSE_CONFIG_PATH"
    )
fi
for doc in "$ROOT"/docs/*.md; do
    MANAGED_PATHS+=("$DOC_PREFIX/$(basename -- "$doc")")
done
if ((WITH_ASIO == 1)); then
    MANAGED_PATHS+=(
        "$PREFIX/lib/wine/x86_64-windows/sidealsa-asio64.dll"
        "$PREFIX/lib/wine/x86_64-unix/sidealsa-asio64.dll.so"
        "$PREFIX/lib/wine/x86_64-windows/sidealsa-asio.dll"
        "$PREFIX/lib/wine/x86_64-unix/sidealsa-asio.dll.so"
    )
    [[ -f "$ROOT/build-asio/sidealsa-asio64.dll" ]] || die "missing ASIO PE binary"
    [[ -f "$ROOT/build-asio/sidealsa-asio64.dll.so" ]] || die "missing ASIO Unix binary"
fi

MANIFEST_ACTUAL="$(destination "$MANIFEST_PATH")"
declare -A OLD_HASHES=()
if [[ -f "$MANIFEST_ACTUAL" ]]; then
    while IFS=$'\t' read -r hash path; do
        [[ -n "$path" && "$hash" != \#* ]] || continue
        OLD_HASHES["$path"]=$hash
    done < "$MANIFEST_ACTUAL"
fi

for path in "${MANAGED_PATHS[@]}"; do
    actual="$(destination "$path")"
    if [[ -e "$actual" && -z "${OLD_HASHES[$path]+owned}" && $FORCE -eq 0 ]]; then
        die "refusing to replace unmanaged file: $actual (use --force)"
    fi
    if [[ -e "$actual" && -n "${OLD_HASHES[$path]+owned}" && $FORCE -eq 0 ]]; then
        [[ "$(file_hash "$actual")" == "${OLD_HASHES[$path]}" ]] || \
            die "managed file changed since install: $actual (use --force)"
    fi
done
for path in "${RETIRED_MANAGED_PATHS[@]}"; do
    actual="$(destination "$path")"
    [[ -e "$actual" && -n "${OLD_HASHES[$path]+owned}" ]] || continue
    if ((FORCE == 1)) || [[ "$(file_hash "$actual")" == "${OLD_HASHES[$path]}" ]]; then
        run_privileged rm -f -- "$actual"
    fi
done

TMP_DIR="$(mktemp -d)"

install_managed_copy() {
    local source=$1
    local path=$2
    local mode=$3
    local temp="$TMP_DIR/$(basename -- "$path").tmp"
    {
        printf '# Managed by SideALSA installer.\n'
        cat "$source"
    } > "$temp"
    run_privileged install -D -m "$mode" "$temp" "$(destination "$path")"
}

run_privileged install -D -m 0755 "$ROOT/target/release/sidealsad" "$(destination "$PREFIX/bin/sidealsad")"
for binary in sidealsa-hw-test sidealsa-pro-test sidealsa-stats sidealsa-pro-client-test sidealsa-shared-test; do
    run_privileged install -D -m 0755 "$ROOT/target/release/$binary" "$(destination "$PREFIX/bin/$binary")"
done
run_privileged install -D -m 0755 "$PLUGIN_SOURCE" "$(destination "$ALSA_PLUGIN_DIR/libasound_module_pcm_sidealsa.so")"
PROFILE_ACTUAL="$(destination "$PROFILE_PATH")"
if [[ -e "$PROFILE_ACTUAL" && "$REPLACE_PROFILE" -eq 0 ]]; then
    info "preserving existing profile: $PROFILE_ACTUAL"
else
    run_privileged install -D -m 0644 "$PROFILE_SOURCE" "$PROFILE_ACTUAL"
    info "installed profile: $PROFILE_ACTUAL"
fi
run_privileged install -D -m 0644 "$ROOT/LICENSE" "$(destination "$LICENSE_PATH")"

for doc in "$ROOT"/docs/*.md; do
    run_privileged install -D -m 0644 "$doc" "$(destination "$DOC_PREFIX/$(basename -- "$doc")")"
done

escaped_socket="$(sed_escape "$SOCKET_PATH")"
alsa_temp="$TMP_DIR/asound.conf"
{
    printf '# Managed by SideALSA installer.\n'
    sed "s|socket \".*\"|socket \"$escaped_socket\"|g" \
        "$ROOT/configs/asound.sidealsa.conf"
} > "$alsa_temp"
run_privileged install -D -m 0644 "$alsa_temp" "$(destination "$ALSA_CONFIG_PATH")"

service_temp="$TMP_DIR/sidealsad.service"
SERVICE_GROUP_DIRECTIVE=
SOCKET_UMASK=0000
if command -v getent >/dev/null 2>&1 && getent group audio >/dev/null; then
    SERVICE_GROUP_DIRECTIVE="Group=audio"
    SOCKET_UMASK=0007
else
    warn "audio group not found; SideALSA socket will be world-accessible"
fi
sed \
    -e "s|@PREFIX@|$(sed_escape "$PREFIX")|g" \
    -e "s|@PROFILE@|$(sed_escape "$PROFILE_PATH")|g" \
    -e "s|@SOCKET@|$(sed_escape "$SOCKET_PATH")|g" \
    -e "s|@GROUP_DIRECTIVE@|$(sed_escape "$SERVICE_GROUP_DIRECTIVE")|g" \
    -e "s|@SOCKET_UMASK@|$SOCKET_UMASK|g" \
    "$ROOT/packaging/sidealsad.service.in" > "$service_temp"
run_privileged install -D -m 0644 "$service_temp" "$(destination "$SERVICE_PATH")"

if ((INSTALL_PIPEWIRE == 1)); then
    install_managed_copy \
        "$ROOT/configs/pipewire/pipewire.conf.d/sidealsa.conf" \
        "$PIPEWIRE_CONFIG_PATH" \
        0644
    install_managed_copy \
        "$ROOT/configs/pipewire/pipewire-pulse.conf.d/sidealsa.conf" \
        "$PIPEWIRE_PULSE_CONFIG_PATH" \
        0644
fi

if ((WITH_ASIO == 1)); then
    run_privileged install -D -m 0644 "$ROOT/build-asio/sidealsa-asio64.dll" \
        "$(destination "$PREFIX/lib/wine/x86_64-windows/sidealsa-asio64.dll")"
    run_privileged install -D -m 0755 "$ROOT/build-asio/sidealsa-asio64.dll.so" \
        "$(destination "$PREFIX/lib/wine/x86_64-unix/sidealsa-asio64.dll.so")"
    run_privileged ln -sfn sidealsa-asio64.dll \
        "$(destination "$PREFIX/lib/wine/x86_64-windows/sidealsa-asio.dll")"
    run_privileged ln -sfn sidealsa-asio64.dll.so \
        "$(destination "$PREFIX/lib/wine/x86_64-unix/sidealsa-asio.dll.so")"
fi

manifest_temp="$TMP_DIR/install-manifest"
{
    printf '# SideALSA install manifest v1\n'
    for path in "${MANAGED_PATHS[@]}"; do
        printf '%s\t%s\n' "$(file_hash "$(destination "$path")")" "$path"
    done
} > "$manifest_temp"
run_privileged install -D -m 0644 "$manifest_temp" "$(destination "$MANIFEST_PATH")"

if [[ -z "$DESTDIR" ]]; then
    if command -v systemctl >/dev/null 2>&1; then
        stop_user_audio
        run_privileged systemctl daemon-reload
        if ((NO_START == 1)); then
            run_privileged systemctl enable sidealsad.service
        else
            run_privileged systemctl enable sidealsad.service
            run_privileged systemctl restart sidealsad.service
            wait_for_socket
            restore_user_audio
        fi
    else
        warn "systemctl not found; start sidealsad.service manually"
    fi
    if ((INSTALL_PIPEWIRE == 1)); then
        info "start/restart user PipeWire session and PulseAudio compatibility:"
        info "  systemctl --user enable --now pipewire.socket pipewire-pulse.socket"
        info "  systemctl --user restart pipewire.service pipewire-pulse.service wireplumber.service"
    fi
fi

info "SideALSA installed"
info "profile: $PROFILE_PATH"
info "socket: $SOCKET_PATH"
info "ALSA plugin: $ALSA_PLUGIN_DIR/libasound_module_pcm_sidealsa.so"
