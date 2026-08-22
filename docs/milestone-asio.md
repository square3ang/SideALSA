# ASIO Frontend

`sidealsa-asio` provides an x86_64 Wine/Proton ASIO adapter. Rust owns the
SideALSA PRO client, double-buffered float32 host buffers, worker lifecycle,
sequence handling, and callback dispatch. The C shim only supplies the Wine
COM, registration, and DLL ABI.

The adapter expects the daemon profile to expose a `64`-frame period. The
reference E1x2 profile uses `period_size = 64` and `buffer_size = 192` to give
the shared client path three periods of scheduling margin.

## Build

```text
cmake -S crates/sidealsa-asio -B build-asio -DCMAKE_BUILD_TYPE=Release
cmake --build build-asio
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
S32-to-float channel metadata, buffer pointers, start/stop, and callback flow.
It returns `77` when daemon connection is unavailable.

The ASIO worker primes two silence blocks at the current daemon sequence before
entering its callback loop. This prevents startup from leaving the first
hardware period without a playback block.

Poll live counters from another control connection while ASIO owns PRO:

```text
cargo run --release -p sidealsa-cli --bin sidealsa-stats -- --socket /tmp/sidealsad.sock
```

## Verification

- E1x2 direct engine: `7500` periods at `48 kHz`, `64/192`, zero hardware XRUNs.
- Live Wine probe: `3337` callbacks during a five-second run, zero hardware
  XRUNs, zero timeline resets.
- Latest probe separates client misses from core callback scheduling misses;
  client `pro_deadline_misses` stayed at zero while core misses remain reported
  by `pro_core_deadline_misses`.

## Limitations

- x86_64 only; no WoW64 frontend.
- Fixed sample rate and buffer size; no runtime rate switching.
- Float32 ASIO buffers converted to physical S32_LE.
- No control panel or output-ready implementation.
- Probe requires a running `sidealsad` and physical hardware.
