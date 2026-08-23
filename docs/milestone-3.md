# Milestone 3: Local Fake PRO Client

`DuplexEngine::run_pro` adds an in-process PRO path without changing physical
ALSA ownership. Each completed capture period waits for a new hardware playback
target and publishes that target once. If playback advanced by more than one
sequence, capture skips forward rather than publishing stale work. A genuine
hardware-generation change may also advance capture to playback, but never
moves capture backward to a duplicate sequence. The wait depends only on the
hardware playback worker, never on a client. A futex notification keeps the
capture worker off the run queue while it waits, so it cannot starve the lower
priority PRO callback.
The playback worker may sleep on a bounded ready event while the
hardware queue drains. Its wait budget comes from current ALSA availability and
shrinks to zero when the worker is late; one quarter-period remains reserved for
fallback selection and the hardware write.

Each audio block carries a sequence number. Playback consumes only its exact
sequence. Missing blocks produce one zero-filled fallback period and stale
blocks are discarded. Client-owned PRO playback misses count
`pro_deadline_misses`. They do not call ALSA recovery or reset the hardware
timeline.

The playback-ready eventfd is only a wake hint. Playback repeatedly checks the
exact requested sequence until one absolute deadline; stale and future wakes do
not end the wait. The daemon publishes its current playback sequence as an
authoritative watermark. PRO clients discard only older capture blocks and use
the oldest target that remains valid. SHARED capture clients retain ordered
FIFO delivery.

`device.pro_latency_periods` configures PRO output lookahead. The direct
pipeline supports zero lead for measured low-latency experiments; zero relies
only on the bounded hardware preparation budget and therefore requires timer
scheduling plus a hardware buffer of at least two periods. Higher values trade
whole periods of latency for more client margin. Maximum value is `7`, limited
by the fixed shared-memory ring. Reference profile remains at `1` until
zero-lead operation passes live deadline testing.

`device.realtime = true` is default. The playback worker runs at
`device.realtime_priority`; capture runs one level lower, clamped to priority
`1`. The reference profile therefore uses playback `88` and capture `87`.
Setting realtime scheduling requires appropriate process privileges.
`device.pro_realtime_priority` controls the ASIO callback. When omitted it is
derived as two below the hardware priority.

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
