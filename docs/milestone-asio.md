# ASIO Frontend

`sidealsa-asio` provides an x86_64 Wine/Proton ASIO adapter. Rust owns the
SideALSA PRO client, double-buffered float32 host buffers, worker lifecycle,
sequence handling, and callback dispatch. The C shim supplies the Wine COM,
registration, DLL ABI, and `CreateThread` bridge required for Wine TEB setup.

The adapter expects the daemon profile to expose a `64`-frame logical period.
The reference E1x2 profile uses physical ALSA Q32, aggregates transfers into
Q64 client cycles, and keeps `buffer_size = 192`. Linked startup primes 172
frames: one Q64 capture interval, one Q32 physical write reserve, two Q32
refill-headroom periods, and the 12 frames consumed by the 250 us client
handoff.

ASIO reports 64 input frames and 64 output frames for the reference profile.
These values do not include USB, firmware, converter, or analog device-loopback
delay. The 192-frame ALSA value is ring capacity, not queued ASIO software
latency.

PRO duplex clients use a zero-lead pipeline. Hardware capture sequence N is
published as playback target N while SHARED capture retains hardware sequence
N. The current block is consumed after an absolute `pro_handoff_us` deadline
that starts when PRO capture publication completes. SHARED capture routing is
deferred until after that handoff. The hardware loop writes the complete Q64
block with a Q32 playback guard. Missing playback gets one zero-filled fallback
period; stale playback cannot affect later sequences. PRO session starts and
deadline misses never request a hardware restart.

Protocol v13 carries the playback-ready eventfd, timing diagnostics, physical
hardware period, effective PRO output latency, linked-start phase calibration
result, and independent SHARED buffer size. Shared-memory v8 carries the
daemon's authoritative playback and activation watermarks plus the
shared-capture discontinuity counter, playback publication timestamps, and
hardware timeline generation.
Shared-memory slot state and sequence ownership remain authoritative for
whether a playback block is ready.
The client chooses the oldest capture target not older than that watermark, so
a newly published future block cannot displace an exact block the daemon still
needs. Sequence gaps advance sample position and double-buffer parity before
callback dispatch.
Playback keeps its original sequence; the daemon discards it if its exact
deadline has already passed.

ASIO `Stop` gates host callbacks without stopping the SideALSA stream. The Wine
worker continues consuming capture and publishes exact-sequence silence. Stop
returns after the daemon playback watermark has advanced beyond the final active
block, so no active block remains pending in the software pipeline. A later
`Start` resets ASIO sample position and double-buffer parity, then reuses the
same worker and PRO activation. `DisposeBuffers` or driver close performs the
actual stream stop, joins the worker, and releases exclusive PRO ownership.
Control-socket operations use a one-second read/write timeout so worker teardown
cannot wait forever on a stalled daemon control plane.

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
are not safe on this device. A later startup-only run exhausted 32 attempts,
added 16 capture XRUNs and 32 timeline resets, and never met the phase target.
That candidate was also rejected. An armed deadline miss writes one
exact-sequence silence fallback, discards any stale late block, and resumes with
the next exact sequence while hardware remains continuous.

The reference profile keeps ALSA hardware `buffer_size = 192`, a 32-frame linked
playback guard, and an independent `shared_buffer_size = 512`. SHARED capacity
does not alter the physical queue.

## Build

```text
cmake -S crates/sidealsa-asio -B build-asio -DCMAKE_BUILD_TYPE=Release
cmake --build build-asio --target \
  sidealsa-asio sidealsa-asio-probe sidealsa-asio-loopback-test
```

Outputs include `sidealsa-asio64.dll`, `sidealsa-asio64.dll.so`,
`sidealsa-asio-probe.exe`, and `sidealsa-asio-loopback-test.exe`. The build tree
also contains Wine's `x86_64-windows` and `x86_64-unix` lookup layout.

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
64-frame reported output latency, S32-to-float channel metadata, buffer
pointers, callback flow, callback quiescence after `Stop`, and reuse of the same
Wine-created worker thread after the next `Start`.
It returns `77` when daemon connection is unavailable.

With playback channel 0 physically looped to capture channel 4, run the strict
analog test and abrupt-process reacquisition harness:

```text
WINELOADER=wine WINEDLLPATH="$PWD/build-asio" \
  build-asio/sidealsa-asio-loopback-test.exe
scripts/test-asio-reacquire.sh
```

