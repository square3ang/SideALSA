#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
((EUID != 0)) || { printf 'Run fixture tests as a non-root user.\n' >&2; exit 1; }
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
mkdir -p "$tmp/repo/scripts" "$tmp/home" "$tmp/bin"
cp -- "$ROOT/scripts/setup-asio.sh" "$tmp/repo/scripts/setup-asio.sh"
cp -- "$ROOT/scripts/install-asio.sh" "$tmp/repo/scripts/entry.sh"
# The entry point is real; only the helper's final installer target is stubbed.
printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\0" "$@" > "$CALL_LOG"' \
    > "$tmp/repo/scripts/install-asio.sh"
printf '%s\n' '#!/usr/bin/env bash' 'printf "unexpected command\n" >> "$FORBIDDEN_LOG"' 'exit 99' \
    > "$tmp/bin/forbidden"
chmod +x "$tmp/bin/forbidden"
for command in wine winegcc winebuild cargo cmake install touch sudo systemctl; do ln -s forbidden "$tmp/bin/$command"; done

home="$tmp/home"
roots=(
    "$home/.steam/steam"
    "$home/.steam/root"
    "$home/.steam/debian-installation"
    "$home/.local/share/Steam"
    "$home/xdg data/Steam"
    "$home/.var/app/com.valvesoftware.Steam/.local/share/Steam"
    "$home/.var/app/com.valvesoftware.Steam/data/Steam"
    "$home/.var/app/com.valvesoftware.Steam/.steam/steam"
    "$home/.var/app/com.valvesoftware.Steam/.steam/root"
)
manifest() {
    printf '"AppState"\n{\n\t"appid"\t\t"%s"\n\t"name"\t\t"%s"\n}\n' "$1" "$2" > "$3"
}
steam_ids=()
steam_names=()
steam_paths=()
for i in "${!roots[@]}"; do
    id=$((i + 10))
    pfx="${roots[i]}/steamapps/compatdata/$id/pfx"
    mkdir -p "$pfx/drive_c/windows/system32"
    # Appid 12 deliberately has no manifest: unknown-name fallback.
    if ((id != 12)); then
        manifest "$id" "Test Game $id" "${roots[i]}/steamapps/appmanifest_$id.acf"
    fi
    steam_ids+=("$id")
    steam_names+=("Test Game $id")
    steam_paths+=("$pfx")
done
# Appid 12 has no manifest; fix its expected display name.
steam_names[2]="Unknown Steam game"
# External library discovered via libraryfolders.vdf, right after its parent root.
external="$home/external library/steamapps/compatdata/42/pfx"
mkdir -p "$external/drive_c/windows/system32"
manifest 42 "External Game" "$home/external library/steamapps/appmanifest_42.acf"
{
    printf '"libraryfolders"\n{\n\t"0"\n\t{\n\t\t"path"\t\t"%s"\n\t}\n}\n' \
        "$home/external library"
} > "$home/.local/share/Steam/steamapps/libraryfolders.vdf"
steam_ids=(10 11 12 13 42 14 15 16 17 18 19)
steam_names=("Test Game 10" "Test Game 11" "Unknown Steam game" "Test Game 13" \
    "External Game" "Test Game 14" "Test Game 15" "Test Game 16" "Test Game 17" "Test Game 18" \
    "Proton Game 19")
# Discovery order: roots in order, external library right after its parent root.
steam_paths=(
    "${roots[0]}/steamapps/compatdata/10/pfx"
    "${roots[1]}/steamapps/compatdata/11/pfx"
    "${roots[2]}/steamapps/compatdata/12/pfx"
    "${roots[3]}/steamapps/compatdata/13/pfx"
    "$external"
    "${roots[4]}/steamapps/compatdata/14/pfx"
    "${roots[5]}/steamapps/compatdata/15/pfx"
    "${roots[6]}/steamapps/compatdata/16/pfx"
    "${roots[7]}/steamapps/compatdata/17/pfx"
    "${roots[8]}/steamapps/compatdata/18/pfx"
)
manual=("$home/.wine" "$home/custom prefix")
for prefix in "${manual[@]}"; do mkdir -p "$prefix/drive_c/windows/system32"; done
ln -s "$home/.wine" "$home/prefix alias"
mkdir -p "$home/.local/share/Steam/steamapps/compatdata/invalid/pfx"
# Proton-owned prefix (tracked_files): included, registers through umu-run.
proton="${roots[8]}/steamapps/compatdata/19/pfx"
mkdir -p "$proton/drive_c/windows/system32"
touch "$proton/tracked_files"
manifest 19 "Proton Game 19" "${roots[8]}/steamapps/appmanifest_19.acf"
# Bottles bottle entered manually below: excluded unless WINE is set explicitly.
bottle="$home/bottles/game"
mkdir -p "$bottle/drive_c/windows/system32"
touch "$bottle/bottle.yml"
steam_paths+=("$proton")
cp -a -- "$home" "$tmp/before"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
run() {
    local input=$1
    shift
    rm -f -- "$tmp/calls" "$tmp/forbidden"
    printf '%s' "$input" | env -i PATH="$tmp/bin:$PATH" HOME="$home" \
        XDG_DATA_HOME="$home/xdg data" WINEPREFIX="$home/custom prefix" \
        CALL_LOG="$tmp/calls" FORBIDDEN_LOG="$tmp/forbidden" \
        bash "$tmp/repo/scripts/entry.sh" "$@" > "$tmp/output"
    [[ ! -e "$tmp/forbidden" ]] || fail 'build/install/Wine command executed'
    diff -r -- "$tmp/before" "$home" || fail 'prefix or HOME changed'
}
cancelled() {
    run "$1" --interactive
    [[ ! -e "$tmp/calls" ]] || fail 'installer invoked on cancellation'
}

