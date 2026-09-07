# Device Profiles and Generated Integration

The hardware engine, routing and client protocol use the selected profile.
Installation now derives ALSA PCMs and PipeWire adapters from that same profile,
rather than requiring the E1x2's port names or channel inventory. No RT loop
parses this integration metadata.

## Profile Topology

Keep physical streams in `[device.playback]` / `[device.capture]` and declare
logical views in `[[ports.playback]]` / `[[ports.capture]]`. Each port provides:

- `id`: globally unique ASCII letters/digits, `_` or `-`.
- `name`: display name; quotes/backslashes are escaped by the renderer.
- `channels`: ordered, zero-based physical channel indices.
- `positions`: optional ordered PipeWire channel positions, one per channel.

The generator produces:

| Profile element | ALSA PCM | PipeWire node |
| --- | --- | --- |
| Exclusive full interface | `sidealsa_pro` | Not exposed to desktop PipeWire |
| Playback port `monitor` | `sidealsa_monitor` | `sidealsa-monitor`, Audio/Sink |
| Capture port `digital` | `sidealsa_digital` | `sidealsa-digital`, Audio/Source |

Existing reference names remain unchanged. Port ID `pro` is rejected during
integration generation because it would collide with the exclusive PCM.
The core can still use such an ID without this ALSA/PipeWire integration.

Mono defaults to `MONO`, stereo to `FL FR`, and larger ports to discrete
`AUX0 ... AUXn`. Physical indices do not imply speaker positions: an eight-input
recording port is not silently treated as surround sound. Set explicit positions
when speaker semantics matter. Positions must be canonical, unique and match the
channel count; `MONO` is only valid alone. PipeWire ports are limited to 64 logical
channels; a larger physical interface may be split into smaller ports.

`profiles/topping-e2x2.toml` shares the E1x2 OTG port mapping and timing; select it when using the E2x2 OTG hardware.
`profiles/example-6x6.toml` is an **unmeasured example**, with a different port
inventory, mono/stereo/discrete views, 44.1 kHz and P64. Replace
`hw:YOUR_INTERFACE,0` with the real ALSA identifiers and validate capabilities
before attempting hardware streaming. It is not a working preset for a named
device.

## Desktop Policy

Topology and buffering policy are separate. Optional frame-based settings are:

```toml
[integration.pipewire]
playback_headroom_frames = 128
capture_headroom_frames = 0
playback_start_delay_frames = 256
```

Those defaults preserve existing installations whose profiles lack this table;
they are not claimed to be optimal for every device. Capture headroom is not
increased automatically. The settings are explicit frame counts, not multipliers
of a hardware period or promises of end-to-end latency.

The renderer deliberately does not emit a fixed `audio.rate` or hardware period.
The ALSA plugin negotiates those from the daemon. Consequently, GUI/admin edits
to hardware rate, period or buffer settings do not silently stale generated
topology or frame-policy values. Changing ports, positions or integration policy
requires regeneration and client reconnection. Policy values remain explicit
when timing changes; review them when selecting a substantially different geometry.

The existing global desktop scheduling policy remains separate: PipeWire/Pulse
RT priority 10 and WirePlumber loop priority 10. This change does not discover IRQ
priorities or automatically tune the scheduler. Choose device/PRO priorities
consistently with the desktop and USB completion threads.

## Offline Generation

```sh
cargo run -p sidealsa-config --bin sidealsa-config-gen -- \
  --profile profiles/example-6x6.toml \
  --socket /tmp/sidealsad.sock \
  --output-dir target/generated-example
```

Outputs are `asound.sidealsa.conf` and `pipewire.conf`. Use `--alsa-only` when
PipeWire output is not needed. The command does not connect to the daemon or
open ALSA. All input validation/rendering completes before output creation;
writing several output files is not a filesystem-wide transaction if an I/O
failure occurs, so installers generate into temporary staging directories.

The former static E1x2 adapter files now live under
`crates/sidealsa-config/tests/fixtures` solely as compatibility regression data.
The remaining `configs/` fragments contain device-neutral scheduling policy.

## Installation and Selection

Pass an actual validated device profile to the installer:

