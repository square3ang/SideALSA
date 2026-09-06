# PRO Deadline Diagnostics

This is an opt-in diagnostic path for direct event-driven linked PRO. It does
not change buffer geometry, deadlines, fallback, client-miss classification or
the hardware timeline. It is not a fix for game clicks by itself.

## Deployment

The rebuilt daemon accepts `--pro-diagnostics`. Enabling it requires a later
authorized daemon launch; it cannot instrument the already-running installed
binary. Do not launch another hardware owner alongside the existing daemon.
No running service or installed file was changed while implementing this feature.

The flag is rejected for other engine modes before ALSA is opened. The main,
non-RT thread prints one `pro_diagnostics` snapshot per second to stderr and a
final snapshot after a successful shutdown. Under systemd this is journal output.
The hardware thread only updates fixed atomics; it never formats or writes logs.

No `GET_STATS` fields, protocol versions or shared-memory layouts change. This
uses the playback publication timestamp already supplied by shipped clients, so
new ASIO binaries are not required solely for these daemon diagnostics.

## Recorded Data

`last_miss` is a coherent record for one counted PRO miss:

| Field | Meaning |
| --- | --- |
| `generation`, `session_id`, `sequence` | Identity of the missed playback block. |
| `wait_entry_budget_nanos` | Remaining time when the direct client wait was entered, not the configured maximum. `Some(0)` means exhausted; `None` means no matching wait was recorded. |
| `cutoff_nanos` | Absolute monotonic acceptance deadline, if supplied. |
| `selection_started_nanos` | Monotonic time before the daemon began selecting the block. |
| `published_nanos` | Existing exact-slot publication timestamp, if observed and nonzero. |
| `capture_to_publish_nanos` | Publication minus the matched core capture-read timestamp, if available and ordered. Includes daemon capture delivery, client scheduling, callback and output publication work. |
| `late` | An exact block was found but rejected by the publication-time cutoff. False means the unsuccessful selection was Missing. |
| `core` | Existing core-miss classification; false is the existing client-miss bucket. This does not establish which component caused a scheduling stall. |

For a timestamped late block, `published_nanos - cutoff_nanos` gives timestamp
lateness. `selection_started_nanos - cutoff_nanos`, when positive, shows how late
selection began. A zero timestamp is rejected as Late but reported as unknown,
not fabricated as an elapsed duration.

`accepted_roundtrip_samples` and `accepted_roundtrip_max_nanos` summarize matched
capture-read-to-publication timing of accepted blocks. They are cumulative over
the daemon lifetime, across client sessions, and are not a per-game callback
maximum. Their counters are approximate independent atomic observations; only
the last-miss record is a coherent multi-field snapshot.

## Limits

- This is a last-miss snapshot, not an event trace. `misses_recorded` counts all
  recorded misses; multiple misses between prints overwrite the earlier record.
  Readers retry at most three times and skip a snapshot if the writer is busy.
- A block missing at selection has no publication timestamp to report. Its later
  arrival and stale-slot reclamation are not traced. Missing timing is `None`.
- Publication is timestamped just before READY is made visible. Preemption in
  that small interval means it is not an exact timestamp of visibility.
- Capture timestamps are retained in eight sequence-tagged core slots. Missing,
  overwritten or reversed matches produce unknown timing. Exact diagnostic
  correlation is supported only on the direct, single-hardware-thread path.
- The extra timestamps and atomics have overhead. Diagnostics are disabled by
  default; compare enabled and disabled behavior before claiming performance
  equivalence. No game-load or live-hardware validation of this feature has yet
  been performed.

Hardware-free tests cover Ready/Late/Missing publication metadata, zero
timestamps, slot reuse, unknown timing, matched zero wait budgets, unchanged
fallback/counters and coherent snapshots under concurrent reading.
