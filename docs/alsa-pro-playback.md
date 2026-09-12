# ALSA PRO Playback Optimization

Current behavior supports independent PRO application buffers and aligned starts:
see [ALSA PRO buffers](alsa-pro-buffers.md). The original B256-only behavior below
is retained as measurement history.

For the later measured buffering/startup latency comparison, see
[ALSA PRO ioplug latency investigation](alsa-pro-latency.md). The historical
silent deadline tests below did not measure end-to-end latency.

This change targets playback-only ALSA handles opened on `sidealsa_pro`. It does
not add the ASIO spin strategy to ALSA, change an application's scheduling policy,
increase its FIFO, or change Q64/P32, zero lead, the daemon queue or write reserve.

## Cause and Change

The ioplug previously used duplex capture acquisition to observe PRO cycles,
then copied all physical input channels into a discard buffer. For playback-only
clients, already-queued output can be consumed before the application wakes; the
matching capture block then becomes expired under duplex rules. Requiring valid
capture at that point unnecessarily postpones submission of ready playback data.
This is particularly harmful to applications that poll, render, then write.

`AudioStream::wait_pro_playback_period` now observes the existing daemon-owned
cycle sequence directly and discards unused input without copying it. It:

- Skips the activation cycle and an already-consumed first playback cycle.
- Reclaims unused input even when startup cannot yet return a cycle, preventing
  a full capture ring from suppressing subsequent producer notifications.
- Preserves future capture slots, lifecycle/hardware-generation checks and
  notification handling; zero-timeout calls never sleep.
- Returns the observed cycle without adding a sequence lead. The ioplug retains
  its existing FIFO sequence assignment, maximum publishable sequence and EPIPE
  policy for genuinely expired playback. It explicitly checks the daemon's
  playback-consumption watermark rather than using capture validity as a proxy.
  Frames are not relabelled to hide loss.

ALSA playback also submits directly from its preallocated FIFO, removing an
intermediate scratch copy/allocation. Capture handles keep their scratch buffers
and existing acquisition path. SHARED keeps its existing wait and scheduling
policy; only the redundant playback copy is removed there.

The ALSA application's existing B256 buffer and queued playback distinguish this
route from ASIO's same-cycle host callback. These results do not imply that an
ASIO callback can now spend 1100 us under the unchanged zero-lead deadline.

## Probe

The hardware-free compile check is:

```sh
bash scripts/test-alsa-pro.sh --compile-only
```

With PRO available, explicitly run silent S32_LE, 8-channel playback:

```sh
bash scripts/test-alsa-pro.sh --run installed 1500 500 poll 46
ALSA_PLUGIN_DIR="$PWD/target/release" \
  bash scripts/test-alsa-pro.sh --run local 1500 500 poll 46
```

Arguments are periods, per-period work in microseconds, `blocking`/`poll`,
requested RT priority `0`/`46`, and optional prefill periods `1..4` (default 4).
The external-poll model performs work **after**
waking, before writing. The blocking model performs work before `writei`.
The probe prefills the existing B256 application buffer and explicitly starts;
it reports partial writes, EAGAIN, client EPIPE/recovery, CPU and wall time.
It uses a private PRO definition for `/tmp/sidealsad.sock`; no device settings or
services are modified. Its final queued silence is dropped, not drained.

The runner saves logs under `target/alsa-pro`, checks daemon PID/generation and
reports separate PRO/HW/SHARED deltas. An owned-probe watchdog defaults to 30 s
(`SIDEALSA_ALSA_PRO_TIMEOUT`); it does not kill other audio processes. Actual
scheduler output must be checked when comparing RT runs.

## Measured Results (2026-09-07)

All cases retained installed daemon PID 145312 / generation 0, with no hardware
XRUNs, timeline resets or SHARED miss deltas. No existing user app or service was restarted.

| Plugin / model | Work | Requested periods | ALSA EPIPEs | PRO misses | Wall time |
| --- | ---: | ---: | ---: | ---: | ---: |
| Original, poll then work, FIFO46 | 500 us | 1500 | 749 | 749 | 6.00 s |
| Candidate, same model | 500 us | 1500 | 0 | 0 | 2.00 s |
| Original, repeated | 500 us | 1500 | 749 | 749 | 6.00 s |
| Final candidate, repeated | 500 us | 1500 | 0 | 0 | 2.00 s |
| Final candidate, poll then work, FIFO46 | 1100 us | 3000 | 0 | 0 | 4.00 s |
| Final candidate, blocking, FIFO46 | 500 us | 3000 | 0 | 0 | 4.00 s |
| Final candidate, poll then work, ordinary scheduling | 500 us | 3000 | 0 | 0 | 4.00 s |
| Candidate, intentional overload | 2000 us | 300 | 149 | 299 | 1.60 s |

The overload still reports real client failure instead of concealing it. The
additional one-period-prefill test also reported ALSA EPIPEs, and unit tests
verify output already below the consumption watermark cannot be silently
submitted as current playback. The
500 us comparison used approximately 1.18 s total thread CPU before versus
0.76 s after. CPU percentage rises because the fixed amount of work completes
in two seconds rather than spending six seconds recovering; no busy-spin was
added. EAGAIN may occur as normal partial/nonblocking progress and is not itself
a hardware XRUN.

Earlier prototype runs performed work before polling and did not reproduce the
failure. They are not used as evidence for the after-poll comparison above.

Decisive logs: `target/alsa-pro/20260907-011855-installed`, `011916-local`,
`012959-local`, `013012-installed`, `013038-local`, `013048-local`,
`013101-local`, and `013258-local` (all suffixes use the same date prefix).

## Deployment and Limits

The verified plugin was atomically installed at
`/usr/lib/alsa-lib/libasound_module_pcm_sidealsa.so` using
`scripts/install-alsa-plugin.sh`. Only its matching installer-manifest hash was
updated. Existing mappings, daemon, PipeWire, profile and ASIO installation were
left intact. Existing applications may retain the old module until a later
process launch; osu itself was not launched or terminated for these tests.

Backup: `target/alsa-plugin-backup.EDvVsv` contains the original plugin and
manifest; `target/alsa-plugin-backup.eSOj6b` retains the intermediate version before
the explicit output-watermark guard. The narrow installer verifies managed ownership, preserves other
manifest bytes and conditionally rolls back a failed manifest update. Its
hardware-free failure/concurrent-edit tests are in
`scripts/test-install-alsa-plugin.sh`.

Post-install system-plugin verification: 3000 periods, 500 us work after poll,
0 ALSA EPIPEs, 0 PRO/HW/SHARED miss/reset deltas in 4.00 s. Log:
`target/alsa-pro/20260907-015727-installed`. The final candidate and one-period
prefill checks are in `20260907-015438-local` and `20260907-015450-local`.

These are silent synthetic tests with the raw full-channel PRO PCM, not a
measurement of osu's actual backend, stereo conversion layer or audible sample
continuity. There is no new guarantee against genuine application scheduling
stalls, and end-to-end loopback latency was not measured. ASIO capture semantics
and already-installed ASIO spin behavior are unchanged.
