# Milestone 5: SHARED Path

## Scope

`sidealsad` now precreates one shared-memory endpoint per configured logical
port. PRO remains one exclusive raw multichannel endpoint. SHARED endpoints
may run concurrently with PRO and with different SHARED ports.

Each logical port has its own memfd, capture eventfd, playback eventfd, owner,
and active state. Playback ports expose logical channels. Capture ports expose
mapped physical input channels. Inactive direction uses zero channels.

The RT bridge performs fixed channel mapping and saturating `i32` addition:

- PRO output is the raw physical playback block.
- Active SHARED playback blocks are added to mapped physical channels.
- Active SHARED capture ports receive mapped logical blocks.
- The first missing SHARED playback block in an armed episode increments
  `shared_underruns` and contributes silence. The next valid block rearms it.
- Full SHARED capture rings increment `shared_overruns`.

SHARED misses never increment PRO misses, hardware XRUN counters, generation,
or timeline resets.

## Control

`OPEN_SHARED { port_id }` returns session ID, direction, shared layout, and four
file descriptors through `SCM_RIGHTS`: memfd, capture eventfd, playback eventfd,
and playback-ready eventfd. One client owns each logical port.
PRO ownership remains independent. Disconnect releases either session.

## Verification

```text
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

`sidealsa-shared-test` consumes playback cycle eventfds and fills SHARED
playback slots through `sidealsa-client`.

Playback stress:

Terminal 1:

```text
target/debug/sidealsad --profile profiles/topping-e1x2.toml --socket /tmp/sidealsad.sock
```

Terminal 2:

```text
target/debug/sidealsa-shared-test --socket /tmp/sidealsad.sock --port line1 --periods 3000 --delay-ms 2 --delay-every 16
```

Capture mapping check:

```text
target/debug/sidealsa-shared-test --socket /tmp/sidealsad.sock --port mic1 --periods 3000
```

Expected delayed playback result: `shared_underruns` may increase; hardware
XRUNs, PRO misses, generation, and timeline resets remain zero.

An endpoint arms only after its first exact playback block. A missing block
disarms it after recording one underrun, so an inactive or paused PipeWire node
does not repeatedly count the same outage at the hardware period rate.

Observed on Topping E1x2 at Q32 with profile realtime scheduling: 1500 delayed playback
periods produced 95 shared underruns, zero hardware XRUNs, zero PRO misses, and
zero timeline resets. 3000 capture periods produced zero shared overruns.

## Limitations

- No SHARED ALSA ioplug yet.
- No PipeWire integration yet.
- One owner per logical port.
- No mixing policy beyond channel mapping and saturating sum.
