#!/usr/bin/env bash
# Prefix-ownership guard tests for the real scripts/install-asio.sh.
# Mock build artifacts only; logging wine/umu stubs verify which runner runs
# where. No real Wine, builds, or services.
set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
((EUID != 0)) || { printf 'Run fixture tests as a non-root user.\n' >&2; exit 1; }
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
mkdir -p "$tmp/build" "$tmp/root" "$tmp/home" "$tmp/bin"
printf 'MOCK ASIO PE\n' > "$tmp/build/sidealsa-asio64.dll"
printf 'MOCK ASIO Unix\n' > "$tmp/build/sidealsa-asio64.dll.so"
printf '%s\n' '#!/usr/bin/env bash' '[[ "${STUB_FAIL:-0}" == 1 ]] && exit 3' 'printf "%s\n" "wine $*" >> "$WINE_CALL_LOG"' 'exit 0' \
    > "$tmp/bin/wine"
printf '%s\n' '#!/usr/bin/env bash' '[[ "${STUB_FAIL:-0}" == 1 ]] && exit 3' 'printf "%s\n" "explicit $*" >> "$WINE_CALL_LOG"' 'exit 0' \
    > "$tmp/bin/custom-wine"
printf '%s\n' '#!/usr/bin/env bash' '[[ "${STUB_FAIL:-0}" == 1 ]] && exit 3' 'printf "GAMEID=%s WINEPREFIX=%s umu-run %s\n" "$GAMEID" "$WINEPREFIX" "$*" >> "$UMU_CALL_LOG"' 'exit 0' \
    > "$tmp/bin/umu-run"
printf '%s\n' '#!/usr/bin/env bash' '[[ "${STUB_FAIL:-0}" == 1 ]] && exit 3' 'printf "bottles-cli %s\n" "$*" >> "$BOTTLES_CALL_LOG"' 'exit 0' \
    > "$tmp/bin/bottles-cli"
chmod +x "$tmp/bin/wine" "$tmp/bin/custom-wine" "$tmp/bin/umu-run" "$tmp/bin/bottles-cli"

plain="$tmp/home/plain"
proton="$tmp/home/steamapps/compatdata/44/pfx"
bottled="$tmp/home/bottled-game"
for prefix in "$plain" "$proton" "$bottled"; do
    mkdir -p "$prefix/drive_c/windows/system32"
done
touch "$proton/tracked_files"
touch "$bottled/bottle.yml"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
install() {
    rm -f -- "$tmp/wine-calls" "$tmp/umu-calls" "$tmp/bottles-calls"
    if env -i PATH="$tmp/bin:/usr/bin:/bin" HOME="$tmp/home" WINE_CALL_LOG="$tmp/wine-calls" \
        UMU_CALL_LOG="$tmp/umu-calls" BOTTLES_CALL_LOG="$tmp/bottles-calls" \
        STUB_FAIL="${STUB_FAIL:-0}" \
        bash "$ROOT/scripts/install-asio.sh" --install-root "$tmp/root" \
        --build-dir "$tmp/build" --no-build "$@" > "$tmp/output" 2>&1; then
        printf 'ok\n'
    else
        printf 'refused\n'
    fi
}

# Plain prefix with default wine: registers, wine runs once.
[[ "$(install --steam-prefix "$plain")" == ok ]] || fail 'plain prefix refused'
grep -Fq 'registering SideALSA ASIO' "$tmp/output" || fail 'plain prefix not registered'
[[ "$(wc -l < "$tmp/wine-calls")" == 1 ]] || fail 'wine not invoked exactly once'
[[ -f "$plain/drive_c/windows/system32/sidealsa-asio64.dll" ]] || fail 'dll not staged'

# Proton prefix with default wine: umu-run registers, system wine never runs.
[[ "$(install --steam-prefix "$proton")" == ok ]] || fail 'proton prefix refused'
grep -Fq 'umu-run' "$tmp/output" || fail 'umu-run path not reported'
[[ ! -e "$tmp/wine-calls" ]] || fail 'system wine ran inside proton prefix'
[[ "$(wc -l < "$tmp/umu-calls")" == 1 ]] || fail 'umu-run not invoked exactly once'
grep -Fq 'GAMEID=umu-44' "$tmp/umu-calls" || fail 'umu GAMEID missing appid'
grep -Fq "WINEPREFIX=$proton" "$tmp/umu-calls" || fail 'umu WINEPREFIX wrong'
grep -Fq 'regsvr32 /s sidealsa-asio64.dll' "$tmp/umu-calls" || fail 'umu regsvr32 args wrong'
[[ -f "$proton/drive_c/windows/system32/sidealsa-asio64.dll" ]] || fail 'dll not staged in proton prefix'

