#!/usr/bin/env bash
# Silent callback-time sweep. Never restarts services or changes hardware settings.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
BUILD_DIR="${SIDEALSA_ASIO_BUILD_DIR:-$ROOT/build-asio}"
DLL_DIR="${SIDEALSA_ASIO_DLL_DIR:-/usr/local/lib/wine}"
SOCKET="${SIDEALSA_SOCKET:-/tmp/sidealsad.sock}"
STATS="$ROOT/target/release/sidealsa-stats"
PROBE="$BUILD_DIR/sidealsa-asio-probe.exe.so"
RUN_MS="${SIDEALSA_DEADLINE_RUN_MS:-4000}"
WORKERS="${SIDEALSA_DEADLINE_WORKERS:-0}"
SPIN_US=driver-default
SPIN_ENV=()
if [[ -v SIDEALSA_ASIO_SPIN_US ]]; then
    SPIN_US="$SIDEALSA_ASIO_SPIN_US"
    [[ "$SPIN_US" =~ ^[0-9]{1,3}$ ]] && ((10#$SPIN_US <= 250))
    SPIN_ENV=("SIDEALSA_ASIO_SPIN_US=$SPIN_US")
fi
read -r -a WORK_VALUES <<< "${SIDEALSA_DEADLINE_WORK_US:-0 50 100 150 200 250 300 200 100 0}"
[[ "$RUN_MS" =~ ^[0-9]{1,5}$ && "$WORKERS" =~ ^[0-9]{1,2}$ ]]
RUN_MS=$((10#$RUN_MS)); WORKERS=$((10#$WORKERS))
((RUN_MS >= 1000 && RUN_MS <= 30000 && WORKERS <= 64 && ${#WORK_VALUES[@]} > 0))
for work in "${WORK_VALUES[@]}"; do
    [[ "$work" =~ ^[0-9]{1,4}$ ]] && ((10#$work <= 1000))
done
[[ -x "$STATS" && -x "$PROBE" ]]
for executable in wine timeout pgrep; do command -v "$executable" >/dev/null; done
MEMORY=0
((WORKERS == 0)) || MEMORY=512
TIMEOUT=$((RUN_MS * 2 / 1000 + 20))
LOG_DIR="${SIDEALSA_DEADLINE_LOG_DIR:-$ROOT/target/deadline-sweep/$(date +%Y%m%d-%H%M%S)}"
mkdir -p -- "$LOG_DIR"

value() {
    local pattern="(^|[[:space:]])${2}=([0-9]+)"
    [[ "$1" =~ $pattern ]] || return 1
    printf '%s' "${BASH_REMATCH[2]}"
}
initial="$("$STATS" --socket "$SOCKET" --samples 1 --interval-ms 0)"
printf '%s\n' "$initial" > "$LOG_DIR/initial.log"
daemon_pid="$(value "$initial" daemon_pid)"
generation="$(value "$initial" generation)"
stats() { "$STATS" --socket "$SOCKET" --samples 1 --interval-ms 0 --expect-peer-pid "$daemon_pid"; }
printf 'logs=%s dll=%s workers=%s spin_us=%s daemon_pid=%s generation=%s\n' "$LOG_DIR" "$DLL_DIR" "$WORKERS" "$SPIN_US" "$daemon_pid" "$generation"
index=0
failed=0
for work in "${WORK_VALUES[@]}"; do
    work=$((10#$work))
    before="$(stats)"
    [[ "$(value "$before" generation)" == "$generation" ]]
    printf '%s\n' "$before" > "$LOG_DIR/$index-before.log"
    status=0
    timeout --kill-after=3s "${TIMEOUT}s" env \
        -u SIDEALSA_ASIO_SPIN_US \
        -u SIDEALSA_ASIO_PROBE_LOOPBACK -u SIDEALSA_ASIO_PROBE_LIFECYCLE \
        -u SIDEALSA_ASIO_CRASH_AFTER_START -u SIDEALSA_ASIO_PROBE_SINE_SELF_TEST \
        -u SIDEALSA_ASIO_EXPECTED_LOOPBACK_FRAMES -u SIDEALSA_ASIO_EXPECTED_OUTPUT_LATENCY \
        "${SPIN_ENV[@]}" SIDEALSA_SOCKET="$SOCKET" \
        SIDEALSA_ASIO_PROBE_SINE_VOICES=0 SIDEALSA_ASIO_PROBE_MS="$RUN_MS" \
        SIDEALSA_ASIO_PROBE_BENCHMARK=1 SIDEALSA_ASIO_PROBE_CALLBACK_WORK_US="$work" \
        SIDEALSA_ASIO_PROBE_STRESS_THREADS="$WORKERS" \
        SIDEALSA_ASIO_PROBE_STRESS_MEMORY_MIB="$MEMORY" \
        SIDEALSA_ASIO_PROBE_HEARTBEAT_MS=10 SIDEALSA_ASIO_PROBE_RT_PRIORITY=0 \
        WINEDEBUG=-all WINEDLLPATH="$DLL_DIR" \
        wine "$PROBE" > "$LOG_DIR/$index-probe.log" 2>&1 &
    runner=$!
    found=0
    # Capture loaded-library provenance while the owned benchmark is running.
    for ((attempt=0; attempt<10; attempt++)); do
        sleep 0.5
        for observed_pid in $(pgrep -P "$runner" || true); do
            [[ -r "/proc/$observed_pid/maps" ]] || continue
            while IFS= read -r mapping; do
                if [[ "$mapping" == *sidealsa-asio*dll.so* ]]; then
                    printf 'mapped_pid=%s %s\n' "$observed_pid" "$mapping" >> "$LOG_DIR/$index-maps.log"
                    if ((found == 0)); then printf 'loaded_driver=%s\n' "$mapping"; fi
                    found=1
                fi
            done < "/proc/$observed_pid/maps" || true
        done
        ((found == 0)) || break
    done
    wait "$runner" || status=$?
    after="$(stats)"
    printf '%s\n' "$after" > "$LOG_DIR/$index-after.log"
    printf 'index=%s work_us=%s workers=%s spin_us=%s exit=%s' "$index" "$work" "$WORKERS" "$SPIN_US" "$status"
    for key in pro client core hw_playback hw_capture generation shared_underruns shared_overruns; do
        before_value="$(value "$before" "$key")"
        after_value="$(value "$after" "$key")"
        delta=$((after_value - before_value))
        ((delta >= 0)) || { printf '\nABORT: counter regressed: %s\n' "$key" >&2; exit 1; }
        printf ' %s_delta=%s' "$key" "$delta"
        ((delta == 0)) || failed=1
        if [[ "$key" == hw_playback || "$key" == hw_capture || "$key" == generation ]]; then
            ((delta == 0)) || { printf '\nABORT: hardware changed\n' >&2; exit 1; }
        fi
    done
    printf '\n'
    passed=0
    while IFS= read -r line; do
        if [[ "$line" == *'[asio-probe] benchmark callbacks:'* ]]; then printf '%s\n' "$line"; fi
        if [[ "$line" == *'[asio-probe] benchmark thread_cpu:'* ]]; then printf '%s\n' "$line"; fi
        if [[ "$line" == '[asio-probe] PASS' ]]; then passed=1; fi
    done < "$LOG_DIR/$index-probe.log"
    printf 'probe_exit_status=%s loaded_driver_observed=%s spin_us=%s work_us=%s workers=%s\n' "$status" "$found" "$SPIN_US" "$work" "$WORKERS" > "$LOG_DIR/$index-status.log"
    ((status == 0 && passed == 1)) || exit 1
    ((found == 1)) || failed=1
    index=$((index + 1))
done
printf 'measurement_complete=1 failures_observed=%s logs=%s\n' "$failed" "$LOG_DIR"
exit "$failed"
