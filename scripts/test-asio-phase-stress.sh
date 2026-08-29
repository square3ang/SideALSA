#!/usr/bin/env bash

set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd -- "$ROOT"
BUILD_DIR="${SIDEALSA_ASIO_BUILD_DIR:-$ROOT/build-asio}"
TEST="$BUILD_DIR/sidealsa-asio-loopback-test.exe.so"
STATS="${SIDEALSA_STATS:-$ROOT/target/release/sidealsa-stats}"
NATIVE_TEST="${SIDEALSA_LOOPBACK_TEST:-$ROOT/target/release/sidealsa-loopback-test}"
NATIVE_RT_PRIORITY="${SIDEALSA_NATIVE_RT_PRIORITY:-46}"
WINE_COMMAND="${WINE:-wine}"
RUN_MS="${SIDEALSA_ASIO_PHASE_STRESS_MS:-15000}"
if ! [[ "$RUN_MS" =~ ^[1-9][0-9]*$ ]]; then
    printf 'error: SIDEALSA_ASIO_PHASE_STRESS_MS must be a positive integer\n' >&2
    exit 1
fi
IDLE_REACQUIRE_ROUNDS="${SIDEALSA_ASIO_IDLE_REACQUIRE_ROUNDS:-0}"
NATIVE_PERIODS="${SIDEALSA_ASIO_NATIVE_PERIODS:-$((RUN_MS * 3 / 4))}"
NATIVE_TOLERANCE="${SIDEALSA_ASIO_NATIVE_TOLERANCE_FRAMES:-0}"
SOCKET_PATH="${SIDEALSA_SOCKET:-/tmp/sidealsad.sock}"
EXPECTED_DAEMON_PID="${SIDEALSA_DAEMON_PID:-}"
STATS_TIMEOUT="${SIDEALSA_STATS_TIMEOUT:-3}"
STRESS_CPU_LIST="${SIDEALSA_STRESS_CPU_LIST:-}"
STAMP="$(date +%Y%m%d-%H%M%S)"
LOG_DIR="${SIDEALSA_PHASE_STRESS_LOG_DIR:-$ROOT/target/phase-stress/$STAMP}"
STRESS_TARGET="${SIDEALSA_STRESS_TARGET_DIR:-$LOG_DIR/cargo-target}"
ASIO_LOG="$LOG_DIR/asio.log"
STRESS_LOG="$LOG_DIR/stress.log"
NATIVE_BEFORE_LOG="$LOG_DIR/native-before.log"
NATIVE_AFTER_LOG="$LOG_DIR/native-after.log"
STATS_BEFORE_LOG="$LOG_DIR/stats-before.log"
STATS_AFTER_LOG="$LOG_DIR/stats-after.log"
STATS_NATIVE_BEFORE_LOG="$LOG_DIR/stats-native-before.log"
STATS_NATIVE_AFTER_LOG="$LOG_DIR/stats-native-after.log"

if [[ "${1:-}" == "--" ]]; then
    shift
