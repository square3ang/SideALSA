# Same-Process Audio Load Benchmark

This benchmark renders actual multivoice sine audio inside the ASIO playback
callback. It is not an external CPU burner or the existing timed busy loop.
Optional background workers live in that same playback process. It uses the
reference 48 kHz/Q64, 10-input/8-output interface and the output-0/input-4
internal digital return, not an analog DAC/ADC latency measurement.

## Build and Run

```sh
cmake -S crates/sidealsa-asio -B build-asio -DCMAKE_BUILD_TYPE=Release
cmake --build build-asio --target sidealsa-asio-probe sidealsa-asio-loopback-test
ctest --test-dir build-asio --output-on-failure

bash scripts/test-asio-audio-load.sh
```

The daemon must already be running and PRO must be free. The script never
restarts the daemon/hardware or launches an external CPU load. It acquires and
releases PRO for each test, leaving other applications untouched. Output 0 carries
a sine contribution bounded to 0.03125 peak (-30 dBFS), plus the existing 0.25
measurement impulses. Isolate the test output from speakers as appropriate.

Five cases run by default, with a native pulse-only reference before and after
each ASIO process:

| Case | Loaded-stage sine voices | Same-process background workers |
| --- | ---: | --- |
| `pulse_only` | None, original pulse test | None |
| `sine_baseline` | 1 | None |
| `dsp` | Configured count | None |
| `workers` | 1 | Configured threads/memory |
| `combined` | Configured count | Configured threads/memory |

Each ASIO process performs two Start legs. Inside each leg the sample clock
selects warm (first 25%), loaded (middle 50%), and cool (last 25%) stages without
stopping the stream. Warm/cool render one sine voice; loaded renders all voices
and enables any background workers. Worker allocation, memory prefaulting,
thread creation, oscillator initialization, and libm prewarming happen before
Start. Work is bounded; large voice counts can still exceed the client deadline.
The oscillator sum is used in the real output, not discarded by the compiler.

The sine is below the pulse detector threshold even at its peak. Pulse positions
remain frame 65 plus multiples of 4097; pending pulses retain their emission
stage across transitions. Callback timing, loopback counts/ranges, oscillator
operations, completed worker units, and first-stage monotonic timestamps are
printed only after Stop. Completed worker batches can be reported in cool after
the gate closes; `worker_units` is not an exact instantaneous CPU-time measure.
Host sleep overshoot and the final 100 ms pulse drain can extend the cool stage.

## Controls

| Harness environment | Default / bounds |
| --- | --- |
| `SIDEALSA_ASIO_AUDIO_LOAD_MS` | 8000 ms per leg, 1000-600000 |
| `SIDEALSA_ASIO_AUDIO_LOAD_VOICES` | 512, 1-4096 |
| `SIDEALSA_ASIO_AUDIO_LOAD_THREADS` | 24, 1-64 |
| `SIDEALSA_ASIO_AUDIO_LOAD_MEMORY_MIB` | 512, 0-4096 |
| `SIDEALSA_ASIO_AUDIO_LOAD_CASES` | All five cases; space-separated unique case names |
| `SIDEALSA_ASIO_NATIVE_PERIODS` | 1500, at least 128 |
| `SIDEALSA_ASIO_BUILD_DIR` | `build-asio` under the repository |
| `SIDEALSA_ASIO_DLL_DIR` | Same as build directory; use `/usr/local/lib/wine` for installed DLLs |
| `SIDEALSA_AUDIO_LOAD_LOG_DIR` | Timestamped directory under `target/audio-load` |

Example focused combined test:

```sh
SIDEALSA_ASIO_DLL_DIR=/usr/local/lib/wine \
SIDEALSA_ASIO_AUDIO_LOAD_CASES=combined \
SIDEALSA_ASIO_AUDIO_LOAD_VOICES=256 \
  bash scripts/test-asio-audio-load.sh
```

