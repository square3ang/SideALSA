# Milestone 6: sidealsa-client

## Scope

`sidealsa-client` owns reusable client-side control and shared-memory transport
logic. `sidealsa-daemon` uses its common `SharedRegion` implementation. Client
connections perform the protocol handshake, receive shared descriptors through
`SCM_RIGHTS`, and own one daemon session per connection.

Core API:

```text
SideAlsaClient::connect(path)
client.get_info()
client.open_pro()
client.open_shared("line1")
stream.start()
stream.wait_period(timeout)
stream.capture_buffer(&mut samples)
stream.playback_buffer(&samples)
stream.stop()
stream.close()
```

Buffers use safe copy-based APIs. Ring slots, eventfds, sequence numbers, and
all period indices are preallocated or fixed before period processing. Zero-copy
views remain deferred until a slot-guard API exists.

## Diagnostics

`sidealsa-shared-test` now lives in `sidealsa-cli` and uses only
`sidealsa-client` for control, descriptor reception, event polling, and audio
slot operations.

## Verification

```text
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Observed on Topping E1x2 with profile realtime scheduling:

- Simultaneous PRO and SHARED playback: 1500 periods each, 93 shared underruns under 2 ms delays, one startup PRO miss, zero hardware XRUNs.
- Client capture: 1500 mapped blocks, zero shared overruns, zero hardware XRUNs.
- Runs: zero generation changes and zero timeline resets.

## Limitations

- No ALSA ioplug or PipeWire integration.
- No ASIO frontend.
