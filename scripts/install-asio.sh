#!/usr/bin/env bash

# don't execute this file as sudo.


set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
if [[ "${1:-}" == --interactive ]]; then
    shift
    exec bash "$ROOT/scripts/setup-asio.sh" "$@"
elif (($# == 0)); then
    exec bash "$ROOT/scripts/setup-asio.sh"
fi
INSTALL_ROOT="${SIDEALSA_ASIO_INSTALL_ROOT:-$HOME/.local}"
BUILD_DIR="${SIDEALSA_ASIO_BUILD_DIR:-$ROOT/build-asio}"
WINE_BIN="${WINE:-wine}"
# An explicitly chosen wine binary means the user accepts responsibility for
# foreign-owned prefixes (pipeasio-register semantics). The TUI only forwards
# --wine when WINE is set, so a default lookup stays guarded.
WINE_EXPLICIT=0
[[ -z "${WINE:-}" ]] || WINE_EXPLICIT=1
BUILD=1
REGISTER=1
ALL_STEAM=0
APPID=
PREFIXES=()
declare -A BOTTLE_NAMES=()
LAST_PREFIX=

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

info() {
    printf '%s\n' "$*"
}

# Host wine's first process in a prefix owned by another Wine build migrates
# the prefix to the host build. Proton prefixes (tracked_files) register
# through umu-run instead of system wine; Bottles bottles (bottle.yml) are
# refused with manual instructions unless the wine binary was chosen
# explicitly. Returns: proton, bottles, or wine.
prefix_owner() {
    local prefix=$1
    if [[ -e "$prefix/tracked_files" || -e "$prefix/../tracked_files" ]]; then
        printf 'proton'
    elif [[ -e "$prefix/bottle.yml" ]]; then
        printf 'bottles'
    else
        printf 'wine'
    fi
}

# Steam appid owning a compatdata prefix, for umu GAMEID selection.
proton_appid() {
    local parent
    parent=$(basename -- "$(dirname -- "$1")")
    if [[ "$parent" =~ ^[0-9]+$ ]]; then
        printf 'umu-%s' "$parent"
    else
        printf 'umu-sidealsa-asio'
    fi
}

install_atomic() {
    local source=$1 target=$2 mode=$3 temporary
    mkdir -p -- "$(dirname -- "$target")"
    if [[ -f "$target" && ! -L "$target" ]] && cmp -s -- "$source" "$target" \
        && [[ "$(stat -c '%a' -- "$target")" == "${mode#0}" ]]; then
        return
    fi
    # Never truncate a library that a running Wine process may have mapped.
    temporary=$(mktemp "${target}.new.XXXXXX")
    if ! install -m "$mode" -- "$source" "$temporary" || ! mv -Tf -- "$temporary" "$target"; then
        rm -f -- "$temporary"
        die "could not atomically install $target"
    fi
}

usage() {
    cat <<'EOF'
Usage: scripts/install-asio.sh [options]

Build and install SideALSA ASIO for Wine/Proton, then register it in Wine prefixes.

Options:
  --interactive         Guided setup (use alone; also default with no args)
  --install-root PATH   Wine library install root (default: $HOME/.local)
  --build-dir PATH      CMake build directory (default: build-asio)
  --steam-prefix PATH   Register one Wine/Proton prefix (repeatable)
  --bottle-name NAME    Bottles name for the preceding --steam-prefix; registers
                        through bottles-cli when available, otherwise the DLL is
                        staged and manual regsvr32 instructions are printed
  --appid ID            Select Steam compatdata ID when using --all-steam
  --all-steam           Register every discovered Steam compatdata prefix
  --wine PATH           Wine executable used for regsvr32 (default: wine)
  --no-build            Reuse existing build-asio artifacts
  --no-register         Install files without prefix registration
  -h, --help            Show this help
Proton-managed prefixes (tracked_files) register through umu-run with
GAMEID=umu-<appid>, never through system wine. In Bottles bottles (bottle.yml)
the DLL is staged automatically but host wine is not started: run
regsvr32 /s sidealsa-asio64.dll inside the bottle yourself. Passing --wine or
setting WINE explicitly bypasses both rules and counts as accepting
responsibility.

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
            LAST_PREFIX="$2"
            shift 2
            ;;
        --bottle-name)
            (($# >= 2)) || die "--bottle-name requires a name"
            [[ -n "$LAST_PREFIX" ]] || die "--bottle-name needs a preceding --steam-prefix"
            BOTTLE_NAMES["$LAST_PREFIX"]="$2"
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
            WINE_EXPLICIT=1
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
install_atomic "$DLL_SOURCE" "$WINDOWS_ROOT/sidealsa-asio64.dll" 0644
install_atomic "$UNIX_SOURCE" "$UNIX_ROOT/sidealsa-asio64.dll.so" 0755
ln -sfn sidealsa-asio64.dll "$WINDOWS_ROOT/sidealsa-asio.dll"
ln -sfn sidealsa-asio64.dll.so "$UNIX_ROOT/sidealsa-asio.dll.so"
info "installed ASIO under $WINE_ROOT"

if ((REGISTER == 0)); then
    exit 0
fi

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
NEED_UMU=0
NEED_WINE=0
for prefix in "${PREFIXES[@]}"; do
    [[ -d "$prefix/drive_c/windows/system32" ]] || die "invalid Wine prefix: $prefix"
    canonical=$(readlink -f "$prefix")
    # Bottle names are given alongside raw paths; re-key them by canonical path.
    if [[ -n "${BOTTLE_NAMES[$prefix]:-}" ]]; then
        BOTTLE_NAMES["$canonical"]="${BOTTLE_NAMES[$prefix]}"
    fi
    owner=$(prefix_owner "$canonical")
    if [[ "$owner" == proton && "$WINE_EXPLICIT" -eq 0 ]]; then
        NEED_UMU=1
    elif [[ "$owner" == bottles && "$WINE_EXPLICIT" -eq 0 ]]; then
        : # DLL is staged below; regsvr32 stays manual, no wine binary needed.
    else
        NEED_WINE=1
    fi
    duplicate=0
    for existing in "${UNIQUE_PREFIXES[@]}"; do
        [[ "$existing" == "$canonical" ]] && duplicate=1
    done
    ((duplicate == 0)) && UNIQUE_PREFIXES+=("$canonical")
done

if ((NEED_WINE == 1)); then
    command -v "$WINE_BIN" >/dev/null 2>&1 || die "Wine executable not found: $WINE_BIN"
fi
if ((NEED_UMU == 1)); then
    command -v umu-run >/dev/null 2>&1 || die "umu-run not found: install umu to register Proton prefixes (or pass --wine explicitly to accept responsibility)"
fi

BOTTLES_STAGED=()
for prefix in "${UNIQUE_PREFIXES[@]}"; do
    info "registering SideALSA ASIO in $prefix"
    install_atomic "$DLL_SOURCE" \
        "$prefix/drive_c/windows/system32/sidealsa-asio64.dll" 0644
    if [[ "$(prefix_owner "$prefix")" == bottles && "$WINE_EXPLICIT" -eq 0 ]]; then
        bottle_name="${BOTTLE_NAMES[$prefix]:-}"
        if [[ -n "$bottle_name" ]] && command -v bottles-cli >/dev/null 2>&1; then
            info "Bottles bottle '$bottle_name': registering through bottles-cli instead of system wine"
            # Bottles limits the system environment, so an exported WINEDLLPATH
            # never reaches its wine. Persist it in the bottle instead; the
            # Unix half stays in the install root and game launches need it too.
            bottles-cli edit -b "$bottle_name" --env-var "WINEDLLPATH=$WINE_ROOT" \
                || die "bottles-cli edit failed for '$bottle_name'; set WINEDLLPATH=$WINE_ROOT in the bottle environment yourself"
            bottles-cli shell -b "$bottle_name" -i "regsvr32 /s sidealsa-asio64.dll" \
                || die "bottles-cli regsvr32 failed in '$bottle_name'"
        else
            info "Bottles bottle: DLL staged, host wine not started."
            BOTTLES_STAGED+=("$prefix")
        fi
    elif [[ "$(prefix_owner "$prefix")" == proton && "$WINE_EXPLICIT" -eq 0 ]]; then
        info "Proton prefix: registering through umu-run instead of system wine"
        env \
            WINEPREFIX="$prefix" \
            GAMEID="${GAMEID:-$(proton_appid "$prefix")}" \
            WINEDLLPATH="$WINE_ROOT${WINEDLLPATH:+:$WINEDLLPATH}" \
            umu-run regsvr32 /s sidealsa-asio64.dll \
            || die "umu-run regsvr32 failed in $prefix"
    else
        env \
            WINEPREFIX="$prefix" \
            WINEDLLPATH="$WINE_ROOT${WINEDLLPATH:+:$WINEDLLPATH}" \
            "$WINE_BIN" regsvr32 /s sidealsa-asio64.dll \
            || die "regsvr32 failed in $prefix"
    fi
done

info "registered SideALSA ASIO in $((${#UNIQUE_PREFIXES[@]} - ${#BOTTLES_STAGED[@]})) prefix(es)"
if ((${#BOTTLES_STAGED[@]} > 0)); then
    info "ACTION REQUIRED for ${#BOTTLES_STAGED[@]} Bottles bottle(s): the DLL was staged, but host wine was not started."
    for prefix in "${BOTTLES_STAGED[@]}"; do
        info "  - $prefix"
    done
    info "Inside each bottle, first persist the library path, then register:"
    info "  bottles-cli edit -b <bottle> --env-var WINEDLLPATH=$WINE_ROOT"
    info '  bottles-cli shell -b <bottle> -i "regsvr32 /s sidealsa-asio64.dll"'
fi
info "Steam launch option: WINEDLLPATH=$WINE_ROOT %command%"
info "ASIO uses /tmp/sidealsad.sock unless SIDEALSA_SOCKET is set"
