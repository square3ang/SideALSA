# ASIO Frontend

`sidealsa-asio` provides an x86_64 Wine/Proton ASIO adapter. Rust owns the
SideALSA PRO client, double-buffered float32 host buffers, worker lifecycle,
sequence handling, and callback dispatch. The C shim supplies the Wine COM,
registration, DLL ABI, and `CreateThread` bridge required for Wine TEB setup.

The adapter expects the daemon profile to expose a `64`-frame period. The
reference E1x2 profile uses `period_size = 64` and `buffer_size = 192` to give
the shared client path three periods of scheduling margin.

ASIO input and output latency each report one operational Q64 period. The
192-frame ALSA value is ring capacity, not queued ASIO software latency.

PRO duplex clients use cycle notifications for a prepared-buffer pipeline. The
playback worker is the sole sequence clock. Before waiting for hardware playback
period `N`, it publishes writable sequence `N + lead`; the capture worker binds
its next completed block to that target. The client may publish playback for
that target any time before the hardware writer consumes it. The hardware
writer never waits for the client.
Missing playback gets one zero-filled fallback period, while stale playback is
discarded and cannot affect later sequences.

Protocol v5 still carries the playback-ready eventfd for compatibility, but the
daemon does not use it for RT scheduling. Shared-memory slot state and sequence
ownership determine whether a playback block is ready.

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
that sequence. The E1x2 profile uses zero additional lookahead; the hardware
transfer interval supplies the callback window without timed client waits in
the daemon or hardware writer.

Poll live counters from another control connection while ASIO owns PRO:

```text
cargo run --release -p sidealsa-cli --bin sidealsa-stats -- --socket /tmp/sidealsad.sock
```

## Verification

- E1x2 direct engine: `7500` periods at `48 kHz`, `64/192`, zero hardware XRUNs.
- Live Wine probe: `3337` callbacks during a five-second run, zero hardware
  XRUNs, zero timeline resets.
- Prepared playback misses are reported by `pro_deadline_misses`. Core timed
  waits no longer exist, so `pro_core_deadline_misses` remains zero.

## Limitations

- x86_64 only; no WoW64 frontend.
- Fixed sample rate and buffer size; no runtime rate switching.
- Float32 ASIO buffers converted to physical S32_LE.
- No control panel.
- Probe requires a running `sidealsad` and physical hardware.