```sh
scripts/install.sh --profile /path/to/your-interface.toml
```

The profile is installed under `/etc/sidealsa/profiles/<basename>.toml`.
`/etc/sidealsa/active.toml` records its path and socket. Service and desktop
launchers receive explicit paths; bare daemon/admin/GUI commands resolve the
trusted selection. Explicit CLI profile/socket arguments take precedence.
Update the GUI and admin helper together; the GUI requires the helper's resolved
profile/socket fields and reports an error with an older helper rather than
silently assuming the reference device.

The selected profile must be a regular root-owned `.toml` file directly in the
managed profile directory, with no group/world write permission or symlinks.
The active selection and relevant parent directories are also checked. These
checks do not pin the profile inode against an uncoordinated privileged writer;
they are not a complete cross-process filesystem transaction.

When no selection exists, runtime commands retain compatibility with the old
installed E1x2 profile only if that concrete file passes validation. Otherwise
they require `--profile`, rather than guessing hardware. The installer can
migrate the old generated service's known selection syntax; unknown service
commands require explicit selection. Fresh installs still use the E1x2 seed
when no profile is supplied.

Reinstallation without explicit selection reuses the installed selection.
Existing destination profiles are preserved unless replacement is explicitly
requested, including under `--force`. Configuration is generated from the profile
that will actually be used, not from an ignored newer seed. To replace it:

```sh
scripts/install.sh --profile profiles/topping-e1x2.toml --replace-profile
```

Bare `--replace-profile` is rejected. The reference seed now uses **physical P64
with logical Q64**. Existing P32 installations are not changed by a normal
reinstall. Explicit P32 remains supported; omitting `hardware_period_size` still
means inherit `period_size`, not an engine-wide hardcoded 64.

Installed service/desktop paths currently exclude whitespace and reserved command
characters. Profile display names are not filenames and may contain spaces.

## Preserving PipeWire Configuration

`--preserve-pipewire` retains existing files and their previous ownership hashes,
including local edits. It requires a verified generated-profile snapshot at
`/etc/sidealsa/integration-profile.toml`. Compatibility is checked against that
snapshot, not against a potentially edited current profile. Port IDs, directions,
channel counts and effective positions must match; reordering ports or changing
display names does not change topology.

The snapshot is updated only when adapters are regenerated. A tampered snapshot
or a topology change is rejected before installed files are modified, even under
`--force`. `--no-pipewire` also retires this snapshot.

Legacy installations without a snapshot must first regenerate without
`--preserve-pipewire`. Back up and review local customizations before doing so;
put unrelated user overrides in separate drop-ins when practical. Preserving
files intentionally retains their old presentation/policy values even if new
profile metadata requests different ones.

## Limits and Verification

- The engine still requires supported duplex ALSA streams and `S32_LE`. Other
  formats, playback-only hardware, device hotplug and automatic capability
  discovery are not added by configuration generation.
- No physical-card WirePlumber exclusion is inferred from an ALSA PCM string or
  device display name. Ensure PipeWire is not using the physical PCM and select
  the generated logical endpoints. No unrelated sound cards are disabled.
- Existing handles still need reconnection after a daemon/profile switch.
- Reference-specific ASIO/loopback benchmarks remain reference-specific. The
  shared test client now selects a configured port by default and uses the
  negotiated rate for tone synthesis.
- Tests exercise generation, escaping, metadata preservation, trusted selection,
  different topologies and staged installation. They do not establish real-device
  stability for the example interface or remeasure the new P64 reference default.

Run the hardware-free checks with:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p sidealsa-config
bash scripts/test-install-profile-generation.sh
```

Installer tests use the real renderer but mock runtime/plugin artifacts in a
DESTDIR tree. They never install into the live system or manage services.

Optional parser-level checks, with ALSA development headers and PipeWire tools:

```sh
cc -Wall -Wextra -Werror crates/sidealsa-config/tests/parse_alsa_config.c \
  -lasound -o target/parse-alsa-config
target/parse-alsa-config target/generated-example/asound.sidealsa.conf /tmp/sidealsad.sock
spa-json-dump target/generated-example/pipewire.conf
```

These parse configuration only and do not open audio devices.
