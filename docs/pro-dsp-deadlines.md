# PRO deadlines for multithreaded DSP

## Problem and correction

The old reference profile used a fixed 1,000 us PRO handoff ceiling, independent
of the ASIO period. Increasing the period to 256 frames at 48 kHz therefore did
not give a host its expected processing window. The direct engine also reserved
one quarter of the logical period for writing: 1.333 ms at Q256, versus 0.333 ms
at Q64. Both cut into the host's usable DSP time.

The correction has two parts:

- `pro_handoff_auto = true` uses one logical period **minus write reserve** as the handoff ceiling in
  direct, linked zero-lead mode. The real deadline remains bounded by observed
  ALSA queued frames and time already spent since that observation.
- The write reserve is `min(ceil(Q/4), ceil(rate/3000))` frames. Q64 at 48 kHz
  retains its existing 16-frame margin. Q256 now reserves 16 rather than 64
  frames. At nominal one-period remaining occupancy this permits 5 ms of total
  handoff time instead of 4 ms; publication, wakeup and conversion time consume
  that same budget. Late hardware wakes still shorten it, down to zero.

No hardware buffer, startup queue, SHARED headroom, sample sequence mapping,
thread priorities, ASIO spin setting, or repeat fallback was changed. Processing
still returns as soon as the correct block is ready, without waiting until the
ceiling. The engine never waits indefinitely for a DSP worker.

## Enable for an existing installation

Update the daemon, admin helper and GUI. Automatic handoff defaults to enabled,
including existing profiles that omit `pro_handoff_auto`. The checkbox is under
**Scheduling → Automatic PRO handoff (scales with buffer period)**. The retained manual value is
used when automatic mode is off; manual zero remains a zero budget. Modes other
than direct linked zero-lead continue to use the manual value.

```toml
[device]
pro_handoff_auto = true
pro_handoff_us = 1000 # retained manual ceiling, ignored in automatic direct mode
```

New reference E1x2/E2x2 profiles explicitly enable automatic handoff. Existing profiles
without the key use the same default without requiring a file rewrite. Explicit
`pro_handoff_auto = false` is preserved; enable the checkbox and Apply to opt back
in. `update.sh` does not overwrite user profiles. The GUI's **Manual
PRO handoff** field is labelled accordingly.

Automatic handoff does not make unsupported ALSA geometry valid. It also cannot
make a DSP graph whose critical path exceeds the available period run in time.

## Real Wine / hardware A/B benchmark

The added `sidealsa-asio-dsp-probe` uses real ASIO COM callbacks in a private Wine
prefix. Each callback signals four or eight Windows worker threads, which do
bounded CPU work in parallel; the callback waits for all workers and publishes
silence. This models a fork/join DSP dependency instead of merely running
unrelated background CPU burners. It does not instantiate FL Studio or Ozone.

The first 32 callbacks are silent warmup, ensuring valid PRO output arms deadline
accounting before load begins. Each short case runs for three seconds and
delivers approximately 562 callbacks. Means include warmup; maxima are measured
in the probe with QueryPerformanceCounter. Global max-counter differences are
not callback-duration measurements.

All comparisons used Q256/P256/B512, 48 kHz, a two-period startup queue, the same
ASIO build and thread priorities. Temporary systemd overrides selected the test
daemon/profile and were removed afterwards. The existing profile, installed
daemon binary and ordinary service command were restored. No user's Bottles or
Proton prefix was used, and PipeWire services were not restarted.

| Worker work / count | Fixed 1 ms misses | Automatic + capped reserve misses |
| --- | ---: | ---: |
| 0 us / 4 | 0 | 0 |
| 500 us / 4 | 1 | 0 |
| 1,500 us / 4 | 530 | 0 |
| 3,000 us / 4 | 530 | 0 |
| 3,000 us / 8 | 530 | 0 |
| Repeated 3,000 us / 4, fresh daemon | 530 | 0 |

All rows had zero hardware XRUN and timeline-reset deltas. Core misses were
zero; the manual-mode misses were client deadline misses. In the eight-worker
automatic case, the measured maximum callback time was 4.675 ms, which fits the
new nominal window but would exceed the old 4 ms window.

An intermediate automatic-only build retaining the old quarter-period reserve
still produced 5 misses for four workers and 58 for eight workers at 3 ms. This
motivated the separately tested reserve correction, rather than treating the
automatic ceiling alone as sufficient.

### Longer run and overload isolation

A ten-second automatic-mode check kept a SHARED PipeWire playback stream active:

