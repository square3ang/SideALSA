# Bounded ASIO Capture Spin

The ASIO worker can wake shortly before the next predicted capture arrival and
poll shared-memory readiness within a bounded window. This trades CPU time for
less dependence on a notification wake at the last moment. It does not change
Q64/P32, the hardware queue, playback sequence, zero lead, deadline acceptance or
the hardware write reserve. It is never used by the hardware thread.

## Policy

- Default: a **200 us half-window** for zero-lead clients whose worker obtains RT
  scheduling. Other latency modes default to zero.
- `SIDEALSA_ASIO_SPIN_US=0` disables the feature. Explicit values 0-250 select the
  half-window in microseconds. Invalid values fail worker startup.
- No spinning occurs if the worker's RT request is disabled or fails, even with
  an explicit nonzero setting.
- Each half-window is additionally capped at one quarter of the logical period.
  At 48 kHz/Q64, the selected default permits at most 400 us of spinning around
  one predicted arrival, not continuous full-period polling.
- Prediction is anchored to observed capture acquisition, not a free-running
  audio clock. Sequence gaps and host lifecycle changes invalidate prediction.
  A stopped host does not spin. READY acquisition still performs normal expiry
  and hardware-generation checks.
- Outside the window the existing notification wait remains in use. Stop and
  lifecycle commands are checked during spinning. Control disconnection remains
  checked before callback dispatch. Interruptions never extend the fixed window.

The default is a CPU/latency tradeoff, not a guarantee against missed deadlines.
Actual output latency was not measured by the silent tests below; no buffering
was intentionally added and ASIO-reported latency remains unchanged.

## Measurements (2026-09-07)

All runs retained daemon PID 145312, generation 0, Q64/P32/B256, prime128,
zero lead, handoff ceiling 1000 us and the existing write reserve. The same local
ASIO build was compared with explicit OFF and spin settings. Loaded module paths
and FIFO46 callback scheduling were observed. No game, daemon or PipeWire was
restarted. No hardware or SHARED failure-counter deltas occurred in these runs.

For approximately 6035 callbacks with a fixed 200 us workload:

| Half-window | PRO misses | Worker CPU, percent of one core |
| ---: | ---: | ---: |
| OFF, initial | 158 | 16.064 |
| 50 us | 89 | 18.362 |
| 100 us | 28 | 21.874 |
| 200 us | 0 | 30.135 |
| OFF, repeat | 106 | 15.973 |

The CPU metric reads the actual callback thread's CPU clock at the first
benchmark callback and again from the non-RT reporting path after Stop. It
includes work between callbacks, not just callback execution, over both Start
legs and the intervening stopped interval. Clock failures report unavailable
rather than fabricated zero CPU usage.

The decisive loaded comparisons used 24 same-process background workers and
512 MiB, with callback work still fixed at 200 us:

| Order / mode | Callbacks | PRO misses | Misses per callback | Worker CPU |
| --- | ---: | ---: | ---: | ---: |
| Short pair, OFF | 6507 | 1022 | 15.71% | 17.594% |
| Short pair, 200 us | 6458 | 46 | 0.71% | 30.941% |
| Longer reverse pair, 200 us | 16049 | 125 | 0.78% | 31.017% |
| Longer reverse pair, OFF | 16281 | 2067 | 12.70% | 17.404% |

The longer pair reduced normalized misses by approximately 94%, at a cost of
about 13.6 percentage points of **one core**, not of the whole machine. This
supports a benefit from the wait strategy under the tested load, but does not
directly measure notification-to-acquisition latency or prove its exact mechanism.

The improvement is not universal or complete. At 250 us work, OFF/spin recorded
2337/2029 misses in approximately 6035 callbacks; at 300 us, 4810/4250. At 150 us
without background workers the counts were 0/1, with one longer callback in the
spin run; with workers they were 7/0. Longer callbacks can still exceed the
available zero-lead budget. DJMAX/EZ2ON themselves were not launched for these
comparisons, so their actual improvement remains to be checked in normal use.

Logs are under `target/deadline-sweep/20260907-*`: `000259`, `000321`, `000340`,
`000400`, `000431`, `000601`, `000655`, `000750`, `000821`, `000920`, `000954`.

## Applied Installation

DJMAX (960170) and EZ2ON (1477590) launch options select
`WINEDLLPATH="$HOME/.local/lib/wine"`. Their user-local Unix module was byte-for-byte
identical to the old `/usr/local` module before deployment, but is a separate
file. Only the user-local installation was updated:

`~/.local/lib/wine/x86_64-unix/sidealsa-asio64.dll.so`

The ASIO-only installer now uses same-directory atomic replacement, preserving
old inodes for already-loaded processes. The unchanged PE stub matched both game
prefix copies, so no prefix registration or game-file update was needed. Neither
the system-wide `/usr/local` module nor the installed daemon was replaced.

Backup: `target/asio-spin-backup.HAqZQf/sidealsa-asio64.dll.so`.

Post-install verification loaded the user-local module with no spin environment
override: 6034 callbacks at 200 us work, 0 PRO/HW/SHARED misses/resets, 30.162% of
one core. Log: `target/deadline-sweep/20260907-002845`. The local default was also
verified before installation in `20260907-002457`.

The new default applies on the next game process launch. Existing processes keep
their loaded code. To disable on a later launch, prepend
`SIDEALSA_ASIO_SPIN_US=0` to the existing Steam launch options; do not remove the
existing `WINEDLLPATH` or other options.

Hardware-free validation includes workspace tests and strict Clippy, prediction
gap/wrap/lifecycle tests, stop/expiry checks, non-consuming readiness hints, the
probe self-test, and `scripts/test-install-asio.sh` for atomic replacement.
