#!/usr/bin/env bash

# Exercises real DSP and background work inside the same ASIO playback process.
set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
BUILD_DIR="${SIDEALSA_ASIO_BUILD_DIR:-$ROOT/build-asio}"
DLL_DIR="${SIDEALSA_ASIO_DLL_DIR:-$BUILD_DIR}"
TEST="$BUILD_DIR/sidealsa-asio-loopback-test.exe.so"
STATS="${SIDEALSA_STATS:-$ROOT/target/release/sidealsa-stats}"
NATIVE="${SIDEALSA_LOOPBACK_TEST:-$ROOT/target/release/sidealsa-loopback-test}"
SOCKET="${SIDEALSA_SOCKET:-/tmp/sidealsad.sock}"
WINE_COMMAND="${WINE:-wine}"
RUN_MS="${SIDEALSA_ASIO_AUDIO_LOAD_MS:-8000}"
VOICES="${SIDEALSA_ASIO_AUDIO_LOAD_VOICES:-512}"
THREADS="${SIDEALSA_ASIO_AUDIO_LOAD_THREADS:-24}"
MEMORY="${SIDEALSA_ASIO_AUDIO_LOAD_MEMORY_MIB:-512}"
NATIVE_PERIODS="${SIDEALSA_ASIO_NATIVE_PERIODS:-1500}"
read -r -a CASES <<< "${SIDEALSA_ASIO_AUDIO_LOAD_CASES:-pulse_only sine_baseline dsp workers combined}"
((${#CASES[@]} > 0)) || { printf 'at least one audio load case is required\n' >&2; exit 2; }
declare -A selected_cases=()
for case_name in "${CASES[@]}"; do
    case "$case_name" in
        pulse_only|sine_baseline|dsp|workers|combined) ;;
        *) printf 'unknown audio load case: %s\n' "$case_name" >&2; exit 2 ;;
    esac
    [[ -z "${selected_cases[$case_name]+selected}" ]] || { printf 'duplicate case: %s\n' "$case_name" >&2; exit 2; }
    selected_cases[$case_name]=1
done

for value in "$RUN_MS" "$VOICES" "$THREADS" "$MEMORY" "$NATIVE_PERIODS"; do
    [[ "$value" =~ ^[0-9]{1,7}$ ]] || { printf 'invalid numeric setting: %s\n' "$value" >&2; exit 2; }
done
RUN_MS=$((10#$RUN_MS)); VOICES=$((10#$VOICES)); THREADS=$((10#$THREADS))
MEMORY=$((10#$MEMORY)); NATIVE_PERIODS=$((10#$NATIVE_PERIODS))
((RUN_MS >= 1000 && RUN_MS <= 600000 && VOICES >= 1 && VOICES <= 4096 &&
   THREADS >= 1 && THREADS <= 64 && MEMORY <= 4096 && NATIVE_PERIODS >= 128)) || {
    printf 'settings out of range: ms=1000..600000 voices=1..4096 threads=1..64 memory=0..4096 native_periods>=128\n' >&2
    exit 2
}
for executable in "$TEST" "$STATS" "$NATIVE"; do
    [[ -x "$executable" ]] || { printf 'missing executable: %s\n' "$executable" >&2; exit 1; }
done
for executable in "$WINE_COMMAND" timeout chrt; do command -v "$executable" >/dev/null; done

LOG_DIR="${SIDEALSA_AUDIO_LOAD_LOG_DIR:-$ROOT/target/audio-load/$(date +%Y%m%d-%H%M%S)}"
mkdir -p -- "$LOG_DIR"
printf 'audio_load_logs=%s\n' "$LOG_DIR"
printf 'audio_load_signal=output0_sine_peak_0.03125_plus_0.25_pulses input4_internal_digital_return\n'
RUN_TIMEOUT=$((RUN_MS * 2 / 1000 + 30))
NATIVE_TIMEOUT=$((NATIVE_PERIODS / 750 + 10))

stat_value() {
    local pattern="(^|[[:space:]])${2}=(-?[0-9]+)"
    [[ "$1" =~ $pattern ]] || return 1
    printf '%s\n' "${BASH_REMATCH[2]}"
}

read_stats() {
    timeout 3s "$STATS" --socket "$SOCKET" --samples 1 --interval-ms 0
}

measure_native() {
    local log=$1 output minimum maximum count lost
    output="$(timeout --kill-after=2s "${NATIVE_TIMEOUT}s" chrt -f 46 "$NATIVE" \
        --socket "$SOCKET" --periods "$NATIVE_PERIODS" --output-channel 0 --input-channel 4 2>&1)" || {
        printf '%s\n' "$output" > "$log"
        printf 'native reference failed: %s\n' "$log" >&2
        return 1
    }
    printf '%s\n' "$output" > "$log"
    minimum="$(stat_value "$output" loopback_min_frames)"
    maximum="$(stat_value "$output" loopback_max_frames)"
    count="$(stat_value "$output" loopback_measurements)"
    lost="$(stat_value "$output" loopback_lost_pulses)"
    ((count >= 2 && lost == 0 && minimum == maximum)) || return 1
    NATIVE_PHASE=$minimum
}

initial="$(read_stats)"
printf '%s\n' "$initial" > "$LOG_DIR/initial-stats.log"
daemon_pid="$(stat_value "$initial" daemon_pid)"
generation="$(stat_value "$initial" generation)"
failed=0

# No daemon/hardware restart and no external CPU burner. Workers belong to each probe.
for case_name in "${CASES[@]}"; do
    voices=1; threads=0; memory=0
    case "$case_name" in
        pulse_only) voices=0 ;;
        dsp) voices=$VOICES ;;
        workers) threads=$THREADS; memory=$MEMORY ;;
        combined) voices=$VOICES; threads=$THREADS; memory=$MEMORY ;;
    esac
    measure_native "$LOG_DIR/$case_name-native-before.log"
    before_phase=$NATIVE_PHASE
    before="$(read_stats)"
    [[ "$(stat_value "$before" daemon_pid)" == "$daemon_pid" &&
       "$(stat_value "$before" generation)" == "$generation" ]] || {
        printf 'hardware identity changed; refusing cross-generation comparison\n' >&2; exit 1;
    }
    printf '%s\n' "$before" > "$LOG_DIR/$case_name-stats-before.log"
    printf 'audio_load_case=%s voices=%s workers=%s memory_mib=%s native_before=%s\n' \
        "$case_name" "$voices" "$threads" "$memory" "$before_phase"
    status=0
    timeout --kill-after=3s "${RUN_TIMEOUT}s" env \
        -u SIDEALSA_ASIO_EXPECTED_LOOPBACK_FRAMES -u SIDEALSA_ASIO_PROBE_LIFECYCLE \
        -u SIDEALSA_ASIO_CRASH_AFTER_START -u SIDEALSA_ASIO_PROBE_SINE_SELF_TEST \
        SIDEALSA_SOCKET="$SOCKET" SIDEALSA_ASIO_PROBE_MS="$RUN_MS" \
        SIDEALSA_ASIO_PROBE_SINE_VOICES="$voices" SIDEALSA_ASIO_PROBE_LOOPBACK=1 \
        SIDEALSA_ASIO_PROBE_STRESS_THREADS="$threads" \
        SIDEALSA_ASIO_PROBE_STRESS_MEMORY_MIB="$memory" \
        SIDEALSA_ASIO_PROBE_CALLBACK_WORK_US=0 SIDEALSA_ASIO_PROBE_BENCHMARK=1 \
        SIDEALSA_ASIO_PROBE_HEARTBEAT_MS=10 SIDEALSA_ASIO_PROBE_RT_PRIORITY=0 \
        WINEDEBUG=-all WINEDLLPATH="$DLL_DIR" \
        "$WINE_COMMAND" "$TEST" > "$LOG_DIR/$case_name-asio.log" 2>&1 || status=$?
    printf 'asio_exit_status=%s\n' "$status" > "$LOG_DIR/$case_name-status.log"
    if ((status != 0)) || ! grep -Fq '[asio-probe] PASS' "$LOG_DIR/$case_name-asio.log"; then
        failed=1
    fi
    if ((voices > 0)) && ! grep -Fq "[asio-probe] sine config: voices=$voices " "$LOG_DIR/$case_name-asio.log"; then
        printf 'case=%s did not enable sine DSP; rebuild the probe\n' "$case_name" >&2
        failed=1
    fi
    legs=0
    stages=0
    declare -A seen_stages=()
    pattern='(first|second) loopback: .* min=([0-9]+) max=([0-9]+)'
    stage_pattern='\[asio-probe\] (first|second) sine stage: stage=(warm|loaded|cool) '
    while IFS= read -r line; do
        if [[ "$line" =~ $pattern ]]; then
            leg=${BASH_REMATCH[1]}; minimum=${BASH_REMATCH[2]}; maximum=${BASH_REMATCH[3]}
            legs=$((legs + 1))
            printf 'case=%s leg=%s asio_min=%s asio_max=%s native_before=%s\n' \
                "$case_name" "$leg" "$minimum" "$maximum" "$before_phase"
            ((minimum == before_phase && maximum == before_phase)) || failed=1
        fi
        if [[ "$line" =~ $stage_pattern ]]; then
            leg=${BASH_REMATCH[1]}; stage=${BASH_REMATCH[2]}
            stage_id="$leg/$stage"
            [[ -z "${seen_stages[$stage_id]+seen}" ]] || failed=1
            seen_stages[$stage_id]=1
            stages=$((stages + 1))
            stage_voices="$(stat_value "$line" voices)"
            callbacks="$(stat_value "$line" count)"
            observations="$(stat_value "$line" loopback_count)"
            operations="$(stat_value "$line" voice_samples)"
            overruns="$(stat_value "$line" period_overruns)"
            worker_units="$(stat_value "$line" worker_units)"
            stage_min="$(stat_value "$line" loopback_min)"
            stage_max="$(stat_value "$line" loopback_max)"
            expected_voices=1
            [[ "$stage" != loaded ]] || expected_voices=$voices
            ((stage_voices == expected_voices && callbacks > 0 && observations > 0 &&
              operations == callbacks * expected_voices * 64 && overruns == 0 &&
              stage_min == before_phase && stage_max == before_phase)) || failed=1
            if [[ "$stage" == loaded ]] && ((threads > 0 && worker_units <= 0)); then
                printf 'case=%s leg=%s did not execute background load\n' "$case_name" "$leg" >&2
                failed=1
            fi
        fi
    done < "$LOG_DIR/$case_name-asio.log"
    ((legs == 2)) || failed=1
    if ((voices > 0)); then
        ((stages == 6 && ${#seen_stages[@]} == 6)) || failed=1
    else
        ((stages == 0)) || failed=1
    fi
    measure_native "$LOG_DIR/$case_name-native-after.log"
    after_phase=$NATIVE_PHASE
    after="$(read_stats)"
    printf '%s\n' "$after" > "$LOG_DIR/$case_name-stats-after.log"
    [[ "$(stat_value "$after" daemon_pid)" == "$daemon_pid" &&
       "$(stat_value "$after" generation)" == "$generation" ]] || {
        printf 'hardware identity changed during %s\n' "$case_name" >&2; exit 1;
    }
    printf 'audio_load_case=%s asio_exit=%s native_after=%s common_shift_frames=%s\n' \
        "$case_name" "$status" "$after_phase" "$((after_phase - before_phase))"
    ((after_phase == before_phase)) || failed=1
    for key in pro client core hw_playback hw_capture timeline_resets shared_underruns shared_overruns; do
        before_value="$(stat_value "$before" "$key")"
        after_value="$(stat_value "$after" "$key")"
        delta=$((after_value - before_value))
        printf 'case=%s %s_delta=%s\n' "$case_name" "$key" "$delta"
        ((delta == 0)) || failed=1
    done
done

if ((failed)); then
    printf 'FAIL: inspect per-stage timing, loopback ranges and counter deltas in %s\n' "$LOG_DIR" >&2
    exit 1
fi
printf 'PASS: same-process audio-load cases held phase with zero miss/reset deltas; logs=%s\n' "$LOG_DIR"
