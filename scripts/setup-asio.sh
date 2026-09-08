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

args=(--install-root "$install_root" --build-dir "$build_dir" --wine "$wine_bin")
((build)) || args+=(--no-build)
if ((register)); then
    for prefix in "${selected[@]}"; do args+=(--steam-prefix "$prefix"); done
else
    args+=(--no-register)
fi
section 'Summary'
row 'Install root' "$(printf '%q' "$install_root")"
row 'Build dir' "$(printf '%q' "$build_dir")"
if ((build)); then row 'Build' 'yes'; else row 'Build' 'reuse existing artifacts'; fi
row 'Files' 'install (atomic library replacement)'
if ((register)); then
    row 'Registration' "$(printf 'yes; Wine %q (may start Wine)' "$wine_bin")"
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
