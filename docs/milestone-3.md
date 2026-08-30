# Milestone 3: Local Fake PRO Client

`DuplexEngine::run_pro` adds an in-process PRO path without changing physical
ALSA ownership. The reference linked implementation uses the same Q64 period for
the client and physical ALSA PCM. Capture sequence N is published as playback
target N. ALSA capture availability and playback queue depth drive the cycle;
playback and capture poll readiness must both reach Q64 before transfer. There
is no playback queue-target sleep or phase calibration.

Each audio block carries a sequence number. Playback consumes only its exact
sequence. Client completion wakes the daemon through eventfd. The E1x2 profile
uses a bounded 1.0 ms handoff. The cutoff uses post-poll ALSA availability and
reserves Q16 for fallback selection, mixing, and the ALSA write. It is shortened
further after a late hardware wake. A missing block repeats the last
valid PRO period; silence is used before the first valid block or after session,
lifecycle, or hardware-generation changes. Stale blocks are discarded. The sequence still
advances exactly once, and a miss never calls ALSA recovery or resets the
hardware timeline.

The playback-ready eventfd is a bounded client-completion wakeup, not the
hardware clock. The daemon publishes its current playback sequence as an
authoritative watermark. PRO clients discard only older capture blocks and use
the oldest target that remains valid. SHARED capture clients retain ordered FIFO
delivery.

`device.pro_latency_periods` configures PRO output lookahead. The E1x2 reference
profile uses zero lead, Q64/B256 physical ALSA geometry, and a 128-frame silence
prime before the linked start. The one-Q64 latency reported to host APIs
describes their callback buffer; SideALSA inserts zero additional process-ahead
frames in the direct linked path. Higher values use the legacy lookahead paths
and trade whole periods of latency for more client margin. Maximum value is `7`,
limited by the fixed shared-memory ring.

Direct whole-period PRO requires `linked_phase_max_attempts = 0` and does not
dither or restart a healthy linked stream during startup. The former Q32
startup-qualification experiments remain documented in the ASIO history.

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

## Direct ALSA Parity

With `sidealsad` stopped and the analog loopback cable connected, run the direct
capture-to-playback loop with B256 capacity and the same Q128 startup queue:

```text
target/release/sidealsa-direct-loopback-test \
  --profile profiles/topping-e1x2.toml --periods 10000 \
  --buffer-frames 256 --start-frames 128
```

The acceptance target is no SideALSA-only whole-period displacement, not a fixed
absolute analog number. An earlier Q128 revision accumulated 523 linked hardware
XRUNs and was temporarily replaced by a diagnostic Q192 queue. After linked
startup/recovery and interrupted-poll fixes, the Q128 path completed an installed
208232-period session with all PRO, SHARED, hardware-XRUN, reset, and generation
counters at zero. That session included simultaneous 100000-period RT PRO and
delayed SHARED runs at a fixed 403 frames. Normal Wine ASIO plus a clean release
workspace build also completed without a deadline miss or hardware timeline
change. A direct B256/Q128 open measured 361 frames and the final installed
SideALSA smoke test measured 373 frames; neither contained a Q64 displacement.
Reopen and load transitions can still select different sub-Q64 common hardware
phases.

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
- Missing PRO output repeats only the last valid PRO period. Repeating arbitrary
  material can sustain a tone or DC value until exact-sequence output resumes.
- The Q128 event-driven path passed native, delayed-PRO, simultaneous-SHARED,
  Wine ASIO, and release-build stress. The full Discord/WebRTC soak remains.
