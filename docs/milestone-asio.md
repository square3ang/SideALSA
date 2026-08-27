# ASIO Frontend

`sidealsa-asio` provides an x86_64 Wine/Proton ASIO adapter. Rust owns the
SideALSA PRO client, double-buffered float32 host buffers, worker lifecycle,
sequence handling, and callback dispatch. The C shim supplies the Wine COM,
registration, DLL ABI, and `CreateThread` bridge required for Wine TEB setup.

The adapter expects the daemon profile to expose a `64`-frame logical period.
The reference E1x2 profile uses physical ALSA Q32, aggregates transfers into
Q64 client cycles, and keeps `buffer_size = 256`. Linked startup primes 216
frames: one Q64 capture interval, a 32-frame physical write reserve, three Q32
refill-headroom periods, and the 24 frames consumed by the 500 us client
handoff.

ASIO reports 64 input frames and 64 output frames for the reference profile.
These values do not include USB, firmware, converter, or analog device-loopback
delay. The 256-frame ALSA value is ring capacity, not queued ASIO software
latency.

PRO duplex clients use a zero-lead pipeline. Hardware capture sequence N is
published as playback target N while SHARED capture retains hardware sequence
N. The current block is consumed after an absolute `pro_handoff_us` deadline
that starts when PRO capture publication completes. SHARED capture routing is
deferred until after that handoff. The hardware loop writes the complete Q64
block with the configured linked playback guard. Missing playback gets one
zero-filled fallback period; stale playback cannot affect later sequences. PRO
session starts and deadline misses never request a hardware restart.
After capture, the daemon also bounds the handoff by current ALSA playback delay.
A late RT wake therefore shortens or skips the client wait before it consumes
the Q64 emergency write reserve.

Protocol v14 carries the playback-ready eventfd, timing diagnostics, physical
hardware period, effective PRO output latency, linked-start phase calibration
result, independent SHARED buffer size, and the loaded profile fingerprint.
Shared-memory v8 carries the
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
startup before the control socket opens. Each attempt runs one second of silence
at the production capture, handoff, playback-target, and write cadence. The
minimum observed capture-to-playback write interval must reach the configured
handoff less one eighth of a physical period. A shorter cycle
drops, prepares, primes, relinks, dithers, and restarts the hardware before any
client exists. Intentional retries increment generation, timeline-reset, and
phase-rebase counters, but not hardware-XRUN counters. Exhausting the configured
attempts fails daemon startup instead of exposing an unqualified final stream.
Maintenance transfers do not advance `sample_position`, playback/capture
positions, or `periods_processed`. `linked_phase_score_nanos` reports the final
attempt's minimum write interval; analog loopback remains the latency authority.

The E1x2 reference profile enables eight startup attempts. Runtime recovery does
not rerun the qualifier, and a recoverable error between qualification and first
readiness fails that startup instead of publishing an unqualified restart. An
earlier occupancy-prediction qualifier and a client-triggered runtime rebase
experiment were rejected after producing genuine capture XRUNs and repeated
timeline resets. An armed deadline miss writes one
exact-sequence silence fallback, discards any stale late block, and resumes with
the next exact sequence while hardware remains continuous.

The reference profile keeps ALSA hardware `buffer_size = 256`, a 32-frame linked
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
reference uses the ASIO worker's default realtime priority of `46`.

ASIO begins with the first playback-clock target and publishes playback under
that exact sequence. The E1x2 profile keeps zero process-ahead blocks. It uses a
Q32 packet cadence, a 32-frame playback guard, and a real 500 us client handoff.
The daemon never waits past that bounded cutoff. It samples the exact
shared-memory sequence once, substitutes silence when absent, and resumes only
with the next exact sequence.