| Work / workers | Callbacks | PRO misses | HW XRUNs | SHARED underruns/overruns |
| --- | ---: | ---: | ---: | ---: |
| 3 ms / 8 | 1,876 | 0 | 0 | 0 / 0 |
| 6.5 ms / 8, intentional overload | 1,499 | 1,843 | 0 | 0 / 0 |

The normal run's maximum callback time was 4.689 ms. The overloaded run reached
9.760 ms and recorded 1,467 callback-period overruns. Misses can exceed callback
count because hardware periods continue while an overlong callback is running.

These bounded measurements demonstrate removal of the artificial DSP budget
bottleneck on the reference host. They are not a direct PipeASIO performance
comparison, a universal reliability guarantee, or an FL Studio/Ozone acceptance
test. Native PipeASIO's source follows the PipeWire graph processing cycle;
SideALSA's old independent 1 ms ceiling was the concrete mismatch investigated.

## Reproduce and evidence

```bash
cmake -S crates/sidealsa-asio -B build-asio -DCMAKE_BUILD_TYPE=Release
cmake --build build-asio --target sidealsa-asio-dsp-probe
# Use a private Wine prefix with the driver's PE DLL staged in system32.
WINEPREFIX=/path/to/private-prefix WINEDLLPATH="$PWD/build-asio" \
  wine "$PWD/build-asio/sidealsa-asio-dsp-probe.exe.so" 3000 8 10000
```

The probe acquires exclusive PRO and outputs silence. It accepts the daemon's
reported period and requires a running daemon. Record stats before/after each
case and compare only within one daemon PID. Reconfiguration interrupts hardware
audio and must be deliberate.

Host-local evidence:

- `/tmp/opencode/dsp-deadline-fn9o5sf9/`: automatic-only intermediate comparison.
- `/tmp/opencode/dsp-deadline-lx086u6_/`: final A/B and repeated confirmation.
- `/tmp/opencode/dsp-deadline-o6wtvgw0/`: ten-second SHARED-active / overload checks.
- `/tmp/opencode/dsp_deadline_ab.py`: temporary host-specific orchestration.

Each result directory contains per-case stats, probe callback timing, exact test
profiles and restoration evidence. Unit tests cover automatic/manual semantics,
fingerprints and the actual queue-bounded deadline formula.

## Q64 ceiling regression and latency investigation

The initial automatic implementation used a whole-period configured ceiling.
When ALSA reported more than one period queued, Q64/48k could therefore wait
up to 1.333 ms instead of the previous 1 ms. The corrected implementation
subtracts the write reserve from the configured ceiling too, using the same
reserve function as the engine's occupancy bound. This keeps Q64 at 1 ms even
with extra queued frames; Q256 retains its nominal 5 ms DSP window.

A user's approximately 13 ms report was reproduced as 671 frames / 13.979 ms
using native PRO before restarting the hardware. With identical Q64/P64/B256
and 128-frame startup settings:

- Direct ALSA reference runs measured 323 and 329 frames (6.729 / 6.854 ms).
- Fresh daemon runs measured 323 and 317 frames (6.729 / 6.604 ms).
- Five deliberate 20 ms process stops each produced a genuine hardware rebase,
  but did not reproduce the 14 ms state. Their subsequent loopback observations
  were 323–353 frames. Some probes failed strict acceptance due to phase variation,
  one or two native-client misses, or a lost pulse; those are not clean passes.
- After deployment of the ceiling correction, a FIFO-46 native Q64 probe
  measured 353 frames / 7.354 ms for 118 pulses with no misses, lost pulses or
  hardware resets.
- A subsequent private-Wine ASIO loopback passed both legs at 378 frames /
  7.875 ms, 36/36 pulses per leg. A following native PRO probe measured exactly
  the same 378 frames with no misses. The installed daemon's observed handoff
  budget maximum was 999,521 ns, below the corrected 1 ms ceiling.

The higher-latency runtime state cleared on full close/reopen **before** the
code change. Thus the ceiling regression is confirmed and corrected, but it
does not by itself explain the entire original 14 ms measurement. Residual
device/transport phase variation remains: these measurements do not establish
a permanently fixed 6–8 ms latency or prove that XRUN count caused the old state.
The installed profile values were preserved, and PipeWire was not restarted.

Evidence: `/tmp/opencode/q64-latency-szwplgeu/`,
`/tmp/opencode/q64-xrun-k7s1oan1/`, and `/tmp/opencode/q64-asio-cpljkmo9/`.
