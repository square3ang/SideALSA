#!/usr/bin/env bash

set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
BUILD_DIR="${SIDEALSA_ASIO_BUILD_DIR:-$ROOT/build-asio}"
TEST="$BUILD_DIR/sidealsa-asio-loopback-test.exe.so"
STATS="${SIDEALSA_STATS:-$ROOT/target/release/sidealsa-stats}"
NATIVE_TEST="${SIDEALSA_LOOPBACK_TEST:-$ROOT/target/release/sidealsa-loopback-test}"
NATIVE_RT_PRIORITY="${SIDEALSA_NATIVE_RT_PRIORITY:-46}"
WINE_COMMAND="${WINE:-wine}"
RUN_MS="${SIDEALSA_ASIO_REACQUIRE_MS:-5000}"
NATIVE_PERIODS="${SIDEALSA_ASIO_NATIVE_PERIODS:-$((RUN_MS * 3 / 4))}"
NATIVE_TOLERANCE="${SIDEALSA_ASIO_NATIVE_TOLERANCE_FRAMES:-0}"
SOCKET_PATH="${SIDEALSA_SOCKET:-/tmp/sidealsad.sock}"
EXPECTED_DAEMON_PID="${SIDEALSA_DAEMON_PID:-}"
STATS_TIMEOUT="${SIDEALSA_STATS_TIMEOUT:-3}"
RUN_TIMEOUT=$((RUN_MS * 2 / 1000 + 15))

[[ -x "$TEST" ]] || {
    printf 'error: ASIO loopback test not found: %s\n' "$TEST" >&2
    exit 1
}
[[ -x "$STATS" ]] || {
    printf 'error: stats executable not found: %s\n' "$STATS" >&2
    exit 1
}
[[ -x "$NATIVE_TEST" ]] || {
    printf 'error: native loopback test not found: %s\n' "$NATIVE_TEST" >&2
    exit 1
}
command -v "$WINE_COMMAND" >/dev/null 2>&1 || {
    printf 'error: Wine command not found: %s\n' "$WINE_COMMAND" >&2
    exit 1
}
command -v timeout >/dev/null 2>&1 || {
    printf 'error: timeout command not found\n' >&2
    exit 1
}
command -v chrt >/dev/null 2>&1 || {
    printf 'error: chrt command not found\n' >&2
    exit 1
}
stat_value() {
    local text="$1"
    local key="$2"
    local pattern="(^|[[:space:]])${key}=([0-9]+)"
    [[ "$text" =~ $pattern ]] || return 1
    printf '%s\n' "${BASH_REMATCH[2]}"
}

read_stats() {
    timeout --signal=KILL "${STATS_TIMEOUT}s" \
        "$STATS" --socket "$SOCKET_PATH" --samples 1 --interval-ms 0
}

timed_out() {
    [[ "$1" -eq 124 || "$1" -eq 137 ]]
}

measure_native_reference() {
    local label="$1"
    local output
    local status
    local minimum
    local maximum
    local lost
    local measurements

    set +e
    output="$(timeout --signal=KILL "${RUN_TIMEOUT}s" chrt -f "$NATIVE_RT_PRIORITY" \
        "$NATIVE_TEST" \
        --socket "$SOCKET_PATH" --periods "$NATIVE_PERIODS" \
        --output-channel 0 --input-channel 4 2>&1)"
    status=$?
    set -e
    printf '%s\n' "$output"
    if ((status != 0)); then
        printf 'error: %s native phase reference failed (status %d)\n' \
            "$label" "$status" >&2
        exit 1
    fi
    minimum="$(stat_value "$output" loopback_min_frames)"
    maximum="$(stat_value "$output" loopback_max_frames)"
    lost="$(stat_value "$output" loopback_lost_pulses)"
    measurements="$(stat_value "$output" loopback_measurements)"
    if ((measurements == 0 || lost != 0 || minimum != maximum)); then
        printf 'error: %s native phase reference was not stable (%s..%s, lost=%s, count=%s)\n' \
            "$label" "$minimum" "$maximum" "$lost" "$measurements" >&2
        exit 1
    fi
    NATIVE_REFERENCE="$minimum"
}

check_phase_normalized_parity() {
    local label="$1"
    local asio="$2"
    local native="$3"
    local residual=$((asio - native))
    local magnitude="$residual"
    if ((magnitude < 0)); then
        magnitude=$((-magnitude))
    fi
    printf 'phase_analysis_%s_asio_raw_frames=%s\n' "$label" "$asio"
    printf 'phase_analysis_%s_native_raw_frames=%s\n' "$label" "$native"
    printf 'phase_analysis_%s_asio_minus_native_frames=%s\n' "$label" "$residual"
    if ((magnitude > NATIVE_TOLERANCE)); then
        printf 'error: %s ASIO frontend residual is %s frames (limit %s); reference window is inconclusive or the frontend added latency\n' \
            "$label" "$residual" "$NATIVE_TOLERANCE" >&2
        exit 1
    fi
}

before_stats="$(read_stats)"
daemon_pid_before="$(stat_value "$before_stats" daemon_pid)"
if [[ -n "$EXPECTED_DAEMON_PID" && "$daemon_pid_before" != "$EXPECTED_DAEMON_PID" ]]; then
    printf 'error: socket peer PID %s does not match SIDEALSA_DAEMON_PID %s\n' \
        "$daemon_pid_before" "$EXPECTED_DAEMON_PID" >&2
    exit 1
