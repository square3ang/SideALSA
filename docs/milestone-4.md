# Milestone 4: sidealsad

## Scope

`sidealsa-daemon` owns one physical ALSA duplex device. Control uses a Unix
domain socket. PRO audio uses a memfd-backed shared-memory region. Audio never
travels through the control socket.

The daemon starts the existing `DuplexEngine::run_pro` loop. The playback-clock
cycle publishes capture into the shared region and consumes playback by exact
sequence number. Missing playback becomes silence and increments
`pro_deadline_misses`; ALSA is not restarted for that condition.

## Control

Every client sends `HELLO` first. Supported operations:

- `GET_INFO`
- `OPEN_PRO`
- `START`
- `STOP`
- `CLOSE`
- `GET_STATS`

`OPEN_PRO` is exclusive. The response passes the memfd and two eventfds with
`SCM_RIGHTS`. Disconnect releases the PRO owner. `OPEN_SHARED` was deferred to
Milestone 5.

## Build

```text
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## Run

```text
cargo run -p sidealsa-daemon --bin sidealsad -- --profile profiles/topping-e1x2.toml --socket /tmp/sidealsad.sock
```

Reference profile enables realtime scheduling internally. Use `chrt` only when
overriding profile settings:

```text
target/debug/sidealsad --profile profiles/topping-e1x2.toml --socket /tmp/sidealsad.sock
```

## Limitations

- No `sidealsa-client` crate yet.
- No SHARED logical-port implementation yet.
- No daemon-side shared-memory client utility yet.
- Hardware acceptance still requires the Topping device.
- Shared ring cleanup after a crashed client is conservative; playback stale
  slots are discarded by sequence matching.
