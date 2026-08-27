# Milestone 2: Device Profiles and Splitting

`sidealsa-config` owns TOML loading and profile validation. `sidealsa-core`
compiles validated port definitions into an immutable `RoutingTable` before
opening ALSA. RT code receives channel indices only; it never parses TOML or
looks up strings.

Physical streams remain unchanged:

- playback: one 8-channel stream
- capture: one 10-channel stream

Logical ports are channel views into those streams. No split gets its own
thread or loopback.

## Build

```text
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Inspect Ports

```text
cargo run -p sidealsa-core --bin sidealsa-hw-test -- --profile profiles/topping-e1x2.toml --list-ports
```

`--list-ports` validates and compiles the profile without opening hardware.

## Hardware Regression

Release `hw:OTG,0` from PipeWire first. Then run the direct engine test:

```text
cargo run --release -p sidealsa-core --bin sidealsa-hw-test -- --profile profiles/topping-e1x2.toml --periods 15000
```

Expected hardware result remains zero playback and capture XRUNs.

The reference profile enables `SCHED_FIFO` priority `48` for the engine worker
tree and priority `46` for the ASIO callback. This keeps both below the
reference PREEMPT_RT xHCI IRQ thread at `50`; external `chrt` is not required
for the engine worker and buffer size remains unchanged.

## Validation

Profiles reject empty or malformed IDs, empty or control-character names,
duplicate IDs, duplicate physical mappings, empty port mappings, out-of-range
channels, and unknown TOML fields.

## Limitations

- No daemon, IPC, shared memory, client library, or ALSA ioplug.
- No PRO or SHARED stream ownership.
- Mapping helpers operate on caller-provided preallocated buffers only.
- Capture is still discarded by the direct engine.
- Playback is still zero-filled by the direct engine.
