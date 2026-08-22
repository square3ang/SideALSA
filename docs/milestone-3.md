# Milestone 3: Local Fake PRO Client

`DuplexEngine::run_pro` adds an in-process PRO path without changing physical
ALSA ownership. Capture and playback workers communicate with a separate PRO
callback thread through fixed-capacity SPSC rings.

Each audio block carries a sequence number. Playback consumes only its exact
sequence. Missing or stale blocks produce zero-filled fallback audio and count
`pro_core_deadline_misses`. Client-owned PRO playback misses count
`pro_deadline_misses`. Neither condition calls ALSA recovery or resets the
hardware timeline.

`device.pro_latency_periods` configures fixed PRO output lookahead. `0` keeps
zero-period internal latency. Each additional period gives callback one more
period to publish output before playback consumes it. Maximum value is `7`,
limited by fixed eight-slot PRO ring. Reference profile uses `1` period.

`device.realtime = true` is default. Engine worker parent enters `SCHED_FIFO`
with `device.realtime_priority` before spawning capture, callback, and playback
workers. Setting realtime scheduling requires appropriate process privileges.

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

Delay callback every 16 periods by 2 ms:

```text
target/release/sidealsa-pro-test --profile profiles/topping-e1x2.toml --periods 15000 --delay-ms 2 --delay-every 16
```

Expected:

- `pro_core_deadline_misses > 0`
- `hw_playback_xruns=0`
- `hw_capture_xruns=0`
- `generation=0`
- `timeline_resets=0`
- `periods_processed=15000`

## Limitations

- PRO client is in-process only.
- No Unix socket, shared memory, client ownership, or crash recovery protocol.
- No SHARED path or mixer.
- Fake PRO copies capture samples into playback samples.
- Missing PRO output uses silence.