The underlying probe enables this mode with
`SIDEALSA_ASIO_PROBE_SINE_VOICES=1..4096`. Unset/0 preserves the previous probe
behavior. It implicitly enables loopback and timing, and rejects lifecycle mode
and legs shorter than one second. Existing worker controls remain available;
the harness explicitly sets synthetic `CALLBACK_WORK_US=0` for separation.
Probe compilation now explicitly uses `-O2` and `-lm`; Release did not previously
optimize the custom C probe commands. Driver/DLL compilation flags are unchanged.

## Acceptance and Self-Test

The harness rejects missing counters, stale probes without sine support, missing
or duplicate stage records, missing loopback observations, incorrect operation
counts, and worker-enabled legs with no completed loaded-stage work. It checks
the process status, all stage/leg phases, native before/after parity, daemon PID,
generation, and PRO/HW/SHARED counter deltas. It keeps failed-case logs rather
than silently relaxing the checks. An unstable native reference aborts the run.

`period_overruns=0` does not imply zero PRO misses: a Q64 period is 1333 us, but
the client handoff is at most 1000 us and can be shortened. High DSP counts are
useful negative controls, not expected clean passes.

`SIDEALSA_ASIO_PROBE_SINE_SELF_TEST=1` returns before COM/driver connection.
CTest rebuilds the probe, checks for its self-test marker before execution, and
requires the explicit self-test PASS result. It covers all oscillator phase
advances and an independent mixed-sample calculation at 4096 voices, output
bounds, pulse/shift detection, stage/reset behavior, and per-leg worker snapshots.
It does not open SideALSA audio; Wine runtime initialization still occurs.

## Reference-Host Results (2026-09-05)

All runs retained daemon PID 169730/generation 0. Desktop activity was not
disabled; these are live-host observations, not isolated causal experiments.

| Case | Loaded callback mean (two legs) | PRO miss delta | HW XRUN delta | Measured phase |
| --- | --- | ---: | ---: | --- |
| 512 voices, DSP only | 278 / 297 us | 0 | 0 | 347 frames |
| 1 voice + 24 workers/512 MiB | 7.1 / 7.3 us | 0 | 0 | 347 frames |
| 512 voices + 24 workers/512 MiB | 445 / 435 us | 1 | 0 | 347 frames |
| 1024 voices, DSP only | 546 / 513 us | 181 | 0 | 347 frames, one pulse lost |
| 1024 voices + 24 workers/512 MiB | 888 / 918 us | 3848 | 0 | 384 frames; native before was 347 |
| 256 voices + 24 workers/512 MiB, hardened checks | 226 / 227 us | 0 | 0 | 371 frames before, during, and after |

The first matrix already moved 366 to 347 frames in its pulse-only control.
The 1024-voice combined case reproduced a persistent 37-frame increase, but its
first warm stage was already at 384 frames before heavy DSP/workers were enabled.
This localizes the change to the interval between the preceding native reference
and the first ASIO observations, not to the later overloaded stage. Startup,
allocation, driver activation, and other host activity remain candidates.

A later matrix correctly aborted when its native reference itself changed
384 to 330 frames, without a miss, XRUN, or reset. A subsequent focused
256-voice combined test passed all checks: 200/200 pulses, positive worker units
in both loaded legs, and no counter delta. Logs are under
`target/audio-load/20260905-154757`, `20260905-155139`, `20260905-161111`, and
`20260905-162407` respectively. The first two runs preceded the extra per-stage
worker-counter/harness validation; they still recorded actual synthesis and
positive whole-process worker totals.

At a later inspection FL Studio used `ILWASAPI2ASIO_x64.dll` and Wine Pulse,
with PipeWire SHARED line4 playback and mic1 capture, not the exclusive SideALSA
ASIO slot. Its process predated the observed native-reference transition; that
transition must not be labelled a captured FL launch. This benchmark does not
yet reproduce FL's plugin loading, graphics/CUDA work, Wine device enumeration,
or PipeWire graph activation sequence. Neither overloaded callbacks nor a
single common-path shift alone identifies the root cause of that application
startup behavior.