# No arguments dispatch to the real wizard even with redirected input/output.
run ''
[[ ! -e "$tmp/calls" ]] || fail 'no-argument EOF invoked installer'
grep -Fq 'Cancelled' "$tmp/output" || fail 'no-argument EOF did not reach wizard'
run $'\n\n2\n1\ny\n'
[[ -e "$tmp/calls" ]] || fail 'no-argument redirected confirmation did not reach stub installer'

# EOF at each prompt, 0/invalid menu input, skipped steps, and final default cancellation.
for input in '' $'\n' $'\n\n' $'\n\n2\n' $'\n\n2\n2\n' \
    $'\n\n2\n2\n0\n' $'\n\n2\n2\n999\n' $'\n\n2\n2\n01\n' \
    $'\n\n2\n2\n1; touch bad\n' $'\n\n2\n2\n\n' \
    $'\n\n2\n2\n\n'"$tmp/missing"$'\n' $'\n\n2\n2\n\n\n0\n' \
    $'\n\n2\n2\n\n\n99\n' $'\n\n2\n2\n\n\n\n' \
    $'\n\n2\n2\nall\n\n\nno\n' $'\n\n2\n2\nall\n\nall\n\n' $'relative\n'; do
    cancelled "$input"
done
for selection in 0 999 -1 01 '1 999' '1; touch bad' 'a[$(touch bad)]' ''; do
    cancelled $'\n\n2\n2\n'"$selection"$'\nINSTALL\n'
done
for selection in 0 99 -1 '1 99' 'allx'; do
    cancelled $'\n\n2\n2\n\n\n'"$selection"$'\nINSTALL\n'
done

