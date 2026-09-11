# PRO deadlines for multithreaded DSP

## Problem and correction

The old reference profile used a fixed 1,000 us PRO handoff ceiling, independent
of the ASIO period. Increasing the period to 256 frames at 48 kHz therefore did
not give a host its expected processing window. The direct engine also reserved
one quarter of the logical period for writing: 1.333 ms at Q256, versus 0.333 ms
at Q64. Both cut into the host's usable DSP time.

The correction has two parts:

- `pro_handoff_auto = true` uses one logical period as the handoff ceiling in
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

Update the daemon, admin helper and GUI, then use **Scheduling → Automatic PRO
handoff (scales with buffer period)** and Apply. The retained manual value is
used when automatic mode is off; manual zero remains a zero budget. Modes other
than direct linked zero-lead continue to use the manual value.

```toml
[device]
pro_handoff_auto = true
pro_handoff_us = 1000 # retained manual ceiling, ignored in automatic direct mode
```

New reference E1x2/E2x2 profiles enable automatic handoff. Existing profiles
without the new key retain manual semantics and need the checkbox enabled;
`update.sh` deliberately does not overwrite user profiles. The GUI's **Manual
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
