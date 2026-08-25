# ASIO Frontend

`sidealsa-asio` provides an x86_64 Wine/Proton ASIO adapter. Rust owns the
SideALSA PRO client, double-buffered float32 host buffers, worker lifecycle,
sequence handling, and callback dispatch. The C shim supplies the Wine COM,
registration, DLL ABI, and `CreateThread` bridge required for Wine TEB setup.

The adapter expects the daemon profile to expose a `64`-frame logical period.
The reference E1x2 profile uses physical ALSA Q32, aggregates transfers into
Q64 client cycles, and keeps `buffer_size = 192` for scheduling margin. Linked
startup primes 108 frames: Q64 output, one Q32 hardware period, and 12 frames
for the 250 us client handoff.

ASIO input latency reports one operational period. Output latency reports at
least one period, or the configured PRO lookahead when larger. These values do
not include calibrated USB, firmware, converter, or device-loopback delay. The
192-frame ALSA value is ring capacity, not queued ASIO software latency.

PRO duplex clients use cycle notifications for a prepared-buffer pipeline. The
playback worker defines the initial sequence target. Synchronized duplex start
starts both directions together, and capture advances one sequence for each
completed period. A real hardware-generation change rebases it to the
playback target. The client may publish playback for
that target any time before the hardware writer consumes it. The writer prepares
output before its ALSA wait and may block on `playback_ready` only for the budget
remaining before the hardware reserve. Missing playback then gets one
zero-filled fallback period, while stale playback is discarded and cannot
affect later sequences. The ALSA write deadline and queue guard never move for
the client. PRO session starts and deadline misses never request a hardware
restart.

Protocol v12 carries the playback-ready eventfd, timing diagnostics, physical
hardware period, linked-start phase calibration result, and independent SHARED
buffer size. Shared-memory v7 adds the daemon's authoritative playback and
activation watermarks plus the shared-capture discontinuity counter.
Shared-memory slot state and sequence ownership remain authoritative for
whether a playback block is ready.
The client chooses the oldest capture target not older than that watermark, so
a newly published future block cannot displace an exact block the daemon still
needs. Sequence gaps advance sample position and double-buffer parity before
callback dispatch.
Playback keeps its original sequence; the daemon discards it if its exact
deadline has already passed.

`device.linked_phase_max_attempts` optionally calibrates linked zero-lead
startup before the control socket opens. Each attempt runs two warmup and four
measured silence cycles. Each cycle reads synchronized playback occupancy after
the capture block, predicts the normal writer's wait and reserve with a
four-frame processing margin, then writes silence immediately without draining
toward an underrun. At least three of four
measurements must fit within half a physical period while retaining half a
physical period of queued playback. A rejected attempt drops, prepares, primes,
relinks, dithers, and restarts the hardware. These intentional starts increment
generation, timeline-reset, and phase-rebase counters, but not hardware-XRUN
counters. Exhaustion uses the final running phase instead of stopping the
daemon. This timing score classifies starts; analog loopback remains the latency
authority. Linked XRUN recovery repeats calibration before client callbacks
resume. Warmup and measured maintenance transfers do not advance
`sample_position`, playback/capture positions, or `periods_processed`;
generation and reset counters identify the discontinuity.

The E1x2 reference profile disables startup calibration. An earlier runtime
rebase experiment produced 18 intentional rebases and 16 genuine capture XRUNs
during a live Discord and PRO run, showing that client-triggered linked restarts
are not safe on this device. That policy was removed. An armed deadline miss
writes one exact-sequence silence fallback, discards any stale late block, and
resumes with the next exact sequence while hardware remains continuous.

The reference profile keeps ALSA hardware `buffer_size = 192` and gives SHARED
clients an independent `shared_buffer_size = 256`. E1x2 experiments with
delayed hardware writes fail even with a 32-frame guard, so SHARED capacity does
not alter the physical queue.

## Build

```text
cmake -S crates/sidealsa-asio -B build-asio -DCMAKE_BUILD_TYPE=Release
cmake --build build-asio --target sidealsa-asio sidealsa-asio-probe
```

Outputs include `sidealsa-asio64.dll`, `sidealsa-asio64.dll.so`, and the
`sidealsa-asio-probe.exe` host. The build tree also contains Wine's
`x86_64-windows` and `x86_64-unix` lookup layout.

## Probe

Register the PE half in a test Wine prefix, then run with the Unix half on
`WINEDLLPATH`:

```text
cp build-asio/sidealsa-asio64.dll "$WINEPREFIX/drive_c/windows/system32/"
WINEDLLPATH="$PWD/build-asio" wine regsvr32 /s sidealsa-asio64.dll
SIDEALSA_SOCKET=/tmp/sidealsad.sock \
WINEDLLPATH="$PWD/build-asio" \
"$PWD/build-asio/sidealsa-asio-probe.exe"
```

