# SHARED Timing and Capture Accounting

## Applied Changes

The reference profile now uses `shared_latency_periods = 5`, down from 7. This
advances scheduled SHARED playback consumption by two Q64 periods (128 frames,
2.667 ms at 48 kHz) for the same sequence assignment. It does not establish a
fixed reduction in measured end-to-end latency across different stream starts.

Physical Q64/P32 operation, B256 hardware capacity, the two-period hardware
startup queue, zero-lead PRO, PRO's eight slots and the hardware write reserve
are unchanged. SHARED playback retains its eight-slot B512 ring, external Q256 /
B768 geometry, 128-frame PipeWire headroom and 256-frame startup delay.

Three adapter/transport corrections accompany the shorter playback offset:

1. SHARED playback can observe a newly published daemon cycle before its eventfd
   hint arrives. It no longer postpones an already-prepared batch merely because
   that hint is not yet readable. Activation, generation, duplicate-cycle and
   timeout checks remain, as does the existing sequence assignment.
2. SHARED capture availability counts complete published frames, scratch frames
   and frames already retired. The C shim tracks ALSA application-pointer changes;
   `snd_pcm_forward()` now discards the corresponding actual samples, including
   partial periods. Partial-transfer errors preserve the returned prefix and are
   latched until prepare. Invalid/backward cursor changes require recovery rather
   than inventing sample time.
3. SHARED capture reserves twice the configured base buffer: 4-16 slots for a
   base of 2-8 periods. At the reference B512 setting that is **16 slots / B1024**.
   The external capture period stays Q256; capture minimum/maximum ALSA buffer
   capacity is B1024. PRO and SHARED playback allocations remain eight slots.

The existing handshake already carries a variable slot count and layout size;
no protocol fields or layout version changed. An installed, older
`sidealsa-shared-test` binary successfully read 750 capture blocks after the
change. Extra capture storage can absorb startup/scheduling stalls, but it is
not a promise of smaller input latency: actual occupancy still matters.

PipeWire capture adapters now request 64 frames of headroom, matching the Q64
publication granularity. The previous configured zero became an effective
32-frame minimum in PipeWire's timer-driven capture path. The new setting adds
32 frames of nominal capture margin while the SHARED playback offset removes
128 frames. The daemon never waits for SHARED data.

## Measurements (2026-09-07)

The test connected `jack_delay:out` through PipeWire to
`sidealsa-line1:playback_FL`, with the device's internal digital return read from
`sidealsa-input56:capture_FL`. This is a full software/device-return round trip,
not analog converter latency or playback-only latency. The graph stayed Q256.

Loaded cases ran a silent PRO ASIO client with 100 us callback work and 24
same-process background workers / 512 MiB. Its two nominal 15-second legs ran
inside 35- or 60-second SHARED measurement windows, covering activation and stop
transitions as well as steady operation. Existing workloads were not isolated.
Each run has its own before/after counter snapshots; counters across different
daemon PIDs were never subtracted.

Important observations, including failed candidates:

| Stage | SHARED playback misses | Capture overruns | Device-return RTT |
| --- | ---: | ---: | --- |
| Original 7-period baseline, long-running daemon | 0 | 0 | 25.958 ms |
| 6 periods alone, after restart | 0 | 4 | 27.17-31.17 ms |
| Restored 7 periods, after restart | 0 | 4 | 27.40-30.06 ms |
| Capture accounting candidate, headroom 0, repeated control | 0 | 4 | 28.63-29.96 ms |
| Same daemon/candidate, capture headroom 64, repeated | 0 | 0 | 25.958 ms |
| 5 periods before the later transport/reserve changes | 29 | 4 | 26.79-29.46 ms |
| 6 periods/headroom 64 before playback clock fast path, longer run | 9 | 4 | 25.46-29.96 ms |
| Playback fast path, 6 periods, two later runs | 0 / 0 | 0 / 3 | Variable |
| Capture reserve added, 6 periods, two 60-second runs | 0 / 0 | 0 / 0 | 23.77-30.44 ms |
| Final 5-period combination, two 60-second runs | **0 / 0** | **0 / 0** | 29.208 ms / 25.208 ms |

All listed runs had zero hardware XRUN and timeline-reset deltas. Some heavily
loaded runs recorded one or two PRO misses; the final pair had zero and one PRO
client miss respectively. The final PipeWire playback, capture and JACK node
error counts were zero. These observations do not prove zero PRO impact under
every workload or a universal optimum at five periods.

Digital return latency varies independently of counted XRUNs, including user
observations on Windows. The data does **not** demonstrate an invariant lower
round-trip latency: the final runs differ by 4 ms. The reliable claims are a
shorter configured SHARED playback offset, corrected capture bookkeeping, and
zero SHARED failures in the two final bounded load tests. A consistently lower
playback-only latency still needs a dedicated, synchronized one-way measurement.

The capture-reserve deployment also rebuilt the daemon from the current tree,
including earlier deadline-protection changes not present in its previously
installed binary. That comparison is not an isolated proof that capacity alone
caused every observed improvement.

## Current Installation and Logs

Installed profile: `/etc/sidealsa/profiles/topping-e1x2.toml`, matching the
reference profile's five-period SHARED offset. Capture headroom is persisted in
`/etc/pipewire/pipewire.conf.d/99-sidealsa.conf`, not merely a runtime parameter.
The daemon and ALSA plugin were atomically updated; their managed-file hashes
were updated without rewriting unrelated manifest entries. Audio services were
restarted with the user's permission. ASIO binaries and application launch
options were not changed.

Original settings: `target/shared-tuning-backup.Um4eRK/profile.toml` and
`pipewire-original.conf`. Original pre-experiment plugin backup:
`target/alsa-plugin-backup.Q6aNra/plugin.so`. Original daemon backup:
`target/alsa-plugin-backup.W8Dnyu/plugin.so` (the updater's generic backup name).

Representative logs under `target/`:

- `shared-rtt.4DDvaa`: original loaded baseline.
- `shared-rtt.d2DNIu`, `shared-rtt.30Ah0Y`: initial 6/7-period restart controls.
- `shared-rtt.btkMhp`, `shared-rtt.dDegRm`: headroom 0/64 comparison.
- `shared-rtt.wvcHN2`: failed longer 6-period candidate.
- `shared-rtt.2OnB2E`, `shared-rtt.s8yuhy`, `shared-rtt.EpE23n`: playback fast-path comparisons, including residual failures.
- `shared-rtt.WTn44J`, `shared-rtt.qRfQSL`: added capture reserve.
- `shared-rtt.Vhi9Uj`, `shared-rtt.duLuFb`: final five-period combination.

The experiment helpers remain in `/tmp/opencode/shared-rtt.sh` and
`/tmp/opencode/change-shared-latency.sh`; these are temporary host-specific
helpers, not portable acceptance tests. Raw latency output, PipeWire node
snapshots, PRO workload output and counter snapshots are retained in each log
directory. Unit tests cover publication/accounting, partial reads, forwarding,
cursor wrap/rejection, full rings, prepare/recovery, dynamic slot counts and
geometry preservation.
