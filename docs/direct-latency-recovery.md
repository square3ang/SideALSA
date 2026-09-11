# Direct-stream latency recovery

This is mandatory engine recovery in the event-driven linked zero-lead path,
not a profile option. There is no `direct_latency_recovery` setting or GUI toggle.

## Reproduced failure

The user's increased latency was independently measured at **425 frames /
8.854 ms** on the existing Q64/P64/B256 stream. A controlled short pause of the
hardware process then isolated the failure on fresh streams:

| Requested pause | Before | After | ALSA playback/capture XRUNs |
| --- | ---: | ---: | ---: |
| 3.5 ms | 323 frames / 6.729 ms | 390 frames / 8.125 ms | 0 / 0 |
| 4 ms (measured stop interval 4.098 ms) | 341 frames / 7.104 ms | 456 frames / 9.500 ms | 0 / 0 |
| 4.5 ms | 329 frames / 6.854 ms | 323 frames / 6.729 ms | 1 / 1 |
| 5 ms | 323 frames / 6.729 ms | 329 frames / 6.854 ms | 1 / 1 |

The shorter pauses left extra playback buffering/duplex phase delay without an
ALSA error, and the engine previously kept that higher-latency state forever.
Longer pauses happened to cause a reported ALSA XRUN, so the existing recovery
re-primed the stream and restored its normal latency range. The outcome also
depends on the phase at which the pause lands; pause duration alone is not an
XRUN threshold.

Thus the demonstrated software defect is **missing recovery for a retained
queue-depth shift after a missed hardware cycle when ALSA still says RUNNING**.
This does not identify the exact IRQ/kernel scheduling path behind the user's
game-time stall, nor prove a device firmware fault. The controlled reproduction
does establish that no device replacement or client deadline miss is necessary
to produce a roughly 2.4 ms persistent increase.

Evidence: `/tmp/opencode/short-stall-dcg_7t48/`. A candidate replacing
`avail_update()` with `avail()` performed worse (timeouts and misses) and was
discarded; it is not part of the fix (`short-stall-9hosh54i/`).

## Correction

`crates/sidealsa-core/src/latency.rs` tracks hardware progress with fixed storage:

1. Ignore the first second of a hardware generation while startup settles.
2. Learn a median baseline from nine paired playback/capture delay observations,
   taken after capture and before PRO publication. Sampling is approximately
   12 Hz and scales with the logical period.
3. Record a suspect hardware stall only when ready-to-completion exceeds one
   logical period, or successive ready observations are more than two periods
   apart. A client-miss counter is never an input.
4. Require two consecutive nine-sample windows showing at least one full logical
   period of additional delay after the stall. Single spikes and smaller phase
   changes do not trigger recovery. The eligibility window is bounded and
   long enough for the observation windows at large periods.
5. Re-prepare and re-prime the actual linked ALSA stream before publishing the
   next capture block. No process restart, buffer-size increase, sample-pointer
   falsification, or silent sample-dropping correction is used.

This is a real stream rebase, with a brief audio discontinuity. It increments
`generation`, `timeline_resets`, and `linked_phase_rebases`; it does not invent
an ALSA XRUN. Clients use the existing hardware-generation recovery path.
The OS can still deschedule the hardware worker; the correction prevents the
resulting increased buffering from remaining unhandled.

No allocations, locks, formatted logging, or filesystem access occur in the
guard. It uses a nine-element array and atomics already provided by the timeline.
ALSA status uses fixed stack storage. Deadline calculations still charge this
work to the same absolute time budget.

The detector requires a learned baseline and ALSA delay observations. It cannot
infer an absolute device round-trip latency at startup, and it does not enforce
an exact universal 6.x ms target. Non-direct scheduling paths retain their
existing recovery behavior.

## Verification

With the candidate, a 3.578 ms controlled pause produced no ALSA XRUN but one
automatic rebase. The subsequent native loopback was **335 frames / 6.979 ms**,
with all 16 pulses returned and no PRO misses. Other tested pauses that caused
actual ALSA XRUNs recovered through the existing path. Evidence:
`/tmp/opencode/short-stall-21i7adb_/`.

A separate run kept real Wine ASIO and SHARED playback/capture open during the
fault and automatic recovery:

- Same daemon PID 176720 throughout.
- `linked_phase_rebases` and `generation` increased by one.
- ALSA playback/capture XRUNs, PRO misses and SHARED underruns/overruns stayed zero.
- ASIO delivered 9,001 callbacks over the 12-second test with zero probe errors.
- Both PipeWire test clients remained alive, and digital-return capture continued
  with the expected 0.005-peak sine (tail RMS approximately 0.0035355).
- No PipeWire/Pulse/WirePlumber or application restart occurred during recovery.

Evidence: `/tmp/opencode/latency-recovery-live-r3bis3hr/`. These bounded tests
validate the reproduced mechanism and client continuation, not every DAW or
driver. The raw phase change is preserved in the timeline statistics.

After installation, two consecutive **60-second real Wine ASIO loopback legs**
each returned all 704 pulses at **312 frames / 6.500 ms**, with identical minimum
and maximum latency. Across both legs, PRO misses, ALSA XRUNs, SHARED losses,
timeline resets and linked phase rebases stayed zero. The normal-run test thus
did not repeatedly reset the stream to obtain the low latency. Evidence:
`/tmp/opencode/q64-asio-drur74ai/`. The installed profile was preserved and the
recovery is always active in the direct path without a configuration key.