The ASIO frontend owns its callback thread and attempts to raise it to the
profile's `device.pro_realtime_priority`. When omitted, it defaults to two below
the hardware worker; the reference profile therefore uses `46` below the linked
hardware worker at `48`. Both stay below the reference PREEMPT_RT xHCI IRQ
thread at `50`. It continues with normal scheduling and increments
`pro_realtime_failures` when the Wine process lacks realtime rights. Native ALSA
clients retain application-owned scheduling because the plugin cannot safely
promote arbitrary host threads. The ASIO worker enters realtime before sending
`START`, so an active PRO session never begins on an unpromoted callback thread.

### In-process priority benchmark

The probe can put the load inside the ASIO host process instead of relying on
unrelated system stress:

```text
SIDEALSA_ASIO_PROBE_MS=3000 \
SIDEALSA_ASIO_PROBE_BENCHMARK=1 \
SIDEALSA_ASIO_PROBE_STRESS_THREADS=24 \
SIDEALSA_ASIO_PROBE_STRESS_MEMORY_MIB=512 \
SIDEALSA_ASIO_PROBE_CALLBACK_WORK_US=350 \
SIDEALSA_ASIO_PROBE_HEARTBEAT_MS=10 \
SIDEALSA_ASIO_PROBE_RT_PRIORITY=40 \
WINEDLLPATH="$PWD/build-asio" \
  build-asio/sidealsa-asio-loopback-test.exe
```

The worker allocation is committed and touched before `Start`. The workers then
perform cache-line memory traffic in the same executable while the callback
performs bounded synthetic CPU work without allocating or logging. The probe
reports callback duration, callback scheduler policy, main-thread heartbeat
delay, and worker throughput. `SIDEALSA_ASIO_PROBE_RT_PRIORITY` lowers the
current ASIO worker from its profile-provided priority during the first callback;
it refuses to raise the worker above that configured priority. This diagnostic
override belongs to the test host and does not change the installed driver or
daemon profile.

A paired RT1/RT40/RT86 sweep ran three times per priority without restarting the
daemon or hardware. All runs used daemon PID 41551, generation 0, the
then-current 450 us handoff, a 350 us callback load, 24 host workers, and 512 MiB
of memory traffic. The probe verified the requested `SCHED_FIFO` priority in
every run.

| Priority | Callbacks | Callback max range | Heartbeat max-gap range | Worker rate range | Strict runs |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 14509 | 366.3-370.2 us | 13.174-14.058 ms | 168.110-168.433 M units/s | 3/3 |
| 40 | 14607 | 361.4-385.3 us | 13.041-17.044 ms | 168.004-168.151 M units/s | 3/3 |
| 86 | 14513 | 357.4-360.9 us | 14.003-16.026 ms | 167.495-168.121 M units/s | 3/3 |

Every run held analog loopback at exactly 362 frames with no lost or pending
pulse. Across 182224 hardware periods and 43960 PRO playback blocks, PRO client
misses, core misses, callback period overruns, SHARED under/overruns, hardware
XRUNs, realtime-promotion failures, generation changes, and timeline resets all
remained zero. RT1 was sufficient against these normal-priority host workers
because any `SCHED_FIFO` thread preempts them. RT86 produced the tightest
callback tail, while heartbeat and worker-throughput variation showed no
material host-responsiveness penalty. That benchmark originally selected RT86
below an RT88 hardware worker. Later USB-transition testing superseded that
choice because both userspace priorities were above the xHCI IRQ thread.

A 400 us callback-load calibration was too close to that 450 us zero-lead
handoff: separate fresh-start runs produced client misses at both RT40 and RT86.
Priority cannot replace callback budget. A preliminary profile-apply sweep was
also rejected as the priority comparator because each apply restarted hardware;
one such run moved analog phase from 320 to 344 frames with every SideALSA and
ALSA failure counter unchanged.

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
  solve the issue. A later zero-lead safety baseline used a 48-frame guaranteed
  guard and a 232-frame startup prime. Its reserve covered ALSA availability
  granularity, USB-driver transfer advancement, and one delayed-wakeup Q32.