The loopback executable fails on a lost pulse, variable phase, callback-index
error, sample-position error, or a difference between its two Start legs. Pulse
coordinates use the ASIO callback's sample position rather than callback count.
Both testers stop emitting and drain every pending pulse before validation. The
reacquisition harness records a baseline, terminates a streaming process without
`Stop` or buffer disposal, requires stable playback after reconnect, compares it
with an immediately following native PRO loopback, and checks that the daemon
PID, hardware XRUN, generation, and timeline-reset counters did not change.
This avoids treating an explicit-feedback hardware phase change between
processes as ASIO process-ahead. The harness verifies the socket peer with
`SO_PEERCRED`; `SIDEALSA_DAEMON_PID` is an optional additional assertion.
It models each measurement as `raw loopback = common SideALSA/hardware path +
ASIO frontend residual`, reports the paired `ASIO - native` residual, and
reports the native baseline-to-reacquisition change separately as a common-path
shift. Only the frontend residual participates in ASIO-specific acceptance; the
common-path shift remains a core/hardware stability finding. The native
reference uses the ASIO worker's default realtime priority of `86`.

ASIO begins with the first playback-clock target and publishes playback under
that exact sequence. The E1x2 profile keeps zero process-ahead blocks, one Q32
playback guard, and a real 250 us client handoff. The daemon never waits past
that bounded cutoff. It samples the exact shared-memory sequence once,
substitutes silence when absent, and resumes only with the next exact sequence.

The ASIO frontend owns its callback thread and attempts to raise it to the
profile's `device.pro_realtime_priority`. When omitted, it defaults to two below
the hardware worker; the reference profile therefore uses `86` below the linked
hardware worker at `88`. It continues with normal scheduling and increments
`pro_realtime_failures` when the Wine process lacks realtime rights. Native ALSA
clients retain application-owned scheduling because the plugin cannot safely
promote arbitrary host threads. The ASIO worker enters realtime before sending
`START`, so an active PRO session never begins on an unpromoted callback thread.

Poll live counters from another control connection while ASIO owns PRO:

```text
cargo run --release -p sidealsa-cli --bin sidealsa-stats -- --socket /tmp/sidealsad.sock
```

## Verification

- The complete release workspace passes tests and clippy with warnings denied.
- Passive USB tracing proved that snd-usb playback OUT URBs can drain and xHCI
  can reseed the endpoint schedule while ALSA remains `RUNNING`. One trace
  skipped two USB microframes without an ALSA XRUN; capture and feedback stayed
  continuous. Heavy usbmon tracing itself can cause normal XRUNs and is not used
  for routine acceptance.
- The old shared queue/client cutoff reached 22 frames of total playback delay.
  Raising that coupled cutoff to 64 frames caused 5010 PRO misses over 10018
  playback blocks, so that design was rejected.
- Q32 staging plus the 64-frame refill guard raised observed minimum total delay
  to 54 frames, kept `playback_low_watermarks=0`, and held
  `capture_to_playback_write_min_nanos` above 252000 ns.
- A strict native run measured all 14 pulses at exactly 318 frames with zero
  lost pulse, PRO miss, hardware XRUN, or timeline reset.
- On the same hardware start, ASIO measured 382 frames in both Start legs. The
  64-frame difference from the native tracker is its callback-timeline
  convention, not another physical buffer.
- The abrupt-process harness retained 382 frames after forced termination and
  reacquisition. The forced death added one isolated client miss; generation,
  hardware-XRUN, and timeline-reset counters remained unchanged.
- A separate warm hardware start retained 425 frames through forced crash and
  reacquisition. A ten-worker OpenSSL run and two 10-second ASIO legs completed
  separately at that phase; this is not evidence for concurrent-load stability.
- The hardened current probe exposed a recurring physical phase failure that
  the older finite checks missed. One fresh native run moved from 288 to 312
  frames inside the same process with zero client miss, hardware XRUN, or reset.
  ASIO then held 376 frames across both Start legs, its expected fixed Q64 above
  native, but forced-process reacquisition later moved from 376 to 412 frames
  while every SideALSA and ALSA failure counter remained unchanged.
- Genuine ten-process OpenSSL overlap was verified by process ID before testing.
  A 160-frame maximal refill guard held two 10-second ASIO legs at 370 frames,
  but crash/reacquisition under the same load moved 370 to 394 frames. A later
  unloaded run moved 424 to 394 frames. A 96-frame guard also failed to prevent
  a load-related transition. Those one-period-lead guard experiments did not
  solve the issue. The current zero-lead profile uses a 32-frame guaranteed
  guard and a 172-frame startup prime; refill is scheduled two Q32 periods early
  to cover ALSA availability granularity and USB-driver transfer advancement.
