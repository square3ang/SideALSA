# Performance

SideALSA keeps CPU-specific selection outside realtime loops. Optimized audio
kernels are detected and prewarmed before the ASIO worker starts; the worker
then uses one fixed function pointer per playback block. No optimized path
allocates, locks, or performs feature detection in a period cycle.

## Optimized paths

- ASIO Float32 playback uses AVX2 when all eight physical output channels are
  active. Eight planar channel vectors are converted with clipping and
  round-away-from-zero semantics, transposed as an 8-by-8 block, and stored as
  interleaved S32. NaN remains silence and values outside `[-1.0, 1.0]` retain
  the scalar clipping behavior.
- ASIO uses a faster scalar fallback that removes the per-sample `f64::round()`
  call while preserving its result. Sparse channel sets, other channel counts,
  non-x86_64 targets, and CPUs without AVX2 use this path.
- Every ASIO double-buffer half is 32-byte aligned. Padding is outside the
  host-visible `buffer_size` samples.
- The ALSA ioplug uses one bulk byte copy for its normal packed RW-interleaved
  S32_LE areas. Padded or unusual channel areas retain the checked scalar path.
  The bulk operation delegates SIMD selection to the compiler and C runtime.

The AVX2 conversion is independent of the ambient floating-point rounding mode:
it masks NaN and clipped values before conversion, performs exact power-of-two
scaling in f64 lanes, applies an explicit half-step, and truncates. The kernel
executes `vzeroupper` before returning to non-AVX code.

## Microbenchmarks

Run optimized, ignored microbenchmarks with:

```text
cargo test -p sidealsa-asio --release \
  tests::benchmark_asio_playback_conversion -- --ignored --exact --nocapture
cargo test -p sidealsa-alsa --release \
  tests::benchmark_packed_interleaved_area_copy -- --ignored --exact --nocapture
```

Reference-host measurements for the active profile were:

| Operation | Previous | Optimized scalar | Selected path |
|---|---:|---:|---:|
| ASIO Q64, 8-channel Float32 to interleaved S32 | 1169 ns | 686 ns | 369 ns AVX2 |
| ALSA Q256 stereo packed-area input copy | 153 ns | n/a | 19 ns bulk copy |

That is a 3.17x ASIO conversion speedup over the previous implementation and an
8.05x packed-area copy speedup. These are cache-hot kernel measurements, not
end-to-end latency claims, and should be rerun on each deployment CPU.

## Audio-load benchmarks

The [SHARED tuning results](shared-tuning.md) separate playback sequence lead,
capture storage capacity, actual capture accounting and measured round-trip
latency. They include rejected settings and remaining measurement limitations.

The [ALSA PRO playback-only optimization](alsa-pro-playback.md) removes an
unnecessary dependency on still-valid capture for playback scheduling. It is
distinct from ASIO spinning and includes blocking/external-poll probe results.

The [bounded ASIO capture-spin experiment and deployment](asio-spin.md) documents
the zero-lead wait strategy, measured miss reduction and additional CPU cost.
It does not change hardware buffering or the write reserve.

For real synthesis and background load inside one playback process, use the
[same-process audio-load benchmark](audio-load-benchmark.md). It separates
callback DSP load from background workers and compares loopback phase and
deadline counters without restarting hardware.

## Hardware deadline protection

### Client-side work reduction

The ASIO worker checks daemon control once after capture acquisition and before
host callback dispatch, rather than polling the same control FD both before and
after acquisition. The blocking wait still monitors daemon control. This removes
one zero-timeout `poll` per acquired cycle (750 per second at 48 kHz/Q64).

Client notification draining returns after one successful non-semaphore eventfd
read, which already consumes the accumulated count. It retries only interruptions,
at most twice, rather than reading until `EAGAIN` with unlimited retries. This
avoids an extra read on the notified fast path and bounds the drain under new
notifications/signals. Shared-memory readiness remains authoritative; the existing
multiple-ready-slot re-notification is retained.

ASIO now uses `AudioStream::try_capture_buffer` to acquire the actual capture
block directly. It no longer calls `wait_period(Duration::ZERO)` to look up a
sequence, polls internally when empty, then looks up/copies capture separately.
The ASIO worker owns the single blocking poll over audio, lifecycle, stop and
daemon-control descriptors. The nonblocking API drains the wake hint before
inspecting capture and re-notifies if another valid block remains, preserving
pollability without spinning on stale notifications. It retains capture expiry
and generation/discontinuity checks; callers must monitor daemon control
separately, as ASIO does before host callback dispatch.

These are overhead reductions, not measured DJMAX/EZ2ON miss-rate improvements.
They do not add buffering or relax stale-block rejection. Tests cover accumulated
notifications, subsequent wakes, spurious wakes, multiple-slot re-notification,
disconnect detection and worker lifecycle behavior. Real game testing requires
loading the rebuilt client/ASIO binaries in a later session; running audio is not
modified by building this change.

Hardware-free acquisition microbenchmark:

```sh
cargo test -p sidealsa-client --release -j 2 \
  benchmark_externally_polled_capture_acquisition -- --ignored --nocapture
```