- USB descriptors put playback and capture on the same internal UAC2 clock
  source, but playback uses an asynchronous explicit-feedback endpoint with
  125-us packets. Earlier xHCI traces showed that endpoint reseeding without an
  ALSA XRUN. The observed 24, 30, 36, 37, and 54-frame movements are sub-Q64 and
  cannot be produced by SideALSA's whole-period slot or sequence mapping.
- Earlier Discord/PipeWire and delayed-PRO/SHARED tests predate the zero-lead
  revision. They established miss isolation but are not latency acceptance data
  for the current revision. The rejected 64-frame/two-Q32 startup prime remains
  rejected because it caused real Discord activation XRUNs. The current
  zero-lead startup primes 216 frames so the 32-frame guard and three Q32
  refill-headroom periods remain after the client handoff.
- Client diagnostics expose expired capture blocks, playback publication
  failures, realtime promotion failures, callback overruns, and maximum callback
  duration without logging from the audio thread.
- Runtime PRO rebasing is not exposed. The E1x2 profile enables startup-only
  qualification with at most eight attempts. Deadline misses never request a
  hardware restart.
- With the former B192 profile, direct ALSA Q64, zero-lead native PRO, and
  zero-lead ASIO each measured exactly 351 frames on the then-canonical B192
  hardware start. The 30000-period
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
  frames. B128 is rejected; B192 was the stable interim buffer before the
  current B256 reference added scheduling margin.
- The zero-lead refill-order fix schedules ALSA writes two Q32 periods earlier,
  starts the absolute PRO handoff before SHARED playback preparation, and moves
  SHARED capture publication after the hardware write. On one hardware start,
  repeated native and ASIO runs held 349 frames. On a fresh start, native held
  362 frames before and during simultaneous PipeWire playback/capture; all 94
  pulses were detected with zero failure deltas. ASIO then held the same 362
  frames across both Start legs, forced process termination, and reacquisition,
  with zero ASIO-native residual and zero common-path shift.
- Before the final handoff update, the B256/guard48/450-us/SHARED7 profile ran
  simultaneous Q64 PipeWire playback and capture, a 12-worker CPU load, and both
  ASIO loopback Start legs.
  ASIO remained fixed at 345 frames with no lost or pending pulse. The daemon
  recorded 15179 PRO playback blocks with zero PRO miss, SHARED under/overrun,
  hardware XRUN, generation change, or timeline reset; PipeWire reported zero
  graph errors.
- Moving SHARED playback preparation ahead of PRO capture publication was
  rejected. The reordered candidate recorded a client miss and changed analog
  phase during strong concurrent stress. Restoring publication first completed
  four concurrent runs and 68730 PRO blocks with no miss and fixed phase, so the
  absolute handoff continues to include bounded SHARED preparation work.
- A stronger run activated all four SHARED playback and all six SHARED capture
  ports, 24 CPU workers, four concurrent 64 MiB memory-copy loops, and both ASIO
  Start legs. Over 97802 hardware periods, ASIO remained fixed at 374 frames and
  every SideALSA PRO, SHARED, hardware-XRUN, generation, and reset delta stayed
  at zero. Under that deliberately oversubscribed all-port load, normal-priority
  `pw-cat` playback feeder nodes accumulated client-local graph errors while the
  SideALSA adapter nodes and daemon remained at zero.
- A normal-priority native PRO negative control under the same pressure produced
  452 client deadline misses and lost 18 of 195 pulses without affecting the
  hardware timeline. Repeating the 12000-period test with the then-profile's
  required `chrt -f 86` scheduling detected all 188 pulses at a fixed 374
  frames, with zero PRO miss, hardware XRUN, or timeline-reset delta. Native PRO
  hosts must arrange their own realtime callback scheduling; the client library
  does not promote arbitrary application threads.
