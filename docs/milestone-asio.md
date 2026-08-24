# ASIO Frontend

`sidealsa-asio` provides an x86_64 Wine/Proton ASIO adapter. Rust owns the
SideALSA PRO client, double-buffered float32 host buffers, worker lifecycle,
sequence handling, and callback dispatch. The C shim supplies the Wine COM,
registration, DLL ABI, and `CreateThread` bridge required for Wine TEB setup.

The adapter expects the daemon profile to expose a `64`-frame logical period.
The reference E1x2 profile uses physical ALSA Q32, aggregates transfers into
Q64 client cycles, and keeps `buffer_size = 192` for scheduling margin.

ASIO input latency reports one operational period. Output latency reports the
configured PRO lookahead in periods. The 192-frame ALSA value is ring capacity,
not queued ASIO software latency.

PRO duplex clients use cycle notifications for a prepared-buffer pipeline. The
playback worker defines the initial sequence target. Synchronized duplex start
fixes capture/playback hardware phase, and capture advances one sequence for
each completed period. A real hardware-generation change rebases it to the
playback target. The client may publish playback for
that target any time before the hardware writer consumes it. The writer prepares
output before its ALSA wait and may block on `playback_ready` only for the budget
remaining before the hardware reserve. Missing playback then gets one
zero-filled fallback period, while stale playback is discarded and cannot
affect later sequences. The ALSA write deadline and queue guard never move for
the client.

Protocol v10 carries the playback-ready eventfd, timing diagnostics, and
physical hardware period, while shared-memory v4 adds the daemon's
authoritative playback watermark.
Shared-memory slot state and sequence ownership remain authoritative for
whether a playback block is ready.
The client chooses the oldest capture target not older than that watermark, so
a newly published future block cannot displace an exact block the daemon still
needs. Sequence gaps advance sample position and double-buffer parity before
callback dispatch.
Playback keeps its original sequence; the daemon discards it if its exact
deadline has already passed.

The reference profile keeps ALSA hardware `buffer_size = 192`. E1x2 experiments
with delayed period writes fail even with a 32-frame guard. PipeWire's 96-frame
`max_delay` is not equivalent to a SideALSA queue threshold; reproducing it
requires PipeWire-style explicit-start and timer scheduling rather than a lower
ALSA availability threshold.

## Build

```text
cmake -S crates/sidealsa-asio -B build-asio -DCMAKE_BUILD_TYPE=Release
cmake --build build-asio --target sidealsa-asio sidealsa-asio-probe
```

Outputs include `sidealsa-asio64.dll`, `sidealsa-asio64.dll.so`, and the
`sidealsa-asio-probe.exe` host. The build tree also contains Wine's
`x86_64-windows` and `x86_64-unix` lookup layout.

## Probe

Register the PE half in a test Wine prefix, then run with the Unix half on
`WINEDLLPATH`:

```text
cp build-asio/sidealsa-asio64.dll "$WINEPREFIX/drive_c/windows/system32/"
WINEDLLPATH="$PWD/build-asio" wine regsvr32 /s sidealsa-asio64.dll
SIDEALSA_SOCKET=/tmp/sidealsad.sock \
WINEDLLPATH="$PWD/build-asio" \
"$PWD/build-asio/sidealsa-asio-probe.exe"
```

The probe checks COM contracts, channel counts, `64`-frame buffer negotiation,
S32-to-float channel metadata, buffer pointers, restart, callback flow, and
that callback stack addresses belong to the Wine-created worker thread.
It returns `77` when daemon connection is unavailable.

ASIO begins with the first playback-clock target and publishes playback under
that exact sequence. The E1x2 profile keeps one safe engine period. The bounded
ready barrier lets the lower-priority callback run while ALSA drains queued
audio. The daemon derives each wait budget from current ALSA availability and
reserves one quarter-period for the hardware write.

The ASIO frontend owns its callback thread and attempts to raise it to the
profile's `device.pro_realtime_priority`. When omitted, it defaults to two below
the hardware worker; the reference profile therefore uses `86` below capture
`87` and playback `88`. It continues with normal scheduling and increments
`pro_realtime_failures` when the Wine process lacks realtime rights. Native ALSA
clients retain application-owned scheduling because the plugin cannot safely
promote arbitrary host threads.

Poll live counters from another control connection while ASIO owns PRO:

```text
cargo run --release -p sidealsa-cli --bin sidealsa-stats -- --socket /tmp/sidealsad.sock
```

## Verification

- E1x2 direct engine: `7500` periods at `48 kHz`, `64/192`, zero hardware XRUNs.
- Live Wine probe: `3337` callbacks during a five-second run, zero hardware
  XRUNs, zero timeline resets.
- Prepared playback misses are reported by `pro_deadline_misses`. The bounded
  daemon barrier preserves the hardware deadline, so `pro_core_deadline_misses`
  remains zero.
- Client diagnostics expose expired capture blocks, playback publication
  failures, realtime promotion failures, callback overruns, and maximum callback
  duration without logging from the audio thread.

## Limitations

- x86_64 only; no WoW64 frontend.
- Fixed sample rate and buffer size; no runtime rate switching.
- Float32 ASIO buffers converted to physical S32_LE.
- No control panel.
- Probe requires a running `sidealsad` and physical hardware.
