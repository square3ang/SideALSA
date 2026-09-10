# SHARED Pop Investigation

## Evidence collected on 2026-09-10

The user reported a brief pop on the reference desktop. No audio recording or
sample-level trace exists for that instant, so its exact cause is **not proven**.

- The same daemon PID 793 remained running across observations. SHARED underruns,
  overruns, hardware XRUNs, timeline resets, and all playback-port miss counters
  stayed zero, including the later snapshot at hardware period 9,641,568.
- `pw-top` showed Line 4 driving Firefox at Q256 / 48 kHz, then Line 4 suspended
  and Firefox idle. Both observed ERR counts were zero. These transitions were
  observed after the report, not synchronized with the audible pop.
- The installed Line 4 adapter has `node.suspend-on-idle = true`, playback
  headroom 128 and start-delay 256. The daemon uses Q64/P64, B256, five SHARED
  latency periods, and no SHARED repeat-on-underrun.
- WirePlumber logged a link activation failure at 18:01:00. That message does
  not identify the affected audio route; it is not proof of the pop's cause.
- Installed daemon and ALSA plugin files matched the corresponding release
  artifacts by SHA-256. This compares disk files, not every process's loaded
  mappings or source provenance.

Zero deadline counters do not measure sample continuity, clipping, source-audio
glitches, or acoustic output. They cannot rule out a pop.

## Confirmed discontinuity mechanism

`SessionState::stop()` in `crates/sidealsa-daemon/src/state.rs` deactivates the
session, clears underrun arming, waits for RT work, and resets its shared slots.
`SharedPortBridge::prepare_playback()` skips inactive sessions;
`commit_prepared()` adds valid samples directly without a lifecycle fade.
The next hardware mix therefore omits the stopped contribution, even if its
last sample was nonzero and continuation blocks were already queued.

The ALSA plugin's stop callback calls `sidealsa_stream_stop()`, which sends the
client stop and clears its transfer FIFO. Its separate drain callback can wait
for scheduled playback, but stop does not invoke drain.

The locally available PipeWire source at
`/home/square3ang/pipewire/spa/plugins/alsa/` maps Pause/Suspend to
`spa_alsa_pause()`, then `do_drop()` and `snd_pcm_drop()`. This is source evidence
for the connection, not a captured stack trace of the installed PipeWire at the
reported instant. Disabling suspend alone is not a demonstrated fix: Pause also
uses this path, and upstream audio may already contain a discontinuity.

## Isolated reproduction

`shared_lifecycle_can_cut_a_waveform_without_an_underrun` exercises the real
daemon state/shared-memory/mixing code without opening a PCM:

1. Q64, 48 kHz, five-period SHARED lookahead.
2. Publish two continuous stereo 1 kHz cosine periods, approximately -12 dBFS.
3. Consume the first period; its first sample is applied directly after silence.
4. Stop before consuming the second, already-published period.
5. Observe an all-zero next output block. The stop-boundary step exceeds twice
   the natural next-sample step of that waveform.
6. SHARED underrun, playback/capture hardware XRUN and timeline-reset counters
   remain zero.

This proves a pop-producing signal boundary is possible without an underrun.
It does **not** prove that the user heard this mechanism, nor that graceful
drain or an upstream fade would produce the same discontinuity.

```bash
cargo test -p sidealsa-daemon shared_lifecycle_can_cut_a_waveform_without_an_underrun
```

## Next evidence needed

Reproduce the reported sound while correlating monotonic timestamps for SHARED
start/stop/close, PipeWire state changes, and samples before and after SHARED
mixing. Capture only the selected playback route, not unrelated microphone
audio. A physical/digital return must be verified for that route before it can
localize a downstream device fault.

- A jump introduced at a lifecycle edge supports SHARED de-click handling.
- A jump already present upstream points to the app/PipeWire or source audio.
- A smooth software output with a discontinuous verified return points farther
  downstream; hardware timing and device processing then need investigation.

If lifecycle truncation is confirmed for the audible reproduction, implement a
bounded, preallocated per-port fade on the hardware timeline, with explicit
stop/close semantics and no wait for the client. Test sibling-port and PRO
isolation, normal drain, and near-zero versus nonzero sample boundaries.
Increasing buffers/headroom is not justified by the evidence collected here.

