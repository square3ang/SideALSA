# ALSA PRO ioplug latency investigation

These are the pre-change measurements. The implemented buffer/start/pointer
correction and new results are in [Independent ALSA PRO buffers](alsa-pro-buffers.md).

## Result

Extra latency was reproduced in an ALSA PRO duplex application, on the same
unrestarted daemon/hardware timeline used for native and Wine ASIO controls.
It changed in exact logical-period increments with application prefill.

Q64/P64/B256, 48 kHz, S32_LE, physical 8-out/10-in, output 0 to internal digital
return input 4; ALSA and native test processes used FIFO priority 46.

| Path / startup prefill | Measured return | Extra vs native | Capture availability maximum after each read/write iteration |
| --- | ---: | ---: | ---: |
| Native PRO, before and after | 348 frames / 7.250 ms | 0 | Not applicable |
| Real Wine ASIO, two legs | 348 frames / 7.250 ms | 0 | Not applicable |
| ALSA PRO, 1 × 64 frames | 412 frames / 8.583 ms in a successful trial | 64 frames / 1.333 ms | 0 |
| ALSA PRO, 2 × 64 frames | 476 frames / 9.917 ms | 128 frames / 2.667 ms | 64 frames |
| ALSA PRO, 3 × 64 frames | 540 frames / 11.250 ms | 192 frames / 4.000 ms | 128 frames |
| ALSA PRO, 4 × 64 frames | 604 frames / 12.583 ms | 256 frames / 5.333 ms | 192 frames |

The one-period trial is **not a reliable low-latency setting**: a subsequent
repetition failed with playback `EPIPE`. Prefills 2–4 repeated with all 45 pulses
returned per run and identical minimum/maximum delay. Each of those preserved
daemon PID/generation and had zero PRO/HW/SHARED miss/reset deltas. The later
Wine control returned 36/36 pulses in each leg at 348 frames.

The ALSA test opens separate playback and capture handles in one process,
prefills playback, starts capture then playback, and loops `readi(64)` followed
by `writei(64)`. Pulses are anchored to the just-read capture block and observed
in subsequent capture blocks. This measures the application capture/process/
output/return path, avoiding an assumption that the two PCM start timestamps
are identical. It is not a pure playback-only one-way measurement and is not
the user's actual game/DAW backend.

## Causes supported by code and measurements

### Client buffer capacity is tied to the hardware buffer

`requested_buffer_size()` in `crates/sidealsa-alsa/src/lib.rs` chooses
`DeviceInfo.buffer_size` for PRO. `plugin_buffer_size()` enforces at least two
periods, and `minimum_plugin_buffer_size()` returns the maximum for PRO.
`sidealsa_hw_params()` and constraints in `src/plugin.c` permit only that exact
PRO buffer size. Thus hardware B256 becomes an ALSA application B256 here;
applications cannot independently negotiate B64/B128 while the daemon keeps
hardware B256.

Capacity alone does not impose 256 frames of latency: all table rows use the
same B256. Actual prefill, retained input and scheduling determine occupancy.

### FIFO playback can run one cycle ahead

Prepared writes enter `playback_fifo`; `start()` flushes that data through
`flush_playback_blocks()`. Once a sequence is assigned, subsequent blocks keep
incrementing it. `latest_publishable_playback_sequence()` permits the observed
cycle plus one. This absorbs the timing of ordinary ALSA writers but differs
from ASIO's current-capture-sequence callback submission.

The test reports `snd_pcm_delay(playback)` as 64 frames after each steady-state
write, consistent with the one-period-ahead contribution in this workload. This
does not prove that every possible ALSA application must add exactly one period.

### Independent buffered capture retains the startup backlog

Directional PRO capture satisfies `is_buffered_capture()` and follows the
buffered capture path. Ready samples are read in order, rather than discarding
old input to imitate a same-cycle ASIO callback. Playback startup can progress
while capture is already collecting. Reading one capture block per subsequent
playback iteration preserves that initial backlog.

The measured additional delay matches a 64-frame playback contribution plus
0/64/128/192 retained capture frames for the successful startup conditions.
That explains the exact 64-frame steps without attributing them to copy cost
or hardware latency drift. PRO does not inherit the SHARED five-period playback
offset: its `playback_latency_periods` is zero in this adapter.

## Additional controls and limitations

Explicitly forwarding the startup capture backlog reduced prefills 2 and 3 to
412 frames / 8.583 ms, with no retained capture availability in the probe. Other
forwarding/startup trials failed with EPIPE. Starting playback before capture
also failed with EPIPE in the tested blocking loop. These are diagnostics, not
recommended workarounds: independent activation, pointer advancement and strict
playback expiry make startup phase significant.

The observed buffering concerns duplex/monitoring latency. Playback-only apps do
not have the capture-backlog component and may have additional application or
ALSA plug/conversion buffers. Comparing their subjective latency requires the
actual app's negotiated params, prefill and callback/write order.

The correction to investigate next would separate PRO client buffer geometry
from hardware capacity and coordinate directional startup/sequence alignment.
Simply removing the FIFO or silently skipping captured samples would sacrifice
data integrity or scheduling tolerance. No production audio behavior was changed
as part of this investigation.

## Evidence

- `/tmp/opencode/alsa_pro_latency.c`: reference-device-only ALSA probe.
- `/tmp/opencode/run_alsa_latency.py`: repeated native/ALSA control runner.
- `/tmp/opencode/alsa-pro-latency-te23k6sw/`: per-case results and counter snapshots.
- `/tmp/opencode/q64-asio-j3_0zj60/`: contemporaneous Wine ASIO control.

No daemon, PipeWire or hardware restart was used for these comparisons. The
7.250 ms contemporaneous control is intentional: the older 6.500 ms result must
not be mixed into these deltas as though hardware state were identical.
