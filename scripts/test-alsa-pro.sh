#!/usr/bin/env bash
set -euo pipefail

# Explicit opt-in only. No daemon/service management, installation, or registration.
# Fixed private PCM in probe.c uses /tmp/sidealsad.sock, not system PCM definitions.
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
usage() {
    printf 'Usage: bash %s --compile-only | --run installed|local [periods [work_us [blocking|poll [0|46 [prefill_periods=4]]]]]\n' "$0"
    printf 'Local: ALSA_PLUGIN_DIR (default target/release). Stats: SIDEALSA_STATS. Probe requests Q64/app B256; hardware buffer is independent.\n'
}
[[ $# -ge 1 ]] || { usage; exit 2; }
action=$1
shift
case "$action" in
    --compile-only) [[ $# == 0 ]] || { usage; exit 2; } ;;
    --run) [[ $# -ge 1 && ( $1 == installed || $1 == local ) ]] || { usage; exit 2; } ;;
    *) usage; exit 2 ;;
esac
build=$(mktemp -d "${TMPDIR:-/tmp}/sidealsa-alsa-pro.XXXXXX")
trap 'rm -rf -- "$build"' EXIT
read -r -a cflags <<< "$(pkg-config --cflags alsa)"
read -r -a libs <<< "$(pkg-config --libs alsa)"
"${CC:-cc}" -std=c11 -O2 -Wall -Wextra -Werror "${cflags[@]}" \
    "$ROOT/crates/sidealsa-alsa/tests/probe.c" -o "$build/probe" "${libs[@]}"
[[ $action == --run ]] || { printf 'Compile passed; no audio opened.\n'; exit 0; }
plugin=$1
shift
log_dir="${SIDEALSA_ALSA_PRO_LOG_DIR:-$ROOT/target/alsa-pro/$(date +%Y%m%d-%H%M%S)-$plugin}"
mkdir -p -- "$log_dir"
timeout_seconds="${SIDEALSA_ALSA_PRO_TIMEOUT:-30}"
[[ "$timeout_seconds" =~ ^[0-9]{1,3}$ ]] && ((10#$timeout_seconds > 0))
if [[ $plugin == local ]]; then
    export ALSA_PLUGIN_DIR="${ALSA_PLUGIN_DIR:-$ROOT/target/release}"
    [[ -f "$ALSA_PLUGIN_DIR/libasound_module_pcm_sidealsa.so" ]] || {
        printf 'Missing local plugin in %s\n' "$ALSA_PLUGIN_DIR" >&2; exit 1;
    }
else
    unset ALSA_PLUGIN_DIR
fi
stats=${SIDEALSA_STATS:-$ROOT/target/release/sidealsa-stats}
stat_value() {
    local pattern="(^|[[:space:]])$2=([0-9]+)"
    [[ $1 =~ $pattern ]] || return 1
    printf '%s\n' "${BASH_REMATCH[2]}"
}
read_stats() {
    "$stats" --socket /tmp/sidealsad.sock --samples 1 --interval-ms 0 "$@"
}
before=$(read_stats)
printf '%s\n' "$before" > "$log_dir/before.log"
printf 'logs=%s\n' "$log_dir"
printf 'plugin=%s ALSA_PLUGIN_DIR=%s\n' "$plugin" "${ALSA_PLUGIN_DIR:-<system>}"
pid=$(stat_value "$before" daemon_pid)
generation=$(stat_value "$before" generation)
printf 'daemon_pid=%s generation=%s\n' "$pid" "$generation"
status=0
timeout --kill-after=2s "${timeout_seconds}s" "$build/probe" "$@" 2>&1 | tee "$log_dir/probe.log" || status=$?
after=$(read_stats --expect-peer-pid "$pid")
printf '%s\n' "$after" > "$log_dir/after.log"
[[ $(stat_value "$after" daemon_pid) == "$pid" && $(stat_value "$after" generation) == "$generation" ]] || {
    printf 'FAIL: daemon PID or hardware generation changed\n' >&2; exit 1;
}
printf 'Daemon PID/generation unchanged. Client EPIPEs are not hardware XRUN counts.\n'
for key in pro client core hw_playback hw_capture timeline_resets shared_underruns shared_overruns; do
    before_value=$(stat_value "$before" "$key")
    after_value=$(stat_value "$after" "$key")
    delta=$((after_value - before_value))
    printf '%s_delta=%s\n' "$key" "$delta"
    ((delta == 0)) || status=1
done
exit "$status"