No services or live configuration were changed during the initial investigation.
Only the isolated characterization test and this report were added at that stage.

## Live benchmark follow-up

At the user's request, nine live cases were subsequently run through the existing
daemon, using otherwise unused Line 1 and Input 5/6. The default Line 4 route was
not changed. The source was stereo 997 Hz cosine, peak 0.01 (-40 dBFS), 48 kHz
S32_LE WAV. Input 5/6 was recorded using `pw-cat` as stereo float32. The returned
tone amplitude was 0.01 and source-to-return alignment error on the initial
2,048 frames was approximately 3.4e-8 RMS, verifying the intended digital return.
This measures the device's internal digital return, not analog speaker output.

| Case | Repetitions / duration | Result |
| --- | --- | --- |
| PipeWire continuous, 10 ms source fade | 12 s | Interior clean; 512 final frames missing (10.67 ms), abrupt final step 0.005598 |
| Same, eight CPU workers | 12 s | Same 512-frame truncation; interior clean |
| PipeWire abrupt source edges | 10 × 350 ms | 20 large steps, including deliberately abrupt source starts; final 736–800 frames missing |
| PipeWire with 20 ms source fades | 10 × 350 ms | All ten endings truncated by 736–800 frames (15.33–16.67 ms); final step 0.003195–0.004821 |
| PipeWire process terminated mid-wave | 10 | Abrupt endings, expected for the intentional interruption; max step 0.009981 |
| ALSA SHARED directly via `aplay` | 12 s | Full faded waveform preserved; no large steps |
| PipeWire faded source plus 100 ms trailing silence | 10 | Full nonzero waveform preserved; no large steps |
| PipeWire faded source with a second silent stream keeping Line 1 active | 10 | Full waveform preserved; no large steps |
| ALSA SHARED directly, faded short files | 10 × 350 ms | Full waveform preserved; no large steps |

Every case had zero deltas in SHARED underruns/overruns, hardware XRUNs and
timeline resets, with daemon PID 793 unchanged. All normal playback commands
exited successfully. Only test-owned processes were terminated, including the
intentional mid-wave interruptions and CPU workers. No restart or buffer/headroom
configuration change was used.

### Analysis details

The natural maximum adjacent-sample step of this test tone is approximately
0.001304. A `> 0.003` step flags a large discontinuity for **this signal**, not a
general-purpose music pop detector. Windowed sine fits exclude the beginning
and ending windows of continuous cases. Their maximum interior residual was
about 1.8e-8 RMS, including under CPU load. Fit errors at intentional fades or
silence boundaries in short-file cases are expected and are not counted as
steady-state glitches.

Captured bursts were aligned against the known source onset and their final
nonzero sample compared with the source. The successful cases end at source
frame 16,798 of 16,800 (or 575,998 of 576,000): the remaining last source sample
is deliberately zero, so the analyzer's raw `missing_tail_frames=1` is **not a
lost audible sample**. Failed cases end hundreds of samples earlier, with a
nonzero value immediately followed by silence.

### What this isolates

The reproduced fault is **tail truncation when the final PipeWire playback
stream goes away and its ALSA sink stops**. It occurs even with a smooth source
fade. Keeping the sink active preserves the same waveform, and the direct ALSA
drain path preserves it too. That separates this fault from a general SHARED
transport underrun, tone-source corruption, or CPU starvation in these runs.

The ALSA ioplug reports its position from elapsed hardware cycles relative to
activation. Its callback table has a custom drain but no delay callback. The
actual output is scheduled through a FIFO and the daemon's SHARED lookahead.
The observed tail loss warrants auditing the relationship between that reported
position, queued audio, PipeWire's drain-complete decision, and subsequent
Pause/Suspend/drop. These measurements do not yet partition the lost frames
among those layers; adding a guessed constant to the reported delay is not a
validated fix. In particular, local PipeWire's `get_status()` derives playback
delay from availability, so a delay callback alone must not be assumed sufficient.

**Revised fix priority:** correct end-of-stream accounting/draining first; a fade
alone would hide the edge while still losing valid audio. Preserve explicit-drop
semantics separately, where a bounded de-click policy may be appropriate. The
100 ms padding and silent keepalive are experimental controls, not a deployed
workaround or a recommendation to increase general buffering.