- USB descriptors put playback and capture on the same internal UAC2 clock
  source, but playback uses an asynchronous explicit-feedback endpoint with
  125-us packets. Earlier xHCI traces showed that endpoint reseeding without an
  ALSA XRUN. The observed 24, 30, 36, 37, and 54-frame movements are sub-Q64 and
  cannot be produced by SideALSA's whole-period slot or sequence mapping.
- Earlier Discord/PipeWire and delayed-PRO/SHARED tests predate the zero-lead
  revision. They established miss isolation but are not latency acceptance data
  for the current revision. The rejected 64-frame/two-Q32 startup prime remains
  rejected because it caused real Discord activation XRUNs; zero-lead startup
  now primes 172 frames so the Q32 guard remains after refill advancement and
  the client handoff.
- Client diagnostics expose expired capture blocks, playback publication
  failures, realtime promotion failures, callback overruns, and maximum callback
  duration without logging from the audio thread.
- Runtime PRO rebasing is not exposed. Startup phase calibration remains
  configurable but is disabled in the E1x2 profile. Deadline misses never
  request a hardware restart.
- Direct ALSA Q64/B192, zero-lead native PRO, and zero-lead ASIO each measured
  exactly 351 frames on the canonical-profile hardware start. The 30000-period
  native run detected all 469 pulses with zero deadline miss, hardware XRUN, or
  timeline reset.
- Forced process termination moved one ASIO run from 351 to 405 frames and a
  later run from 351 to 388 frames. Immediate native PRO measurements were
  exactly 405 and 388 frames respectively. ASIO therefore retained native
  parity while their common SideALSA/hardware path changed; hardware-XRUN,
  generation, and timeline-reset counters remained unchanged. This did not
  establish a hardware-only cause.
- The frontend-normalized crash/reacquisition harness measured raw ASIO/native
  pairs of 400/400 frames before and after forced termination. Both frontend
  residuals were zero, with no common-path shift, hardware XRUN, or timeline
  reset in that run.
- Before the refill-order fix, a zero-lead 12000-period delayed-SHARED run
  retained all 188 PRO loopback
  pulses at exactly 436 frames with zero PRO miss, hardware XRUN, or timeline
  reset. A separate 8000-period run with simultaneous PipeWire playback and
  capture retained all 125 pulses at the same phase with the same zero failure
  deltas.
- A minimum-valid B128 zero-lead experiment reduced the observed ALSA USB-driver
  queue from about 186-192 frames to 120 frames, but produced 311 playback XRUNs,
  311 capture XRUNs, and 622 timeline resets in six seconds. Direct ALSA B128
  also repeatedly returned no ready period before settling near 405 loopback
  frames. B128 is rejected; B192 remains the stable reference buffer.
- The zero-lead refill-order fix schedules ALSA writes two Q32 periods earlier,
  starts the absolute PRO handoff before SHARED playback preparation, and moves
  SHARED capture publication after the hardware write. On one hardware start,
  repeated native and ASIO runs held 349 frames. On a fresh start, native held
  362 frames before and during simultaneous PipeWire playback/capture; all 94
  pulses were detected with zero failure deltas. ASIO then held the same 362
  frames across both Start legs, forced process termination, and reacquisition,
  with zero ASIO-native residual and zero common-path shift.

## Limitations

- x86_64 only; no WoW64 frontend.
- Fixed sample rate and buffer size; no runtime rate switching.
- Float32 ASIO buffers converted to physical S32_LE.
- `GetLatencies` reports the 64-frame input and 64-frame software output
  path, but not USB, converter, cable-loopback, or profile-calibrated latency.
- Earlier builds showed sub-Q64 common-path shifts while both ALSA streams
  remained `RUNNING`. Shallow/late SideALSA refill and SHARED work ordering were
  confirmed software triggers, so these shifts must not be labeled as
  hardware-only. The crash/reacquisition harness checks each leg for stable
  phase, compares ASIO with an immediately following native PRO run, and reports
  any remaining common-path shift separately. The refill-order fix still needs
  a longer multi-hour stability run.
- The zero-lead revision has not yet repeated the full Discord playback,
  microphone, and screen-sharing soak.
- Concurrent CPU pressure and delayed SHARED traffic have not yet been rerun
  alongside the zero-lead ASIO probe.
- Hardware-engine stop is non-draining. A finite `max_periods` run or daemon
  shutdown stops the PCM without draining queued playback.
- While buffers exist, ASIO retains exclusive PRO ownership across `Stop`.
  Dispose buffers or close the driver before opening another PRO client.
- No control panel.
- Probe requires a running `sidealsad` and physical hardware.
