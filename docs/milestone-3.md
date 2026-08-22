# Milestone 3: Local Fake PRO Client

`DuplexEngine::run_pro` adds an in-process PRO path without changing physical
ALSA ownership. The playback worker is the sole PRO sequence clock. It publishes
the writable playback target before waiting for the hardware transfer point.
The capture worker attaches its completed block to that target and wakes the
client. The hardware workers never wait for a client callback.

Each audio block carries a sequence number. Playback consumes only its exact
sequence. Missing blocks produce one zero-filled fallback period and stale
blocks are discarded. Client-owned PRO playback misses count
`pro_deadline_misses`. They do not call ALSA recovery or reset the hardware
timeline.

`device.pro_latency_periods` configures additional PRO output lookahead. `0`
uses the current writable hardware period for minimum latency. Higher values
trade whole periods of latency for client scheduling margin. Maximum value is
`7`, limited by the fixed shared-memory ring. Reference profile uses `0`.

`device.realtime = true` is default. Engine worker parent enters `SCHED_FIFO`
with `device.realtime_priority` before spawning capture and playback workers.
Setting realtime scheduling requires appropriate process privileges.

## Build

```text
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p sidealsa-core
```

## Normal PRO Test

Release `hw:OTG,0` from PipeWire first. Reference profile enables realtime
scheduling internally:

```text
target/release/sidealsa-pro-test --profile profiles/topping-e1x2.toml --periods 15000
```

Expected:

- `pro_core_deadline_misses=0`
- `hw_playback_xruns=0`
- `hw_capture_xruns=0`
- `generation=0`
- `timeline_resets=0`

Compare latency settings by copying profile and changing, for example:

```toml
pro_latency_periods = 2
```

## Late PRO Test

Use the daemon client to delay playback production every 16 periods by 2 ms:

```text
target/release/sidealsa-pro-client-test --periods 15000 --delay-ms 2 --delay-every 16
```

Expected:

- `pro_deadline_misses > 0`
- `pro_core_deadline_misses=0`
- `hw_playback_xruns=0`
- `hw_capture_xruns=0`
- `generation=0`
- `timeline_resets=0`
- `periods_processed=15000`

## Limitations

- PRO client is in-process only.
- No Unix socket, shared memory, client ownership, or crash recovery protocol.
- No SHARED path or mixer.
- Fake PRO capture and playback endpoints are independent.
- Missing PRO output uses silence.