- The B256/guard48/500-us profile retains Q64 daemon and ASIO blocks. Its first
  buffered desktop revision exposed SHARED as Q256/B512 with PipeWire timer
  scheduling. The prior Q64
  desktop cadence produced one PRO client miss and 145 PipeWire playback-client
  graph errors under 24 in-process workers, 512 MiB of memory traffic, and a
  350 us callback workload; increasing only the client buffer left 75 graph
  errors. Two consecutive Q256 PipeWire duplex runs then completed 31723 PRO
  blocks with fixed 374-frame loopback and zero PRO, client, core, SHARED,
  hardware-XRUN, generation, reset, adapter-error, or client-error deltas. A
  concurrent PipeWire Pulse playback/capture run added 15877 PRO blocks with the
  same zero deltas. The observed PRO wait budget was 481.799-499.935 us and the
  maximum callback duration was 378.743 us.
- A later load-induced XRUN sweep combined an RT86 native PRO loopback,
  simultaneous Q256 PipeWire playback/capture, 12-way OpenSSL CPU saturation,
  16 GiB of tmpfs allocation, and 1080p60 uncompressed USB video capture. The
  30-second baseline completed 22565 PRO blocks and 352 fixed 356-frame pulses
  with zero PRO, SHARED, PipeWire graph, hardware-XRUN, generation, or reset
  deltas.
- Deterministic delay injection then exposed the remaining boundary. With the
  former 200-frame startup reserve, a 3 ms hardware-thread callback stall caused
  one playback XRUN in 3000 periods even when the hardware-aware handoff clamp
  was enabled. Raising startup reserve by one Q32 to 232 frames and retaining
  the clamp completed 12000 periods with a 3 ms stall every 16 periods: 750
  injected stalls, one isolated core fallback, and zero hardware XRUNs or
  timeline resets. Disabling only the clamp restored one XRUN in 3000 periods.
  A full 256-frame prime did not reduce the 48 XRUNs produced by 4 ms stalls, so
  its extra latency was rejected; 4 ms exceeds the validated zero-lead B256
  margin.
- The realtime daemon now calls `mlockall(MCL_CURRENT | MCL_FUTURE)` and
  prefaults 64 KiB of its hardware-thread stack before ALSA starts. On the
  reference service, locked mappings increased from 156 KiB to about 205 MiB
  while resident memory remained about 13.5 MiB. Memory locking is fail-fast and
  uses the service's existing unlimited memlock allowance.
- The installed 232-frame candidate then completed a 22500-period RT86 analog
  loopback while all four SHARED playback ports, all six SHARED capture ports,
  12 OpenSSL workers, four memory-bandwidth workers, and 1080p60 uncompressed
  USB video capture were active. All 352 pulses remained exactly 388 frames and
  PRO, core, SHARED, hardware-XRUN, generation, reset, and PipeWire adapter error
  deltas stayed at zero. A separate 3 ms client-delay run produced 479 isolated
  PRO misses while all 52 pulses remained fixed at 388 frames and hardware
  counters stayed unchanged. Finally, 100 process-wide 3 ms
  `SIGSTOP`/`SIGCONT` stalls during a 7500-period RT86 run retained all 118
  pulses at the same 388-frame phase with zero PRO miss, hardware XRUN, or
  timeline reset.
- PipeWire-facing SHARED playback now exposes Q256/B768 and primes one Q256
  start-delay period while retaining the internal Q64/B512 ring. This corrected
  a startup buffer deficit that made PipeWire's ALSA DLL accelerate from
  `1.007716` past `1.02` and periodically skip input samples despite exact Q256
  ioplug writes. A controlled 721152-frame sine run recorded zero discontinuity,
  source-matching maximum sample delta, PipeWire correction near `1.0`, and zero
  SideALSA or PipeWire error delta. The extra external period is startup
  capacity; it does not change PRO timing or the steady Q256 PipeWire target.
