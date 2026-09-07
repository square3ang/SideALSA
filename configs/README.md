# Integration Configuration

Device-specific ALSA PCMs and PipeWire adapters are generated from the selected
profile by `sidealsa-config-gen`; there is no fixed device port inventory here.

```sh
cargo run -p sidealsa-config --bin sidealsa-config-gen -- \
  --profile profiles/example-6x6.toml --socket /tmp/sidealsad.sock \
  --output-dir target/generated-example
```

The remaining PipeWire Pulse and WirePlumber fragments contain device-neutral
desktop scheduling policy. They do not match or disable physical sound cards.
Legacy E1x2 adapter files are retained only as regression fixtures under
`crates/sidealsa-config/tests/fixtures`.

See `docs/device-profiles.md` for policy overrides, channel positions, installer
selection/preservation behavior and supported hardware limits.