fi
set +e
baseline_output="$(timeout --signal=KILL "${RUN_TIMEOUT}s" env \
    SIDEALSA_SOCKET="$SOCKET_PATH" SIDEALSA_ASIO_PROBE_MS="$RUN_MS" WINEDEBUG=-all \
    WINEDLLPATH="$BUILD_DIR" "$WINE_COMMAND" "$TEST" 2>&1)"
baseline_status=$?
set -e
printf '%s\n' "$baseline_output"
if ((baseline_status != 0)) \
    || [[ "$baseline_output" == *" failed:"* ]] \
    || [[ "$baseline_output" != *"[asio-probe] PASS"* ]]; then
    printf 'error: baseline probe reported failure (Wine status %d)\n' \
        "$baseline_status" >&2
    exit 1
fi
baseline_pattern='second loopback: emitted=[0-9]+ count=[0-9]+ lost=0 pending=0 min=([0-9]+) max=([0-9]+)'
if [[ "$baseline_output" =~ $baseline_pattern ]]; then
    baseline="${BASH_REMATCH[1]}"
    baseline_max="${BASH_REMATCH[2]}"
else
    printf 'error: could not read stable baseline latency\n' >&2
    exit 1
fi
if [[ "$baseline" != "$baseline_max" ]]; then
    printf 'error: baseline latency varied from %s to %s frames\n' \
        "$baseline" "$baseline_max" >&2
    exit 1
fi
measure_native_reference baseline
baseline_native="$NATIVE_REFERENCE"
check_phase_normalized_parity baseline "$baseline" "$baseline_native"

set +e
crash_output="$(timeout --signal=KILL 10s env SIDEALSA_SOCKET="$SOCKET_PATH" \
    SIDEALSA_ASIO_PROBE_MS=1000 SIDEALSA_ASIO_CRASH_AFTER_START=1 WINEDEBUG=-all \
    WINEDLLPATH="$BUILD_DIR" "$WINE_COMMAND" "$TEST" 2>&1)"
crash_status=$?
set -e
printf '%s\n' "$crash_output"
crash_pattern='crash loopback: emitted=[0-9]+ count=([1-9][0-9]*)'
if timed_out "$crash_status" || ((crash_status == 0)) \
    || [[ "$crash_output" == *" failed:"* ]] \
    || [[ "$crash_output" != *"[asio-probe] intentional process crash"* ]] \
    || [[ ! "$crash_output" =~ $crash_pattern ]]; then
    printf 'error: crash probe did not reach intentional crash (Wine status %d)\n' \
        "$crash_status" >&2
    exit 1
fi

sleep 1
set +e
reacquire_output="$(timeout --signal=KILL "${RUN_TIMEOUT}s" env \
    SIDEALSA_SOCKET="$SOCKET_PATH" SIDEALSA_ASIO_PROBE_MS="$RUN_MS" \
    WINEDEBUG=-all WINEDLLPATH="$BUILD_DIR" "$WINE_COMMAND" "$TEST" 2>&1)"
reacquire_status=$?
set -e
printf '%s\n' "$reacquire_output"
if ((reacquire_status != 0)) \
    || [[ "$reacquire_output" == *" failed:"* ]] \
    || [[ "$reacquire_output" != *"[asio-probe] PASS"* ]]; then
    printf 'error: reacquire probe failed (Wine status %d)\n' \
        "$reacquire_status" >&2
    exit 1
fi
reacquire_pattern='second loopback: emitted=[0-9]+ count=[0-9]+ lost=0 pending=0 min=([0-9]+) max=([0-9]+)'
if [[ "$reacquire_output" =~ $reacquire_pattern ]]; then
    reacquired="${BASH_REMATCH[1]}"
    reacquired_max="${BASH_REMATCH[2]}"
else
    printf 'error: could not read stable reacquisition latency\n' >&2
    exit 1
fi
if [[ "$reacquired" != "$reacquired_max" ]]; then
    printf 'error: reacquisition latency varied from %s to %s frames\n' \
        "$reacquired" "$reacquired_max" >&2
    exit 1
fi
measure_native_reference reacquired
reacquired_native="$NATIVE_REFERENCE"
check_phase_normalized_parity reacquired "$reacquired" "$reacquired_native"
common_path_shift=$((reacquired_native - baseline_native))
printf 'phase_analysis_common_path_shift_frames=%s\n' "$common_path_shift"
after_stats="$(read_stats)"
daemon_pid_after="$(stat_value "$after_stats" daemon_pid)"

for key in hw_playback hw_capture timeline_resets generation; do
    before="$(stat_value "$before_stats" "$key")"
    after="$(stat_value "$after_stats" "$key")"
    if [[ "$before" != "$after" ]]; then
        printf 'error: %s changed from %s to %s\n' "$key" "$before" "$after" >&2
        exit 1
    fi
done

before_periods="$(stat_value "$before_stats" periods)"
after_periods="$(stat_value "$after_stats" periods)"
if ((after_periods <= before_periods)); then
    printf 'error: daemon period counter did not advance monotonically\n' >&2
    exit 1
fi
if [[ "$daemon_pid_before" != "$daemon_pid_after" ]]; then
    printf 'error: sidealsad PID changed from %s to %s\n' \
        "$daemon_pid_before" "$daemon_pid_after" >&2
    exit 1
fi

printf 'PASS: ASIO frontend residual is zero; common-path shift=%s frames\n' \
    "$common_path_shift"
