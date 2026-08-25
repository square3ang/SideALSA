#!/usr/bin/env bash

# don't execute this file as sudo.


set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
INSTALL_ROOT="${SIDEALSA_ASIO_INSTALL_ROOT:-$HOME/.local}"
BUILD_DIR="${SIDEALSA_ASIO_BUILD_DIR:-$ROOT/build-asio}"
WINE_BIN="${WINE:-wine}"
BUILD=1
REGISTER=1
ALL_STEAM=0
APPID=
PREFIXES=()

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

info() {
    printf '%s\n' "$*"
}

usage() {
    cat <<'EOF'
Usage: scripts/install-asio.sh [options]

Build and install SideALSA ASIO for Wine/Proton, then register it in Wine prefixes.

Options:
  --install-root PATH   Wine library install root (default: $HOME/.local)
  --build-dir PATH      CMake build directory (default: build-asio)
  --steam-prefix PATH   Register one Wine/Proton prefix (repeatable)
  --appid ID            Select Steam compatdata ID when using --all-steam
  --all-steam           Register every discovered Steam compatdata prefix
  --wine PATH           Wine executable used for regsvr32 (default: wine)
  --no-build            Reuse existing build-asio artifacts
  --no-register         Install files without prefix registration
  -h, --help            Show this help

Environment:
  SIDEALSA_SOCKET        Socket used by ASIO at runtime (default: /tmp/sidealsad.sock)
  SIDEALSA_ASIO_INSTALL_ROOT
  SIDEALSA_ASIO_BUILD_DIR
  WINE
EOF
}

while (($# > 0)); do
    case "$1" in
        --install-root)
            (($# >= 2)) || die "--install-root requires a path"
            INSTALL_ROOT=$2
            shift 2
            ;;
        --build-dir)
            (($# >= 2)) || die "--build-dir requires a path"
            BUILD_DIR=$2
            shift 2
            ;;
        --steam-prefix|--wine-prefix)
            (($# >= 2)) || die "$1 requires a path"
            PREFIXES+=("$2")
            shift 2
            ;;
        --appid)
            (($# >= 2)) || die "--appid requires an ID"
            APPID=$2
            shift 2
            ;;
        --all-steam)
            ALL_STEAM=1
            shift
            ;;
        --wine)
            (($# >= 2)) || die "--wine requires a path"
            WINE_BIN=$2
            shift 2
            ;;
        --no-build)
            BUILD=0
            shift
            ;;
        --no-register)
            REGISTER=0
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

[[ "$INSTALL_ROOT" == /* ]] || die "install root must be absolute"
[[ "$BUILD_DIR" == /* ]] || BUILD_DIR="$ROOT/$BUILD_DIR"

if ((BUILD == 1)); then
    command -v cmake >/dev/null 2>&1 || die "cmake is required"
    info "building SideALSA ASIO"
    cmake -S "$ROOT/crates/sidealsa-asio" -B "$BUILD_DIR" -DCMAKE_BUILD_TYPE=Release
    cmake --build "$BUILD_DIR"
fi

DLL_SOURCE="$BUILD_DIR/sidealsa-asio64.dll"
UNIX_SOURCE="$BUILD_DIR/sidealsa-asio64.dll.so"
[[ -f "$DLL_SOURCE" ]] || die "missing ASIO PE binary: $DLL_SOURCE"
[[ -f "$UNIX_SOURCE" ]] || die "missing ASIO Unix binary: $UNIX_SOURCE"

WINE_ROOT="$INSTALL_ROOT/lib/wine"
WINDOWS_ROOT="$WINE_ROOT/x86_64-windows"
UNIX_ROOT="$WINE_ROOT/x86_64-unix"
install -D -m 0644 "$DLL_SOURCE" "$WINDOWS_ROOT/sidealsa-asio64.dll"
install -D -m 0755 "$UNIX_SOURCE" "$UNIX_ROOT/sidealsa-asio64.dll.so"
ln -sfn sidealsa-asio64.dll "$WINDOWS_ROOT/sidealsa-asio.dll"
ln -sfn sidealsa-asio64.dll.so "$UNIX_ROOT/sidealsa-asio.dll.so"
info "installed ASIO under $WINE_ROOT"

if ((REGISTER == 0)); then
    exit 0
fi

command -v "$WINE_BIN" >/dev/null 2>&1 || die "Wine executable not found: $WINE_BIN"

if ((ALL_STEAM == 1)); then
    shopt -s nullglob
    STEAM_ROOTS=(
        "$HOME/.steam/steam"
        "$HOME/.steam/root"
        "$HOME/.local/share/Steam"
        "$HOME/.var/app/com.valvesoftware.Steam/.local/share/Steam"
        "$HOME/.var/app/com.valvesoftware.Steam/data/Steam"
    )
    for steam_root in "${STEAM_ROOTS[@]}"; do
        if [[ -n "$APPID" ]]; then
            candidate="$steam_root/steamapps/compatdata/$APPID/pfx"
            [[ -d "$candidate/drive_c/windows/system32" ]] && PREFIXES+=("$candidate")
        else
            for candidate in "$steam_root"/steamapps/compatdata/*/pfx; do
                [[ -d "$candidate/drive_c/windows/system32" ]] && PREFIXES+=("$candidate")
            done
        fi
    done
    shopt -u nullglob
fi

if ((${#PREFIXES[@]} == 0)); then
    info "no Wine prefixes selected"
    info "register later with --steam-prefix PATH"
    exit 0
fi

UNIQUE_PREFIXES=()
for prefix in "${PREFIXES[@]}"; do
    [[ -d "$prefix/drive_c/windows/system32" ]] || die "invalid Wine prefix: $prefix"
    canonical=$(readlink -f "$prefix")
    duplicate=0
    for existing in "${UNIQUE_PREFIXES[@]}"; do
        [[ "$existing" == "$canonical" ]] && duplicate=1
    done
    ((duplicate == 0)) && UNIQUE_PREFIXES+=("$canonical")
done

for prefix in "${UNIQUE_PREFIXES[@]}"; do
    info "registering SideALSA ASIO in $prefix"
    install -D -m 0644 "$DLL_SOURCE" \
        "$prefix/drive_c/windows/system32/sidealsa-asio64.dll"
    env \
        WINEPREFIX="$prefix" \
        WINEDLLPATH="$WINE_ROOT${WINEDLLPATH:+:$WINEDLLPATH}" \
        "$WINE_BIN" regsvr32 /s sidealsa-asio64.dll
done

info "registered SideALSA ASIO in ${#UNIQUE_PREFIXES[@]} prefix(es)"
info "Steam launch option: SIDEALSA_SOCKET=${SIDEALSA_SOCKET:-/tmp/sidealsad.sock} WINEDLLPATH=$WINE_ROOT %command%"