# Bottles bottle with its bottle name: DLL staged, bottles-cli persists the
# library path and registers, host wine never runs.
[[ "$(install --steam-prefix "$bottled" --bottle-name MyBottle)" == ok ]] || fail 'named bottle failed'
grep -Fq 'MyBottle' "$tmp/output" || fail 'bottle name not reported'
[[ ! -e "$tmp/wine-calls" ]] || fail 'wine ran for named bottle'
grep -Fq 'bottles-cli edit -b MyBottle --env-var WINEDLLPATH=' "$tmp/bottles-calls" || fail 'bottle env-var not persisted'
grep -Fq 'bottles-cli shell -b MyBottle' "$tmp/bottles-calls" || fail 'bottles-cli not invoked'
grep -Fq 'regsvr32 /s sidealsa-asio64.dll' "$tmp/bottles-calls" || fail 'bottles-cli regsvr32 args wrong'
[[ -f "$bottled/drive_c/windows/system32/sidealsa-asio64.dll" ]] || fail 'dll not staged in bottle'

# A failing bottles-cli aborts instead of reporting success.
STUB_FAIL=1
[[ "$(install --steam-prefix "$bottled" --bottle-name MyBottle)" == refused ]] || fail 'bottles-cli failure accepted'
if grep -Fq 'registered SideALSA ASIO' "$tmp/output"; then fail 'false success after bottles-cli failure'; fi
STUB_FAIL=0

# A failing wine aborts instead of reporting success.
STUB_FAIL=1
[[ "$(install --steam-prefix "$plain")" == refused ]] || fail 'wine failure accepted'
if grep -Fq 'registered SideALSA ASIO' "$tmp/output"; then fail 'false success after wine failure'; fi
STUB_FAIL=0

# Bottles bottle without a name: DLL staged, manual regsvr32 reported.
[[ "$(install --steam-prefix "$bottled")" == ok ]] || fail 'nameless bottle failed'
grep -Fq 'ACTION REQUIRED' "$tmp/output" || fail 'bottle action notice missing'
grep -Fq 'regsvr32 /s sidealsa-asio64.dll' "$tmp/output" || fail 'manual reg step missing'
[[ ! -e "$tmp/wine-calls" && ! -e "$tmp/bottles-calls" ]] || fail 'runner executed for nameless bottle'
[[ -f "$bottled/drive_c/windows/system32/sidealsa-asio64.dll" ]] || fail 'dll not staged in bottle'

# --bottle-name without a preceding prefix is rejected.
[[ "$(install --bottle-name Orphaned)" == refused ]] || fail 'orphan bottle name accepted'

# Proton prefix without umu-run on PATH: refused before any change.
mkdir -p "$tmp/minbin"
for tool in readlink mkdir install cmp stat mktemp rm mv ln env sha256sum dirname basename; do
    ln -s "$(command -v "$tool")" "$tmp/minbin/$tool"
done
rm -f -- "$tmp/wine-calls" "$tmp/umu-calls"
if env -i PATH="$tmp/minbin" HOME="$tmp/home" WINE_CALL_LOG="$tmp/wine-calls" \
    UMU_CALL_LOG="$tmp/umu-calls" \
    "$BASH" "$ROOT/scripts/install-asio.sh" --install-root "$tmp/root" \
    --build-dir "$tmp/build" --no-build --steam-prefix "$proton" > "$tmp/output" 2>&1; then
    fail 'umu-less proton prefix accepted'
fi
grep -Fiq 'umu-run' "$tmp/output" || fail 'umu refusal does not name umu-run'
[[ ! -e "$tmp/wine-calls" && ! -e "$tmp/umu-calls" ]] || fail 'runner executed without umu-run'

# Explicit --wine accepts responsibility: proceeds with the chosen binary.
[[ "$(install --steam-prefix "$proton" --wine custom-wine)" == ok ]] || fail 'explicit wine refused'
grep -Fq 'explicit regsvr32' "$tmp/wine-calls" || fail 'explicit wine not used'
[[ -f "$proton/drive_c/windows/system32/sidealsa-asio64.dll" ]] || fail 'dll not staged with explicit wine'

# Explicit WINE env var works the same without flags.
rm -f -- "$tmp/wine-calls"
if env -i PATH="$tmp/bin:/usr/bin:/bin" HOME="$tmp/home" WINE_CALL_LOG="$tmp/wine-calls" \
    WINE=custom-wine bash "$ROOT/scripts/install-asio.sh" --install-root "$tmp/root" \
    --build-dir "$tmp/build" --no-build --steam-prefix "$bottled" > "$tmp/output" 2>&1; then
    :
else
    fail 'WINE env override refused'
fi
grep -Fq 'explicit regsvr32' "$tmp/wine-calls" || fail 'WINE env binary not used'

printf 'ASIO prefix-guard fixture tests passed (mock artifacts only).\n'
