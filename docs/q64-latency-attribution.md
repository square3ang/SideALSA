# Q64 large-latency-change investigation

The target is the user's approximately **6.8 to 9.0 ms** change, about 106
frames at 48 kHz. The smaller 25-frame / 0.52 ms change is not an explanation
for that larger event.

## Confirmed observations

- The old running daemon, PID 77130, produced an exact native PRO loopback of
  **432 frames / 9.000 ms**, 24/24 pulses, with zero miss/XRUN/reset deltas.
- The same daemon had previously produced 378 frames / 7.875 ms in both Wine
  ASIO and native tests. The earlier 353-frame result was also on that daemon.
- That 9 ms state was lost when the daemon was stopped for the direct-ALSA
  comparison. The user cancelled that comparison; the daemon was restarted.
  There is no completed sustained direct-ALSA result from that aborted run.
- The new direct-mode telemetry samples capture and playback status after
  capture and before PRO publication, every 256 cycles. It now exposes the
  ALSA-reported playback ring/driver-delay split instead of misleading zeros.
  ALSA's USB driver delay is an estimate, not a measurement of the device's
  internal signal path. The two status reads are sequential, not simultaneous.

## Test of accumulated callback lateness

Without restarting daemon PID 132842, six Wine DSP workloads were applied:
four workers at 500, 900 and 1,200 us each, repeated twice. A FIFO-46 native
PRO loopback probe ran before and after each workload.

- Cumulative PRO client misses increased from 0 to **7,906**.
- Hardware XRUNs, timeline resets and SHARED underruns stayed zero.
- All seven loopback probes measured **348 frames / 7.250 ms**, with 24/24
  pulses and no misses during the probes themselves.

This does not reproduce a 2.2 ms increase. It shows that repeated deadline
misses/fallback did not accumulate extra latency under these conditions; it
does not exclude all scheduling or device-side mechanisms.

Evidence: `/tmp/opencode/q64-timing-q9zuqhdh/`, runner
`/tmp/opencode/q64_timing_stress.py`.

## Missing evidence

The low and 9 ms states have not yet been captured with the new synchronized
queue telemetry on the same uninterrupted hardware timeline. Consequently the
large change cannot yet be attributed to playback buffering, capture backlog,
USB transfer scheduling, internal device latency, or a reporting discrepancy.
No one of those is established as the cause.

A bounded one-hour read-only collector was started at
`/tmp/opencode/latency-observer-zqdk4fba/` (initial PID 135730). It samples
`sidealsa-stats` and the reference device's USB stream metadata once a second,
does not reserve PRO or restart anything, and exits if the daemon PID changes.
Its observations must be correlated with the actual loopback measurement when
the high-latency state returns. It cannot measure end-to-end latency by itself.

No latency-reduction workaround or fixed-latency claim follows from these tests.