The probe checks COM contracts, channel counts, `64`-frame buffer negotiation,
S32-to-float channel metadata, buffer pointers, restart, callback flow, and
that callback stack addresses belong to the Wine-created worker thread.
It returns `77` when daemon connection is unavailable.

ASIO begins with the first playback-clock target and publishes playback under
that exact sequence. The E1x2 profile keeps one safe engine period. The bounded
ready barrier lets the lower-priority callback run while ALSA drains queued
audio. An exact-sequence PRO event completes the handoff immediately, then the
daemon commits the prepared block when ALSA reaches its write target. The daemon
waits only for the part of the absolute `250 us` capture-to-PRO deadline that
remains; expiry prepares silence. There is no fixed scheduler sleep and the
client cannot stall the hardware timeline indefinitely. The E1x2 miss policy
keeps hardware continuous and resumes only the next exact sequence.

The ASIO frontend owns its callback thread and attempts to raise it to the
profile's `device.pro_realtime_priority`. When omitted, it defaults to two below
the hardware worker; the reference profile therefore uses `86` below capture
`87` and playback `88`. It continues with normal scheduling and increments
`pro_realtime_failures` when the Wine process lacks realtime rights. Native ALSA
clients retain application-owned scheduling because the plugin cannot safely
promote arbitrary host threads. The ASIO worker enters realtime before sending
`START`, so an active PRO session never begins on an unpromoted callback thread.

Poll live counters from another control connection while ASIO owns PRO:

```text
cargo run --release -p sidealsa-cli --bin sidealsa-stats -- --socket /tmp/sidealsad.sock
```

## Verification

- E1x2 direct engine: `7500` periods at `48 kHz`, `64/192`, zero hardware XRUNs.
- A live Wine probe completed `3337` callbacks during a five-second run with
  zero hardware XRUNs and timeline resets.
- Prepared playback misses are reported by `pro_deadline_misses`. The bounded
  daemon barrier preserves the hardware deadline, so `pro_core_deadline_misses`
  remains zero.
- Five consecutive native PRO runs completed `750` periods each with zero new
  deadline misses, hardware XRUNs, or timeline resets. Injecting a `2 ms` delay
  every 16th sequence produced 99 client deadline misses over 1500 periods,
  while hardware XRUNs, core misses, and timeline resets remained zero. The
  next normal run resumed with zero new misses.
- With live Discord WebRTC playback and capture plus a Sunshine playback stream,
  one normal-priority native PRO run completed 7500 periods with one client
  deadline miss and no core miss, hardware XRUN, or timeline reset. One Wine
  probe then completed 750 callbacks, and five consecutive restart runs
  completed 749, 749, 749, 750, and 749 callbacks. Those ASIO runs added no PRO
  deadline misses, shared misses, hardware XRUNs, or timeline resets. A longer
  probe completed two 15-second start legs and reported 11260 callbacks after
  its internal restart, again with no counter changes.
- Windows RTL Utility measurements ranged from `7.750 ms` to `8.375 ms`
  (`372` to `402` frames) at 48 kHz/Q64 from ASIO output 1 to the E1x2 capture
  channel 5 device-loopback path. A monitored five-second interval added no
  deadline miss, hardware XRUN, or timeline reset. The 30-frame spread is below
  one Q64 cycle and does not coincide with a hardware-timeline discontinuity.
  This is a device-loopback measurement, not an analog converter-loopback
  measurement.
- Removing the 12-frame handoff headroom reduced linked startup priming from 108
  to 96 frames and reduced the observed playback-delay maximum from 204 to 192
  frames. The change was rejected: a delayed-client stress run followed by a
  normal start caused one linked playback XRUN, one capture XRUN, and two
  generation changes. Restoring 108-frame priming returned the same stress and
  recovery tests to zero hardware XRUNs and timeline resets.
- Client diagnostics expose expired capture blocks, playback publication
  failures, realtime promotion failures, callback overruns, and maximum callback
  duration without logging from the audio thread.
- Runtime PRO rebasing is not exposed. Startup phase calibration remains
  configurable but is disabled in the E1x2 profile. Deadline misses never
  request a hardware restart.

## Limitations

- x86_64 only; no WoW64 frontend.
- Fixed sample rate and buffer size; no runtime rate switching.
- Float32 ASIO buffers converted to physical S32_LE.
- `GetLatencies` does not yet include profile-calibrated device latency.
- No control panel.
- Probe requires a running `sidealsad` and physical hardware.