run "$tmp/install root"$'\nrelative build\n2\n1\ny\n' --interactive
mapfile -d '' -t args < "$tmp/calls"
expected_args=(--install-root "$tmp/install root" --build-dir "$tmp/repo/relative build" --no-build --no-register)
[[ ${#args[@]} == ${#expected_args[@]} ]] || fail 'file-only argument count'
for i in "${!expected_args[@]}"; do
    [[ "${args[i]}" == "${expected_args[i]}" ]] || fail "file-only argument $i"
done
[[ ! -e "$tmp/install root" ]] || fail 'real installation occurred'

# All Steam games plus all manual prefixes, with game names displayed.
# The Proton-owned game is included: it registers through umu-run.
run $'\n\n1\n2\nall\n\nall\ny\n' --interactive
mapfile -d '' -t args < "$tmp/calls"
expected_all=("${steam_paths[@]}" "${manual[@]}")
[[ ${#args[@]} == $((4 + 2 * ${#expected_all[@]})) ]] || fail 'all prefix count or build flag'
for i in "${!expected_all[@]}"; do
    [[ "${args[4 + 2*i]}" == --steam-prefix && "${args[5 + 2*i]}" == "${expected_all[i]}" ]] \
        || fail "prefix $i missing or duplicated"
done
for name in "Test Game 10" "Unknown Steam game" "External Game" "Proton Game 19"; do
    grep -Fq "$name" "$tmp/output" || fail "game name not displayed: $name"
done
grep -Fq 'umu-run' "$tmp/output" || fail 'proton registration note missing'
grep -Fq 'Step 1 - Steam games' "$tmp/output" || fail 'steam step missing'
grep -Fq 'Step 2 - Manual Wine prefixes' "$tmp/output" || fail 'manual step missing'

# Explicit WINE is forwarded and still covers the Proton-owned game.
rm -f -- "$tmp/calls" "$tmp/forbidden"
printf '%s' $'\n\n1\n2\nall\n\n\ny\n' | env -i PATH="$tmp/bin:$PATH" HOME="$home" \
    XDG_DATA_HOME="$home/xdg data" WINEPREFIX="$home/custom prefix" WINE=custom-wine \
    CALL_LOG="$tmp/calls" FORBIDDEN_LOG="$tmp/forbidden" \
    bash "$tmp/repo/scripts/entry.sh" --interactive > "$tmp/output"
[[ ! -e "$tmp/forbidden" ]] || fail 'build/install/Wine command executed'
mapfile -d '' -t args < "$tmp/calls"
wine_forwarded=0
proton_selected=0
for arg in "${args[@]}"; do
    [[ "$arg" == --wine ]] && wine_forwarded=1
    [[ "$arg" == "$proton" ]] && proton_selected=1
done
# --wine custom-wine must immediately precede its value; check adjacency too.
for ((i = 0; i + 1 < ${#args[@]}; i++)); do
    if [[ "${args[i]}" == --wine && "${args[i+1]}" == custom-wine ]]; then
        wine_forwarded=2
    fi
done
((wine_forwarded == 2)) || fail 'explicit wine not forwarded'
((proton_selected)) || fail 'proton prefix missing with explicit WINE'

# Split selection keeps game order; skipped manual step leaves no manual section.
run $'\n\n2\n2\n2 1\n\n\nY\n' --interactive
mapfile -d '' -t args < "$tmp/calls"
[[ ${#args[@]} == 9 && "${args[4]}" == --no-build ]] || fail 'split selection flags'
[[ "${args[5]}" == --steam-prefix && "${args[6]}" == "${steam_paths[1]}" ]] || fail 'split first game'
[[ "${args[7]}" == --steam-prefix && "${args[8]}" == "${steam_paths[0]}" ]] || fail 'split second game'
if grep -Fq 'Manual prefixes:' "$tmp/output"; then fail 'skipped manual step listed'; fi

# Manual-only: Steam skipped, alias and already-known compatdata deduplicated.
# The Bottles bottle is included with its bottle name for bottles-cli.
run $'\n\n2\n2\n\n'"$home/external library/steamapps/compatdata"$'\n'"$home/prefix alias"$'\n'"$bottle"$'\n\nall\nMyBottle\nYES\n' --interactive
mapfile -d '' -t args < "$tmp/calls"
[[ ${#args[@]} == 13 ]] || fail 'manual-only argument count'
[[ "${args[6]}" == "${manual[0]}" && "${args[8]}" == "${manual[1]}" && "${args[10]}" == "$bottle" ]] || fail 'manual-only prefixes'
[[ "${args[11]}" == --bottle-name && "${args[12]}" == MyBottle ]] || fail 'bottle name not forwarded'
grep -Fq 'MyBottle' "$tmp/output" || fail 'bottle name not summarized'
grep -Fq 'bottles-cli' "$tmp/output" || fail 'bottles auto-registration not summarized'

# Bottles bottle without a name: DLL staged, manual regsvr32 summarized.
run $'\n\n2\n2\n\n'"$bottle"$'\n\nall\n\nYES\n' --interactive
mapfile -d '' -t args < "$tmp/calls"
[[ ${#args[@]} == 11 ]] || fail 'nameless bottle argument count'
for arg in "${args[@]}"; do [[ "$arg" == --bottle-name ]] && fail 'unexpected bottle name forwarded'; done
grep -Fq 'run regsvr32 yourself' "$tmp/output" || fail 'manual regsvr32 note missing'

# Empty discovery cancels even if confirmation text follows.
mkdir -p "$tmp/empty"
env -i PATH="$tmp/bin:$PATH" HOME="$tmp/empty" CALL_LOG="$tmp/empty-calls" \
    FORBIDDEN_LOG="$tmp/forbidden" bash "$tmp/repo/scripts/entry.sh" --interactive \
    > "$tmp/output" <<< $'\n\n2\n2\n\n'
[[ ! -e "$tmp/empty-calls" && ! -e "$tmp/forbidden" ]] || fail 'empty discovery executed commands'

# Interactive options cannot accidentally become noninteractive actions.
if env -i PATH="$tmp/bin:$PATH" HOME="$home" bash "$tmp/repo/scripts/entry.sh" \
    --interactive --no-build > "$tmp/output" 2>&1; then
    fail 'mixed interactive/noninteractive flags accepted'
fi
bash "$tmp/repo/scripts/entry.sh" --help > "$tmp/output"
printf 'ASIO setup fixture tests passed (no real installer or Wine invoked).\n'
