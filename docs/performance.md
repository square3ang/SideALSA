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
