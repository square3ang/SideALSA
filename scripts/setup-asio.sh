#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cancel() { printf 'Cancelled; no changes made.\n'; exit 0; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }
ask() {
    printf '%s' "$1"
    IFS= read -r answer || cancel
}

# Presentation only: color is enabled for terminal output and disabled for
# pipes, NO_COLOR, CLICOLOR=0, or dumb terminals. Wrapping never splits the
# inner text, so fixed-string assertions keep matching either way.
if [[ -t 1 && -z "${NO_COLOR:-}" && "${CLICOLOR:-1}" != 0 && "${TERM:-}" != dumb ]]; then
    B=$'\e[1m'; DIM=$'\e[2m'; GREEN=$'\e[32m'; RESET=$'\e[0m'
else
    B=''; DIM=''; GREEN=''; RESET=''
fi
RULE='──────────────────────────────────────────────────────────────'
section() { printf '\n%s%s%s\n%s%s%s\n' "$B" "$1" "$RESET" "$DIM" "$RULE" "$RESET"; }
row() { printf '  %-17s %s\n' "$1" "$2"; }
hint() { printf '%s%s%s\n' "$DIM" "$1" "$RESET"; }
choice_menu() {
    local title=$1; shift
    section "$title"
    printf '  1. %s\n  2. %s\n  0. Cancel\n' "$1" "$2"
}

