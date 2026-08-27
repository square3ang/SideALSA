# Milestone 3: Local Fake PRO Client

`DuplexEngine::run_pro` adds an in-process PRO path without changing physical
ALSA ownership. The current linked implementation runs Q64 client blocks over a
Q32 physical packet cadence. Capture sequence N is published as playback target
N. The daemon provides the bounded PRO handoff, samples that exact sequence
once, and writes the Q64 block while retaining the configured playback guard. Client
readiness never controls hardware continuity.

Each audio block carries a sequence number. Playback consumes only its exact
sequence. Missing blocks produce one zero-filled fallback period and stale
blocks are discarded. Client-owned PRO playback misses count
`pro_deadline_misses`. They do not call ALSA recovery or reset the hardware
timeline.

The playback-ready eventfd is only a wake hint for diagnostics and client-side
flow. The hardware worker does not poll or wait on it. The daemon publishes its
current playback sequence as an authoritative watermark. PRO clients discard
only older capture blocks and use the oldest target that remains valid. SHARED
capture clients retain ordered FIFO delivery.

`device.pro_latency_periods` configures PRO output lookahead. The E1x2 reference
profile uses zero lead and a 216-frame startup reserve: one Q64 capture interval,
the 32-frame playback guard, three Q32 refill-headroom periods, and the 24 frames
consumed by the 500 us handoff. Its effective PRO output latency is one Q64 host
buffer. Higher values trade whole periods of latency for more client margin.
Maximum value is `7`, limited by the fixed shared-memory ring.

The reference profile also sets `linked_phase_max_attempts = 8`. Each attempt
runs one second of linked silence with the production handoff and write cadence
before client readiness. A shortened capture-to-playback write interval rejects
the hardware start while no client exists. Runtime recovery does not repeat this
startup-only qualification.

`device.realtime = true` is default. The linked hardware worker runs at
`device.realtime_priority`; the reference profile uses `48`, below the
PREEMPT_RT xHCI IRQ thread at `50` on the reference host. Setting realtime
scheduling requires appropriate process and memory-locking privileges. The
daemon locks its mappings and prefaults the hardware-thread stack before the
stream starts.
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

- The fake local client remains useful for direct engine testing, while the
  daemon path now supplies Unix-socket control and shared-memory audio.
- A client attached during initial USB feedback convergence can observe one
  startup phase transition.
- Missing PRO output uses silence.