This establishes a concrete, repeatable pop-producing failure in the tested
PipeWire route. The original unrecorded pop on Line 4 remains unattributed; these
results do not prove every pop or continuous-playback problem has this cause.

### Raw evidence

- First six cases: `/tmp/opencode/shared-pop-ioji6hre/`
- Three control cases: `/tmp/opencode/shared-pop-ftf2vdg0/`
- Runner: `/tmp/opencode/shared_pop_bench.py`
- Source alignment/counter analysis: `/tmp/opencode/analyze_shared_pop.py`

Each directory contains source WAV files, captured `.f32` digital returns,
per-case `.before`/`.after` stats, process-event monotonic timestamps, stderr,
and `results.json`. These are temporary host-local investigation artifacts.
The runner is specific to the verified Line 1/Input 5/6 mapping and must not be
used on an arbitrary interface without verifying its routing first.

## Midstream investigation

The user also reported pops during ongoing playback. This is a separate symptom;
the confirmed end-of-stream truncation must not be generalized to it.

Six additional 45-second cases were measured through Line 1 and the verified
Input 5/6 digital return. Source amplitude remained -40 dBFS. Analysis excludes
one second at each end of the detected tone, leaving approximately 43 seconds
per case (258 seconds analyzed in total).

| Continuous-playback case | Concurrent activity | Midstream anomalies |
| --- | --- | --- |
| Native PipeWire `pw-cat`, 48 kHz | Baseline | None detected |
| Native PipeWire `pw-cat`, 48 kHz | 12 independent CPU workers | None detected |
| Native PipeWire `pw-cat`, 48 kHz | Line 2 silent playback opened/closed 174 times | None detected |
| Native PipeWire `pw-cat`, 48 kHz | Same Line 1 sink gained/lost a silent stream 179 times | None detected |
| PulseAudio compatibility `paplay`, 48 kHz WAV | Baseline | None detected |
| PulseAudio compatibility `paplay`, 44.1 kHz WAV | 12 CPU workers; return at 48 kHz | None detected |

All commands completed successfully. Across all six cases, daemon PID stayed
793 and SHARED underrun/overrun, hardware XRUN and timeline-reset deltas were
zero. The test workers were finite-duration processes and all remaining
test-owned children were terminated at each case's end.

Unlike the initial short benchmark, this analysis includes low-amplitude windows
inside the selected interval and compares phase between adjacent windows:

- Maximum adjacent-sample difference: 0.00130415, consistent with the source sine.
- Steps above 0.003: zero.
- Windows with fitted amplitude below 0.002: zero.
- 1,024-frame windows with sine-fit RMS residual above 0.0001: zero.
- Adjacent-window phase jumps above 0.02 rad: zero.
- Maximum local-fit residual: approximately 1.8e-8 RMS.
- Maximum observed phase-step equivalent: below 0.000004 samples.

These thresholds are signal-specific. A 997 Hz sine does not establish arbitrary
music fidelity, and an exactly whole-cycle discontinuity can be ambiguous with
a periodic probe. There was no detected sustained drift, zero gap or phase jump
in these bounded runs. Silent stream churn tests lifecycle interference, not
clicks already present in a notification's waveform or clipping from loud mixing.

The results weaken CPU starvation and ordinary SHARED sibling start/stop
interference as explanations under these tested conditions, but do not exclude
rarer failures, other workloads, the user's Line 4 route, or downstream analog
events. No root cause for the reported **midstream** pop has been established.
Next evidence should come from the actual affected application/route, with
synchronized upstream playback monitoring and a verified return, rather than
another speculative buffering change. Browser decoding, mixed-stream sample
discontinuities, and downstream device output remain untested possibilities,
not diagnoses.

Raw results and recordings:

- `/tmp/opencode/shared-midstream-qft9g2kv/`: first four cases.
- `/tmp/opencode/shared-midstream-u_zs0qzg/`: PulseAudio compatibility cases.
- `/tmp/opencode/shared_midstream.py`: host-specific runner and analysis.

No service restart, permanent configuration, default-route change, or buffer
increase was performed. The generated silent streams and CPU workers were used
only for the tests.
