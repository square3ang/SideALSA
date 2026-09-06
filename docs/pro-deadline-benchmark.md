# Silent PRO Deadline Benchmark

`scripts/test-asio-deadline-sweep.sh` measures PRO misses while an ASIO callback
performs a fixed-duration busy workload. Output remains silent. It does not use
loopback, synthesize pulses, restart services, edit profiles or install binaries.
It temporarily owns PRO; another owning client is not displaced.

Digital loopback latency variation is a separate observation, also reported by
the user on Windows. It is not used as a failure criterion or presumed cause of
PRO misses in this benchmark.

## Run

Build the standard probe (not the loopback probe), then run with PRO available:

```sh
cmake --build build-asio --target sidealsa-asio-probe -j 2
bash scripts/test-asio-deadline-sweep.sh
```

Controls:

| Environment | Default / meaning |
| --- | --- |
| `SIDEALSA_ASIO_BUILD_DIR` | Repository `build-asio`, containing the standard probe. |
| `SIDEALSA_ASIO_DLL_DIR` | `/usr/local/lib/wine`; use the absolute `build-asio` path for the local candidate. |
| `SIDEALSA_SOCKET` | `/tmp/sidealsad.sock`. |
| `SIDEALSA_DEADLINE_RUN_MS` | 4000 per Start leg; 1000-30000. Each case has two legs. |
| `SIDEALSA_DEADLINE_WORK_US` | Space-separated `0 50 100 150 200 250 300 200 100 0`; values 0-1000. |
| `SIDEALSA_DEADLINE_WORKERS` | 0; 1-64 adds that many same-process background workers and 512 MiB of work memory. |
| `SIDEALSA_ASIO_SPIN_US` | Unset uses the loaded driver's default; explicit 0 disables spin, 1-250 selects the half-window in microseconds. |
| `SIDEALSA_DEADLINE_LOG_DIR` | Timestamped directory under `target/deadline-sweep`. |

Each case saves before/after counters, callback timing/scheduler results, probe
status, callback-thread CPU usage and observed loaded DLL mappings. CPU accounting
includes between-callback work and reports unavailable if the thread clock cannot
be read. The process mapping check is non-RT and
has some measurement overhead. `WINEDLLPATH` alone is not treated as proof of
which DLL loaded. Daemon PID/generation are checked; hardware XRUNs, generation
changes, probe errors and timeouts abort. PRO/SHARED misses mark the run failed
but do not prevent measuring later workload levels. Probe `PASS` only validates
its own callback/lifecycle checks, not zero daemon misses.

## Measurements (2026-09-06)

All measurements below used the existing daemon PID 145312, generation 0,
Q64/P32, 48 kHz, B256, two-period startup queue and 1000 us handoff ceiling.
No service was restarted or reconfigured. Callback scheduling was observed as
FIFO46 with no scheduling setup error. All listed cases had zero hardware XRUN,
core-miss and generation deltas, and zero callback-period overruns.

### Fixed Callback Work

Installed-driver sweep, two 2-second legs per case, approximately 3020 callbacks:

| Work target | Measured callback mean | PRO miss delta |
| ---: | ---: | ---: |
| 0 us | 1.05 us | 0 |
| 50 us | 51.22 us | 5 |
| 100 us | 101.32 us | 21 |
| 150 us | 151.23 us | 47 |
| 200 us | 201.48 us | 267 |
| 250 us | 251.23 us | 1350 |
| 300 us | 301.27 us | 2478 |
| 200 us, repeat | 201.48 us | 291 |
| 100 us, repeat | 101.35 us | 24 |
| 0 us, repeat | 1.06 us | 2 |

A later short run of the repository harness is particularly useful: a 200 us
target measured 201.07 us mean and **208.9 us maximum**, but still produced
**106 misses over 1509 callbacks**. Its zero-work control had 0 misses over 1507
callbacks. Thus long in-callback excursions are not necessary to reproduce the
problem. These are case-wide counts, not sequence-matched miss attribution.