fi
if (($# > 0)); then
    STRESS_COMMAND=("$@")
else
    STRESS_COMMAND=(cargo build --release --workspace)
fi

for executable in "$TEST" "$STATS" "$NATIVE_TEST"; do
    [[ -x "$executable" ]] || {
        printf 'error: required executable not found: %s\n' "$executable" >&2
        exit 1
    }
done
for command in "$WINE_COMMAND" timeout chrt grep; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'error: required command not found: %s\n' "$command" >&2
        exit 1
    }
done
if [[ -n "$STRESS_CPU_LIST" ]]; then
    command -v taskset >/dev/null 2>&1 || {
        printf 'error: taskset is required with SIDEALSA_STRESS_CPU_LIST\n' >&2
        exit 1
    }
fi
if ! [[ "$IDLE_REACQUIRE_ROUNDS" =~ ^[0-9]+$ \
    && "$NATIVE_PERIODS" =~ ^[1-9][0-9]*$ \
    && "$NATIVE_TOLERANCE" =~ ^[0-9]+$ \
    && "$STATS_TIMEOUT" =~ ^[1-9][0-9]*$ ]]; then
    printf 'error: idle rounds, periods, tolerance, and stats timeout must be valid integers\n' >&2
    exit 1
fi
RUN_TIMEOUT=$((RUN_MS * 2 / 1000 + 15))
STRESS_TIMEOUT=$((RUN_MS * 2 / 1000 + 10))

mkdir -p -- "$LOG_DIR" "$(dirname -- "$STRESS_TARGET")"
printf 'phase_stress_logs=%s\n' "$LOG_DIR"
printf 'phase_stress_window_ms=%s_per_asio_leg\n' "$RUN_MS"
printf 'phase_stress_cpu_list=%s\n' "${STRESS_CPU_LIST:-unrestricted}"
printf 'phase_stress_idle_reacquire_rounds=%s\n' "$IDLE_REACQUIRE_ROUNDS"

asio_pid=""
stress_pid=""
cleanup() {
    local status=$?
    trap - EXIT INT TERM
    for pid in "$stress_pid" "$asio_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    exit "$status"
}
trap cleanup EXIT INT TERM

stat_value() {
    local text="$1"
    local key="$2"
    local pattern="(^|[[:space:]])${key}=(-?[0-9]+)"
    [[ "$text" =~ $pattern ]] || return 1
    printf '%s\n' "${BASH_REMATCH[2]}"
}

read_stats() {
    timeout --signal=KILL "${STATS_TIMEOUT}s" \
        "$STATS" --socket "$SOCKET_PATH" --samples 1 --interval-ms 0
}

measure_native_reference() {
    local label="$1"
    local log="$2"
    local output
    local status
    local minimum
    local maximum
    local lost
    local measurements

    set +e
    output="$(timeout --signal=KILL "${RUN_TIMEOUT}s" chrt -f "$NATIVE_RT_PRIORITY" \
        "$NATIVE_TEST" --socket "$SOCKET_PATH" --periods "$NATIVE_PERIODS" \
        --output-channel 0 --input-channel 4 2>&1)"
    status=$?
    set -e
    printf '%s\n' "$output" >"$log"
    if ((status != 0)); then
        printf 'error: %s native reference failed (status %d); another PRO owner may be active\n' \
            "$label" "$status" >&2
        printf 'log: %s\n' "$log" >&2
        exit 1
    fi
    minimum="$(stat_value "$output" loopback_min_frames)"
    maximum="$(stat_value "$output" loopback_max_frames)"
    lost="$(stat_value "$output" loopback_lost_pulses)"
    measurements="$(stat_value "$output" loopback_measurements)"
    if ((measurements == 0 || lost != 0 || minimum != maximum)); then
        printf 'error: %s native phase was not stable (%s..%s, lost=%s, count=%s)\n' \
            "$label" "$minimum" "$maximum" "$lost" "$measurements" >&2
        exit 1
    fi
    NATIVE_REFERENCE="$minimum"
    printf '%s_native_phase_frames=%s\n' "$label" "$minimum"
}

before_stats="$(read_stats)"
printf '%s\n' "$before_stats" >"$STATS_BEFORE_LOG"
daemon_pid_before="$(stat_value "$before_stats" daemon_pid)"
if [[ -n "$EXPECTED_DAEMON_PID" && "$daemon_pid_before" != "$EXPECTED_DAEMON_PID" ]]; then
    printf 'error: socket peer PID %s does not match SIDEALSA_DAEMON_PID %s\n' \
        "$daemon_pid_before" "$EXPECTED_DAEMON_PID" >&2
    exit 1
fi

# This acquires and releases PRO before any load starts, so an existing owner
# makes the harness fail without disturbing the running session.
printf 'phase_stress_stage=native_before\n'
measure_native_reference before "$NATIVE_BEFORE_LOG"
native_before="$NATIVE_REFERENCE"
native_before_stats="$(read_stats)"
printf '%s\n' "$native_before_stats" >"$STATS_NATIVE_BEFORE_LOG"
pointer_phase_before="$(stat_value "$native_before_stats" duplex_pointer_phase_nanos)"
printf 'before_duplex_pointer_phase_nanos=%s\n' "$pointer_phase_before"

timeout --foreground --signal=TERM --kill-after=5s "${RUN_TIMEOUT}s" env \
    SIDEALSA_SOCKET="$SOCKET_PATH" \
    SIDEALSA_ASIO_PROBE_MS="$RUN_MS" \
    SIDEALSA_ASIO_PROBE_LOOPBACK=1 \
    SIDEALSA_ASIO_PROBE_BENCHMARK=1 \
    SIDEALSA_ASIO_PROBE_HEARTBEAT_MS=10 \
    WINEDEBUG=-all WINEDLLPATH="$BUILD_DIR" \
    "$WINE_COMMAND" "$TEST" >"$ASIO_LOG" 2>&1 &
asio_pid=$!

asio_ready=false
for _ in {1..100}; do
    if [[ -f "$ASIO_LOG" ]] && grep -Fq '[asio-probe] CreateBuffers OK' "$ASIO_LOG"; then
        asio_ready=true
        break
    fi
    if ! kill -0 "$asio_pid" 2>/dev/null; then
        break
    fi
    sleep 0.05
done
if [[ "$asio_ready" != true ]]; then
    set +e
    wait "$asio_pid"
    asio_status=$?
    set -e
    asio_pid=""
    printf 'error: ASIO probe did not become ready (status %d)\n' "$asio_status" >&2
    printf 'log: %s\n' "$ASIO_LOG" >&2
    exit 1
fi

if [[ -n "$STRESS_CPU_LIST" ]]; then
    timeout --foreground --signal=TERM --kill-after=5s "${STRESS_TIMEOUT}s" \
        taskset -c "$STRESS_CPU_LIST" env CARGO_TARGET_DIR="$STRESS_TARGET" \
        "${STRESS_COMMAND[@]}" >"$STRESS_LOG" 2>&1 &
else
    timeout --foreground --signal=TERM --kill-after=5s "${STRESS_TIMEOUT}s" \
        env CARGO_TARGET_DIR="$STRESS_TARGET" \
        "${STRESS_COMMAND[@]}" >"$STRESS_LOG" 2>&1 &
fi
stress_pid=$!
printf 'phase_stress_stage=asio_and_stress asio_pid=%s stress_pid=%s\n' \
    "$asio_pid" "$stress_pid"

set +e
wait "$asio_pid"
asio_status=$?
asio_pid=""
wait "$stress_pid"
stress_status=$?
stress_pid=""
set -e

stress_failed=false
if ((stress_status != 0 && stress_status != 124 && stress_status != 137)); then
    stress_failed=true
    printf 'warning: stress command failed (status %d)\n' "$stress_status" >&2
    printf 'log: %s\n' "$STRESS_LOG" >&2
fi
asio_failed=false
if ((asio_status != 0)) || ! grep -Fq '[asio-probe] PASS' "$ASIO_LOG"; then
    asio_failed=true
    printf 'warning: ASIO phase probe failed under stress (status %d)\n' "$asio_status" >&2
    printf 'log: %s\n' "$ASIO_LOG" >&2
fi

printf 'phase_stress_stage=native_after\n'
measure_native_reference after "$NATIVE_AFTER_LOG"
native_after="$NATIVE_REFERENCE"
native_after_stats="$(read_stats)"
printf '%s\n' "$native_after_stats" >"$STATS_NATIVE_AFTER_LOG"
printf 'after_duplex_pointer_phase_nanos=%s\n' \
    "$(stat_value "$native_after_stats" duplex_pointer_phase_nanos)"
phase_shift=$((native_after - native_before))
phase_magnitude="$phase_shift"
if ((phase_magnitude < 0)); then
    phase_magnitude=$((-phase_magnitude))
fi
printf 'common_path_phase_shift_frames=%s\n' "$phase_shift"

idle_phase_failed=false
for ((round = 1; round <= IDLE_REACQUIRE_ROUNDS; round++)); do
    idle_stress_log="$LOG_DIR/idle-stress-$round.log"
    idle_native_log="$LOG_DIR/native-idle-$round.log"
    idle_stats_log="$LOG_DIR/stats-idle-$round.log"
    printf 'phase_stress_stage=idle_stress round=%s\n' "$round"
    set +e
    if [[ -n "$STRESS_CPU_LIST" ]]; then
        timeout --foreground --signal=TERM --kill-after=5s "${STRESS_TIMEOUT}s" \
            taskset -c "$STRESS_CPU_LIST" env CARGO_TARGET_DIR="$STRESS_TARGET" \
            "${STRESS_COMMAND[@]}" >"$idle_stress_log" 2>&1
    else
        timeout --foreground --signal=TERM --kill-after=5s "${STRESS_TIMEOUT}s" \
            env CARGO_TARGET_DIR="$STRESS_TARGET" \
            "${STRESS_COMMAND[@]}" >"$idle_stress_log" 2>&1
    fi
    idle_stress_status=$?
    set -e
    if ((idle_stress_status != 0)); then
        printf 'error: idle stress round %s failed (status %d)\n' \
            "$round" "$idle_stress_status" >&2
        printf 'log: %s\n' "$idle_stress_log" >&2
        exit 1
    fi

    measure_native_reference "idle_$round" "$idle_native_log"
    idle_phase="$NATIVE_REFERENCE"
    idle_stats="$(read_stats)"
    printf '%s\n' "$idle_stats" >"$idle_stats_log"
    idle_pointer_phase="$(stat_value "$idle_stats" duplex_pointer_phase_nanos)"
    idle_shift=$((idle_phase - native_before))
    idle_magnitude="$idle_shift"
    if ((idle_magnitude < 0)); then
        idle_magnitude=$((-idle_magnitude))
    fi
    printf 'idle_%s_common_path_phase_shift_frames=%s\n' "$round" "$idle_shift"
    printf 'idle_%s_duplex_pointer_phase_nanos=%s\n' "$round" "$idle_pointer_phase"
    if ((idle_magnitude > NATIVE_TOLERANCE)); then
        printf 'warning: idle round %s moved native phase by %s frames (limit %s)\n' \
            "$round" "$idle_shift" "$NATIVE_TOLERANCE" >&2
        idle_phase_failed=true
    fi
done

after_stats="$(read_stats)"
printf '%s\n' "$after_stats" >"$STATS_AFTER_LOG"
daemon_pid_after="$(stat_value "$after_stats" daemon_pid)"
if [[ "$daemon_pid_before" != "$daemon_pid_after" ]]; then
    printf 'error: sidealsad PID changed from %s to %s\n' \
        "$daemon_pid_before" "$daemon_pid_after" >&2
    exit 1
fi
for key in pro client core hw_playback hw_capture timeline_resets generation; do
    before="$(stat_value "$before_stats" "$key")"
    after="$(stat_value "$after_stats" "$key")"
    if [[ "$before" != "$after" ]]; then
        printf 'error: %s changed from %s to %s\n' "$key" "$before" "$after" >&2
        exit 1
    fi
done

if [[ "$asio_failed" == true ]]; then
    printf 'error: ASIO phase was not stable under stress\n' >&2
    exit 1
fi
if [[ "$stress_failed" == true ]]; then
    printf 'error: stress command did not complete successfully\n' >&2
    exit 1
fi
if [[ "$idle_phase_failed" == true ]]; then
    printf 'error: one or more idle reacquisition rounds moved native phase\n' >&2
    exit 1
fi
if ((phase_magnitude > NATIVE_TOLERANCE)); then
    printf 'error: native common-path phase moved by %s frames (limit %s)\n' \
        "$phase_shift" "$NATIVE_TOLERANCE" >&2
    exit 1
fi

printf 'stress_status=%s\n' "$stress_status"
printf 'stress_cpu_list=%s\n' "${STRESS_CPU_LIST:-unrestricted}"
printf 'logs=%s\n' "$LOG_DIR"
printf 'PASS: ASIO and native phase remained fixed at %s frames\n' "$native_after"
