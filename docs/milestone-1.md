# Milestone 1: Direct ALSA Engine

Current scope is direct duplex access from `sidealsa-core`.

The engine opens profile-selected playback and capture PCM streams, configures
`S32_LE` interleaved ALSA access at `48 kHz`, `64` frames per period, and a
`192`-frame hardware buffer. It discards capture data and writes zero-filled
playback data.

The RT cycle uses preallocated sample buffers and fixed sample counts. It does
not parse configuration, allocate buffers, use locks, or log.

Realtime scheduling is enabled by default. `device.realtime` controls whether
the engine enters `SCHED_FIFO`; `device.realtime_priority` selects priority
`1..=99`, default `50`. The playback worker uses that priority and capture uses
one level lower. Disable it only for environments without realtime scheduling
rights.

Playback and capture run in independent workers with one owned ALSA PCM per
direction. Streams using the same PCM device are linked for a synchronized
hardware start unless `device.duplex_link = false`; separate devices remain
unlinked unless explicitly enabled. Both workers establish their scheduling
policy before hardware starts. For unlinked devices capture starts first; for
linked devices the playback worker starts the group. Linked streams are then
unlinked so recovery remains direction-local. The timeline
exposes atomic diagnostics for sample
position, transferred playback and capture frames, processed periods, hardware
XRUNs, stream generation, and timeline resets. A detected hardware XRUN is
counted immediately; generation advances only after recovery succeeds. Client
failure statistics and routing are not part of this milestone.

## Build

```text
cargo build --workspace
cargo test --workspace
```

## Hardware Test

Release `hw:OTG,0` from PipeWire first. Then run a finite test:

```text
cargo run -p sidealsa-core --bin sidealsa-hw-test -- --profile profiles/topping-e1x2.toml --periods 15000
```

`7500` periods equals ten seconds at `48 kHz` and `64` frames per period.
Without `--periods`, stop with Ctrl-C. Shutdown drops both PCM streams and
prints diagnostics after the RT loop exits.

## Limitations

- No daemon, IPC, shared memory, client library, or ALSA ioplug.
- No logical split streams in direct engine; profile routing is compiled separately in Milestone 2.
- Capture is discarded.
- Playback is silence only.
- Only `S32_LE` is supported.
- Exact `period_size = 64` and `buffer_size = 192` are required. Devices rejecting that setup fail before streaming.
- E1x2 hardware uses three periods for the current low-latency profile; `64/128` is insufficient for the shared client path.
- Optimized release `64/192` completed `7500` periods with `generation=0`,
  `timeline_resets=0`, `hw_playback_xruns=0`, and `hw_capture_xruns=0`.
- Reported playback and capture positions currently count successful transfers; ALSA hardware pointer queries are not implemented yet.
