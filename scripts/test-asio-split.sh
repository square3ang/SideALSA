#!/usr/bin/env bash
# Hardware-free, opt-in integration. No installed daemon, registry, or service changes.
set -euo pipefail

case "${1:---help}" in
    --help|-h)
        printf '%s\n' 'Usage: bash scripts/test-asio-split.sh [--help|--build-only|--run]' \
            'Default: help only. --build-only compiles without launching Wine or a server.' \
            '--run starts a synthetic 2in/2out 48k Q64 B256 server and a two-COM-object Wine probe.' \
            'No real audio hardware is opened. Wine initialization may start Wine services.' \
            'Private socket/prefix and build artifacts are kept under TMPDIR (default /tmp/opencode).' \
            'Existing JACK, audio services, real daemons, and other Wine sessions are irrelevant and untouched.'
        exit 0 ;;
    --build-only|--run) mode=$1 ;;
    *) printf 'Unknown option: %s\n' "$1" >&2; exit 2 ;;
esac
[[ $# == 1 ]] || { printf 'Expected one option\n' >&2; exit 2; }
root=$(realpath -- "$(dirname -- "${BASH_SOURCE[0]}")/..")
export TMPDIR=${TMPDIR:-/tmp/opencode}
[[ -d "$TMPDIR" ]] || { printf 'TMPDIR must already exist\n' >&2; exit 2; }
for tool in cargo cmake winegcc winebuild timeout; do
    command -v "$tool" >/dev/null || { printf 'Missing tool: %s\n' "$tool" >&2; exit 1; }
done
work=$(mktemp -d "$TMPDIR/asio-split.XXXXXXXX")
printf 'Artifacts: %s\n' "$work"
sim_pid=
probe_pid=
cleanup() {
    local status=$?
    trap - EXIT INT TERM
    # These are our timeout supervisors, which forward signals only to their own groups.
    for pid in "$probe_pid" "$sim_pid"; do
        if [[ -n "$pid" ]]; then
            kill -TERM "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    if (( status != 0 )); then printf 'ASIO split runner FAIL (status=%s); logs: %s\n' "$status" "$work" >&2; fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
timeout -k 5s 300s cargo build --manifest-path "$root/Cargo.toml" \
    -p sidealsa-daemon --example pro-simulator --target-dir "$work/cargo"
timeout -k 5s 60s cmake -S "$root/crates/sidealsa-asio" -B "$work/asio" -DCMAKE_BUILD_TYPE=Debug
timeout -k 5s 300s cmake --build "$work/asio" --target sidealsa-asio-split-probe
if [[ "$mode" == --build-only ]]; then
    printf 'ASIO split build PASS (nothing launched)\n'
    exit 0
fi
command -v wine >/dev/null || { printf 'Missing tool: wine\n' >&2; exit 1; }
export SIDEALSA_SOCKET="$work/control.sock"
export WINEDLLPATH="$work/asio"
export WINEPREFIX="$work/wine"
export WINEDEBUG=-all
printf '%s\n' 'Running synthetic integration only. Wine init may start Wine services; no real audio is used.'
timeout -k 5s 120s "$work/cargo/debug/examples/pro-simulator" "$SIDEALSA_SOCKET" \
    >"$work/simulator.log" 2>&1 &
sim_pid=$!
for ((attempt=0; attempt<500; ++attempt)); do
    [[ -S "$SIDEALSA_SOCKET" ]] && break
    kill -0 "$sim_pid" 2>/dev/null || { printf 'Simulator exited; see simulator.log\n' >&2; exit 1; }
    sleep 0.01
done
[[ -S "$SIDEALSA_SOCKET" ]] || { printf 'Simulator socket timeout\n' >&2; exit 1; }
timeout -k 5s 90s wine "$work/asio/sidealsa-asio-split-probe.exe.so" \
    >"$work/probe.log" 2>&1 &
probe_pid=$!
if wait "$probe_pid"; then probe_pid=; else probe_pid=; exit 1; fi
# Require the explicit final marker as well as a successful process exit.
grep -qx 'ASIO split PASS' "$work/probe.log"
kill -TERM "$sim_pid"
if wait "$sim_pid"; then
    sim_pid=
else
    status=$?
    sim_pid=
    # GNU timeout returns 143 when asked to forward our normal shutdown signal.
    [[ "$status" == 143 ]] || exit "$status"
fi
grep -Eq '^pro-simulator: synthetic_periods=[1-9][0-9]* nonzero_playback=false$' "$work/simulator.log"
printf 'ASIO split runner PASS; logs: %s\n' "$work"