- On the reference PREEMPT_RT host, the xHCI IRQ thread runs as FIFO `50`.
  Running the SideALSA hardware and PRO workers at `88` and `86` inverted that
  ordering. During one 1080p60 USB-video transition run, analog loopback moved
  from 376 to 346 frames with no pulse loss, XRUN, generation change, or reset.
  USB3 LPM disabling did not prevent a separate 372-to-396-frame transition.
  The reference profile now uses hardware `48` and PRO `46`, while PipeWire,
  PipeWire Pulse, and WirePlumber use `10`.
- The corrected priority order completed 80 uncompressed 1080p60 USB-video
  start/stop transitions across three hardware starts. CPU saturation, four
  memory-copy workers, active PipeWire playback, and 3282 measured pulses were
  included. Each hardware start retained its exact 366-, 384-, or 378-frame
  phase with zero pulse loss, PRO miss, hardware XRUN, generation change, or
  timeline reset. Startup phase can still differ between actual hardware
  restarts; with the then-current guard48 profile, the fix prevented normal
  runtime load transitions from moving it.
- A later repeated-start investigation reproduced stable 384- and 390-frame
  starts and 336-to-390-frame settling without an ALSA XRUN. Reducing only the
  startup prime was rejected: a 200-frame prime reached 418 frames during loaded
  startup, while a 136-frame prime reached 402 frames. The accepted profile uses
  guard32, a 216-frame prime, one second of pre-ready silence, and up to eight
  startup-only qualification attempts.
- With that profile, 93 measurable unloaded fresh starts each remained fixed at
  320-369 frames. Thirty PID-matched starts under 12 CPU workers, four memory
  workers, active PipeWire playback, and repeated 1080p60 USB-video opens each
  remained fixed at 320-368 frames. Two starts were rejected and rebased before
  readiness; all client-visible PRO, hardware-XRUN, and reset deltas were zero.
- Three 45000-period loaded runs measured 2109 pulses. A 350-frame start settled
  once at 374 frames, the following run remained fixed at 374, and a separately
  selected 362-frame start remained fixed at 362. The measured maximum was 374
  frames, or 7.792 ms, with zero lost pulse, PRO miss, hardware-XRUN, or runtime
  timeline-reset delta. This bounds the observed reference-host result below
  8 ms; the qualifier does not directly measure analog latency.

## Limitations

- x86_64 only; no WoW64 frontend.
- Fixed sample rate and buffer size; no runtime rate switching.
- Float32 ASIO buffers converted to physical S32_LE.
- `GetLatencies` reports the 64-frame input and 64-frame software output
  path, but not USB, converter, cable-loopback, or startup-qualified latency.
- Earlier builds showed sub-Q64 common-path shifts while both ALSA streams
  remained `RUNNING`. Shallow/late SideALSA refill and SHARED work ordering were
  confirmed software triggers, so these shifts must not be labeled as
  hardware-only. The crash/reacquisition harness checks each leg for stable
  phase, compares ASIO with an immediately following native PRO run, and reports
  any remaining common-path shift separately. The startup qualifier prevents
  clients from seeing the initial one-second settling interval, but one loaded
  run still moved from 350 to 374 frames afterward. A longer multi-hour
  stability run is still required.
- Freezing the daemon process is not normal scheduler load. A later explicit
  3 ms `SIGSTOP` sequence repopulated the USB playback queue and changed analog
  phase without an ALSA XRUN. Runtime continuity is therefore not guaranteed
  across process suspension, debugger stops, or higher-priority external RT
  threads; userspace audio priorities must remain below the USB IRQ thread.
- The zero-lead revision has not yet repeated the full Discord playback,
  microphone, and screen-sharing soak.
- Hardware-engine stop is non-draining. A finite `max_periods` run or daemon
  shutdown stops the PCM without draining queued playback.
- While buffers exist, ASIO retains exclusive PRO ownership across `Stop`.
  Dispose buffers or close the driver before opening another PRO client.
- Probe requires a running `sidealsad` and physical hardware.