The 1 ms setting is a ceiling, not the observed usable interval. The sharp rise
around 200-300 us points to insufficient effective turnaround margin in the
current zero-lead path. It does not by itself distinguish a short hardware-derived
cutoff from scheduling/publication delay outside the callback. Per-miss timing
from the opt-in daemon diagnostics is still needed for that distinction.

### Installed Versus Local ASIO

Two 4-second legs per case, approximately 6040 callbacks. Actual mapped paths
confirmed `/usr/local/lib/wine/x86_64-unix/sidealsa-asio64.dll.so` for the installed
driver and repository `build-asio/sidealsa-asio64.dll.so` for the candidate.
The daemon remained the same installed binary; its candidate changes were not
part of this comparison.

| Work target | Installed PRO misses | Local ASIO PRO misses |
| ---: | ---: | ---: |
| 150 us | 104 | 84 |
| 200 us | 620 | 514 |
| 250 us | 2829 | 2766 |

These sequential, nonrandomized runs do not establish a statistically reliable
speedup. Both binaries retain the steep miss increase; client hot-path cleanup
is not a sufficient solution. The installed 150 us case also had one SHARED
underrun. A longer installed 200 us run had 3442 misses over 22673 callbacks,
illustrating variation between runs.

### Background Load

Installed driver, fixed wall-time callback work, 24 workers / 512 MiB:

| Target | Callback mean / max | Callbacks | PRO misses |
| ---: | --- | ---: | ---: |
| 0 us | 1.51 / 23.9 us | 6505 | 0 |
| 100 us | 101.37 / 200.2 us | 6604 | 4 |
| 200 us | 201.30 / 363.4 us | 6373 | 1127 |

The 200 us case had more misses than the unladen 200 us comparison despite a
similar mean and lower maximum callback duration. This is consistent with
effects outside callback execution or changing available budget, but does not
isolate which. Host sleep overshoot changes case duration and callback count;
raw counts should not be compared without those denominators.

Later [bounded capture-spin comparisons](asio-spin.md) reduced misses materially
at 200 us while retaining zero lead. Those results and the user-local deployment
are separate from the older client hot-path comparison above. When testing the
gaming installation, explicitly select `SIDEALSA_ASIO_DLL_DIR="$HOME/.local/lib/wine"`;
the system-wide `/usr/local` installation is a different file.

Separately, the real sine-DSP matrix recorded 7 misses in the pulse-only control,
278 at 256 voices (loaded callback mean 157-166 us), 6 with workers alone, and
1414 for 256 voices plus workers (243-244 us). Native before/after checks had
zero PRO misses in that matrix. Phase changes were recorded separately and are
not used to explain these miss counts.

## Interpretation and Limits

- Reproduction does not require DJMAX/EZ2ON or loopback processing. The common
  PRO path can miss with approximately 200 us callbacks under this configuration.
- FIFO setup failure and a callback exceeding the full Q64 period do not explain
  these runs. This does not exclude additional game-specific problems.
- Increasing the handoff setting alone cannot override the queue-derived clamp.
  Removing the write reserve risks exchanging client misses for hardware faults.
- A larger real playback queue or an explicitly scheduled extra processing stage
  is a meaningful next comparison, with a latency cost. One Q64 stage is 1.333 ms
  at 48 kHz. Neither change was applied or tested here; hardware reconfiguration
  requires separate permission to interrupt the current stream.
- The new daemon diagnostics were not active, so exact per-miss budget and wake
  latency remain unmeasured. No claim of a fully identified or fixed root cause.

Logs: `target/audio-load/20260906-221309` for the DSP matrix and
`target/deadline-sweep/20260906-230855` for the repository harness smoke run.
Earlier sweep logs are temporary: `/tmp/opencode/pro-budget-sweep.fCieds`,
`.BTvpaV`, `.oFTG8Q`, `.Iu3WrK`, and `.92w2rv` (each suffix belongs to the full
`/tmp/opencode/pro-budget-sweep` path).