One 20,000-block run measured 397 ns/block for wait-then-acquire versus 354
ns/block for direct acquisition. Both include in-process capture publication,
event notification and Q64/10-channel copying. This measures already-ready,
cache-hot capture, not thread wake latency, the empty-ring blocking path or a
game callback. The 43 ns difference is small and does not establish the cause
or resolution of frequent game misses.

### Remaining deadline constraints

The opt-in [PRO deadline diagnostics](pro-diagnostics.md) records a coherent
last-miss identity, actual wait-entry budget and publication outcome without
changing the shipped wire format. Use it in a later authorized diagnostic
session, not by restarting active audio without permission.

The direct-mode 1000 us handoff is a ceiling, not a guaranteed callback window.
For example, an observed 32-frame playback ring remainder leaves approximately
333 us after the Q16 write reserve; capture publication and other elapsed work
spend part of that interval. Driver/USB in-flight frames are not interchangeable
with free ALSA ring write time. A single `/proc/asound` snapshot outside the
capture handoff cannot establish the queue depth of missed cycles.

`callback_max_nanos` excludes worker wake latency, capture acquisition and output
conversion/publication. A client-miss classification is therefore not proof of
an overlong game callback. Sequence-correlated queue/cutoff, wake, callback and
publication measurements are needed to distinguish these causes.

Increasing only B256 capacity or the configured handoff ceiling does not ensure
more usable time. A real additional Q64 queue/pipeline stage would cost roughly
1.333 ms at 48 kHz and must keep consumption sequence, deadline and reported
latency consistent. The existing `pro_latency_periods=1` Q64/P32 mode uses a
different staged-packet path with a tighter handoff limit; it is not a drop-in
full-period processing-window switch. Do not remove the write reserve or accept
stale blocks to make the miss counter look better.

### Absolute cutoff protection

The direct duplex queue observation is timestamped before the playback
availability query. Time spent in that query or preempted around it therefore
consumes the observed queue budget instead of extending the PRO cutoff.

The daemon waits for PRO playback against a `CLOCK_MONOTONIC` absolute timerfd
deadline, alongside the existing playback-ready eventfd. Converting the cutoff
to a relative `ppoll` timeout allowed preemption before syscall entry to add
another sleep after the intended deadline. The timer is created outside the RT
loop, remains private to the daemon, and is disarmed after each wait to avoid
unused deadline interrupts. Stale notifications and interrupted waits reuse the
same absolute cutoff.

These changes do not increase the hardware queue, change the Q64/P32 geometry,
shorten the configured handoff, or change XRUN/miss classification. They cannot
prevent scheduling stalls that already exceed the write reserve. Timer arming
and disarming add syscalls to a pending-client wait; hardware/load measurements
are needed before claiming a net underrun improvement. Unit tests cover expired
deadlines, timeout/notification distinction, timer reuse and disarming, invalid
event descriptors, and elapsed queue-budget accounting.

### Hardware smoke test (2026-09-06)

Compared the installed daemon (PID 117889) with a temporary candidate service
(PID 144251), using the installed Q64/P32/B256, two-period startup queue profile
unchanged. Each case requested 7500 silent PRO blocks from a FIFO46 native test
client. The late case slept 2 ms every 16 hardware sequence numbers.
This comparison predates the client-side work reductions described above.

| Build / case | PRO miss delta | Core miss delta | Playback/capture XRUN delta | Reset delta |
| --- | ---: | ---: | --- | ---: |
| Installed / normal | 0 | 0 | 0 / 0 | 0 |
| Candidate / normal | 0 | 0 | 0 / 0 | 0 |
| Installed / deliberately late | 998 | 0 | 0 / 0 | 0 |
| Candidate / deliberately late | 1000 | 0 | 0 / 0 | 0 |

The late cases begin at different absolute sequence offsets; the two-miss
difference is not an improvement/regression measurement. SHARED underrun and
overrun deltas were zero, but this test did not establish equivalent active
SHARED workloads across the service switch. No loopback latency was measured.
Neither build reproduced a hardware XRUN, so this is isolation/regression smoke
coverage, not proof of fewer underruns under OBS/game/DSP load. The original
installed service was restored after testing; the candidate was not installed.
Host-local logs: `/tmp/opencode/absolute-deadline.8nDS9Z` (temporary storage).

## Deliberate exclusions

- An AVX2 gather implementation for the 10-channel ASIO capture layout measured
  622 ns versus 409 ns for scalar code, so capture remains scalar.
- The daemon's maximum shared mix is only 384,000 signed saturating additions
  per second and uses strided two-channel mappings. AVX2 has no packed signed
  S32 saturating add or scatter store; conversion overhead would outweigh the
  work saved.
- Existing bulk `copy_from_slice`, `fill`, and shared-memory copies remain under
  compiler or libc control. Replacing them with handwritten SIMD would duplicate
  tuned implementations without evidence of a bottleneck.

At the current 48 kHz/Q64 geometry, synchronization, notification syscalls, and
client scheduling remain larger performance concerns than raw sample arithmetic.