(($# == 0)) || fail 'Use --interactive alone; configure options in the menu.'
((EUID != 0)) || fail 'Run interactive setup as your normal desktop user, not root/sudo.'

install_root=${SIDEALSA_ASIO_INSTALL_ROOT:-$HOME/.local}
build_dir=${SIDEALSA_ASIO_BUILD_DIR:-$ROOT/build-asio}
wine_bin=${WINE:-wine}
# Only an explicitly set WINE counts as consent to touch prefixes owned by
# another Wine build. A default lookup stays guarded (see prefix_owner).
wine_explicit=0
[[ -z "${WINE:-}" ]] || wine_explicit=1
printf '%sSideALSA ASIO setup%s\n' "$B" "$RESET"
hint 'No changes occur before final confirmation. Enter keeps defaults; 0/EOF cancels.'
section 'Paths'
printf 'Install root default: %q\n' "$install_root"
ask 'Install root (absolute path, Enter keeps default): '
[[ -z "$answer" ]] || install_root=$answer
[[ "$install_root" == /* ]] || cancel
printf 'Build directory default: %q\n' "$build_dir"
ask 'Build directory (Enter keeps default; relative paths use repository root): '
[[ -z "$answer" ]] || build_dir=$answer
[[ "$build_dir" == /* ]] || build_dir="$ROOT/$build_dir"
choice_menu 'Build artifacts' 'Build' 'Reuse existing artifacts'
ask 'Choice: '
case "$answer" in
    1) build=1 ;;
    2) build=0 ;;
    *) cancel ;;
esac
choice_menu 'Registration (can start Wine)' 'Install files only' \
    'Install files, then register selected prefixes'
ask 'Choice: '
case "$answer" in
    1) register=0 ;;
    2) register=1 ;;
    *) cancel ;;
esac

steam_ids=()
steam_names=()
steam_paths=()
manual_paths=()
seen=()
remembered() {
    local path=$1 existing
    for existing in "${seen[@]}"; do
        [[ "$existing" != "$path" ]] || return 0
    done
    return 1
}
add_manual() {
    local path=$1 canonical
    [[ -d "$path/drive_c/windows/system32" ]] || return 0
    canonical=$(readlink -f -- "$path") || return 0
    remembered "$canonical" && return 0
    seen+=("$canonical")
    manual_paths+=("$canonical")
}
# Game name from a Steam appmanifest; filesystem metadata only, never executed.
parse_acf_name() {
    local file=$1 line
    [[ -f "$file" ]] || return 0
    line=$(grep -m1 -E '"name"[[:space:]]*"' "$file" 2>/dev/null) || return 0
    line=${line#*'"name"'}
    printf '%s' "$line" | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//'
}
# Extra library folders from libraryfolders.vdf, one path per line.
library_paths() {
    local file=$1
    [[ -f "$file" ]] || return 0
    grep -E '"path"[[:space:]]*"' "$file" 2>/dev/null \
        | sed -e 's/^[^"]*"path"[[:space:]]*"//' -e 's/".*$//' || true
}
add_steam_library() {
    local library=$1 pfx canonical appid name
    for pfx in "$library"/steamapps/compatdata/*/pfx; do
        [[ -d "$pfx/drive_c/windows/system32" ]] || continue
        canonical=$(readlink -f -- "$pfx") || continue
        remembered "$canonical" && continue
        appid=$(basename -- "$(dirname -- "$pfx")")
        name=$(parse_acf_name "$library/steamapps/appmanifest_$appid.acf")
        [[ -n "$name" ]] || name="Unknown Steam game"
        seen+=("$canonical")
        steam_ids+=("$appid")
        steam_names+=("$name")
        steam_paths+=("$canonical")
    done
}
# Manual entries accept a prefix itself, a Steam library root, or a compatdata directory.
discover_manual_path() {
    local location=$1 path
    add_manual "$location"
    for path in "$location"/steamapps/compatdata/*/pfx "$location"/*/pfx; do
        add_manual "$path"
    done
}
# Numbered selection into an index array. Blank skips the step; 0/EOF cancels.
pick() {
    local prompt=$1 count=$2
    local -n picked=$3
    local choices=() choice i found
    picked=()
    ask "$prompt"
    [[ -n "$answer" ]] || return 0
    if [[ "$answer" == all ]]; then
        for ((i = 0; i < count; i++)); do picked+=("$i"); done
        return 0
    fi
    read -r -a choices <<< "$answer"
    ((${#choices[@]} > 0)) || cancel
    for choice in "${choices[@]}"; do
        # Match displayed numbers literally; never evaluate user arithmetic.
        found=0
        for ((i = 0; i < count; i++)); do
            if [[ "$choice" == "$((i + 1))" ]]; then
                picked+=("$i")
                found=1
                break
            fi
        done
        ((found)) || cancel
    done
}

if ((register)); then
    shopt -s nullglob
    steam_roots=(
        "$HOME/.steam/steam"
        "$HOME/.steam/root"
        "$HOME/.steam/debian-installation"
        "$HOME/.local/share/Steam"
        "${XDG_DATA_HOME:-$HOME/.local/share}/Steam"
        "$HOME/.var/app/com.valvesoftware.Steam/.local/share/Steam"
        "$HOME/.var/app/com.valvesoftware.Steam/data/Steam"
        "$HOME/.var/app/com.valvesoftware.Steam/.steam/steam"
        "$HOME/.var/app/com.valvesoftware.Steam/.steam/root"
    )
    for location in "${steam_roots[@]}"; do
        add_steam_library "$location"
        while IFS= read -r library; do
            [[ -n "$library" ]] || continue
            add_steam_library "$library"
        done < <(library_paths "$location/steamapps/libraryfolders.vdf")
    done
    add_manual "$HOME/.wine"
    [[ -z "${WINEPREFIX:-}" ]] || add_manual "$WINEPREFIX"

    steam_pick=()
    if ((${#steam_paths[@]} > 0)); then
        section 'Step 1 - Steam games (read-only discovery)'
        for i in "${!steam_paths[@]}"; do
            printf '  %2d. %s (appid %s)\n' "$((i + 1))" "${steam_names[i]}" "${steam_ids[i]}"
            printf '       %s%q%s\n' "$DIM" "${steam_paths[i]}" "$RESET"
        done
        pick 'Select game numbers separated by spaces, "all", Enter to skip, or 0 to cancel: ' \
            "${#steam_paths[@]}" steam_pick
    else
        section 'Step 1 - Steam games'
        printf 'none discovered.\n'
    fi

    section 'Step 2 - Manual Wine prefixes'
    hint 'One path per line: a prefix, a Steam library root, or a compatdata directory.'
    while true; do
        ask 'Additional prefix path (Enter finishes discovery): '
        [[ -n "$answer" ]] || break
        [[ -d "$answer" ]] || cancel
        discover_manual_path "$answer"
    done
    manual_pick=()
    if ((${#manual_paths[@]} > 0)); then
        printf 'Manual prefixes (read-only; aliases deduplicated):\n'
        for i in "${!manual_paths[@]}"; do
            printf '  %2d. %q\n' "$((i + 1))" "${manual_paths[i]}"
        done
        pick 'Select prefix numbers separated by spaces, "all", Enter to skip, or 0 to cancel: ' \
            "${#manual_paths[@]}" manual_pick
    else
        printf 'Manual prefixes: none discovered.\n'
    fi

    selected=()
    for i in "${steam_pick[@]}"; do selected+=("${steam_paths[i]}"); done
    for i in "${manual_pick[@]}"; do selected+=("${manual_paths[i]}"); done
    if ((${#selected[@]} == 0)); then
        printf 'No Wine prefixes selected.\n'
        cancel
    fi
fi

# Forward --wine only when explicitly set: the installer treats an explicit
# binary as consent to touch foreign-owned prefixes.
args=(--install-root "$install_root" --build-dir "$build_dir")
((wine_explicit)) && args+=(--wine "$wine_bin")
((build)) || args+=(--no-build)
declare -A bottle_for=()
if ((register)); then
    for prefix in "${selected[@]}"; do
        args+=(--steam-prefix "$prefix")
        # Bottles bottles need their bottle name for bottles-cli registration;
        # blank keeps DLL staging with a manual regsvr32 step.
        if [[ -e "$prefix/bottle.yml" && "$wine_explicit" -eq 0 ]]; then
            ask "Bottle name for $prefix (Enter: stage DLL only, run regsvr32 yourself): "
            if [[ -n "$answer" ]]; then
                args+=(--bottle-name "$answer")
                bottle_for["$prefix"]="$answer"
            fi
        fi
    done
else
    args+=(--no-register)
fi
proton_selected=0
bottles_selected=0
if ((register)) && ((wine_explicit == 0)); then
    for prefix in "${selected[@]}"; do
        if [[ -e "$prefix/tracked_files" || -e "$prefix/../tracked_files" ]]; then
            proton_selected=1
        elif [[ -e "$prefix/bottle.yml" ]]; then
            bottles_selected=1
        fi
    done
fi
section 'Summary'
row 'Install root' "$(printf '%q' "$install_root")"
row 'Build dir' "$(printf '%q' "$build_dir")"
if ((build)); then row 'Build' 'yes'; else row 'Build' 'reuse existing artifacts'; fi
row 'Files' 'install (atomic library replacement)'
if ((register)); then
    row 'Registration' "$(printf 'yes; Wine %q (may start Wine)' "$wine_bin")"
    if ((proton_selected)); then
        row 'Proton' 'selected Proton prefixes register through umu-run, not system wine'
        if ! command -v umu-run >/dev/null 2>&1; then
            row 'Warning' 'umu-run not found; install umu or registration will fail'
        fi
    fi
    if ((bottles_selected)); then
        named_bottles=()
        unnamed_bottles=()
        for prefix in "${selected[@]}"; do
            [[ -e "$prefix/bottle.yml" ]] || continue
            if [[ -n "${bottle_for[$prefix]:-}" ]]; then
                named_bottles+=("$prefix")
            else
                unnamed_bottles+=("$prefix")
            fi
        done
        for prefix in "${named_bottles[@]}"; do
            row 'Bottles' "registers through bottles-cli as ${bottle_for[$prefix]}"
            printf '      %s%q%s\n' "$DIM" "$prefix" "$RESET"
        done
        if ((${#unnamed_bottles[@]} > 0)); then
            row 'Bottles' 'DLL is staged automatically; run regsvr32 yourself inside Bottles'
            for prefix in "${unnamed_bottles[@]}"; do
                printf '      %s%q%s\n' "$DIM" "$prefix" "$RESET"
            done
        fi
    fi
    if ((${#steam_pick[@]} > 0)); then
        printf '  Steam games:\n'
        for i in "${steam_pick[@]}"; do
            printf '    - %s (appid %s)\n' "${steam_names[i]}" "${steam_ids[i]}"
            printf '      %s%q%s\n' "$DIM" "${steam_paths[i]}" "$RESET"
        done
    fi
    if ((${#manual_pick[@]} > 0)); then
        printf '  Manual prefixes:\n'
        for i in "${manual_pick[@]}"; do printf '    - %q\n' "${manual_paths[i]}"; done
    fi
else
    row 'Registration' 'none'
fi
ask 'Proceed with build/install/register? [y/N] (y: yes, Enter: no): '
case "${answer,,}" in
    y|yes) ;;
    *) cancel ;;
esac
# Explicit options prevent the no-argument TTY entry point from recursing.
exec bash "$ROOT/scripts/install-asio.sh" "${args[@]}"
