# SideALSA

SideALSA is an experimental userspace professional-audio layer for Linux. A
single daemon owns the physical ALSA interface and exposes two isolated client
domains:

- **PRO** is exclusive, low-latency, and exposes the complete physical channel
  set to native clients, the ALSA plugin, and the Wine ASIO frontend.
- **SHARED** provides independently buffered logical ports for PipeWire and
  desktop audio. Different ports can run together, with one owner per port.

The central invariant is:

```text
client deadline miss != hardware XRUN
```

The hardware timeline continues when a client is late or disappears. SideALSA
is not a general audio graph, a resampler, or a JACK replacement.

## Project Status

SideALSA currently implements the complete prototype path from a physical ALSA
device to native, ALSA, PipeWire, Qt, and Wine clients.

| Component | Status |
| --- | --- |
| Direct duplex ALSA engine and XRUN recovery | Implemented |
| `sidealsad` hardware owner and diagnostics | Implemented |
| Exclusive PRO shared-memory client | Implemented |
| Buffered SHARED logical ports | Implemented |
| ALSA external ioplug | Implemented, S32_LE/RW-interleaved only |
| PipeWire integration through the ALSA ioplug | Implemented with static reference-port configuration |
| Qt 6 control panel and privileged profile helper | Implemented |
| x86_64 Wine/Proton ASIO frontend | Experimental |

The Topping E1x2 OTG is the first and currently the only fully exercised
reference device. The core is profile-driven, but installation-time ALSA and
PipeWire adapter generation for arbitrary profiles is not implemented yet.

This is pre-release software. Do not treat successful startup or zero XRUN
counters as proof of fixed end-to-end analog latency; see
[Known Limitations](#known-limitations).

## Architecture

```text
                              Physical ALSA device
                                      hw:X,Y
                                         |
                                  +------v------+
                                  |  sidealsad  |
                                  | HW RT loop  |
                                  | HW timeline |
                                  +-------------+
                                         |
                         +---------------+---------------+
                         |                               |
                  exclusive PRO                  buffered SHARED
                         |                               |
           +-------------+-------------+                 |
           |             |             |                 |
      native client  Wine ASIO   ALSA ioplug       ALSA ioplug
                                     |                   |
                                ALSA PRO app          PipeWire
                                                  desktop applications
```

Control and handshakes use a Unix socket. Audio uses fixed shared-memory rings
with sequence numbers and eventfd notifications; audio is not sent through the
socket. The real-time hardware worker uses preallocated storage and never waits
indefinitely for a client.

A missing PRO block repeats the last valid PRO period without changing its
sequence; silence is used before the first valid block and after lifecycle or
hardware-generation changes. The current SHARED contribution is mixed after
that PRO decision, so stale SHARED audio is never part of the PRO repeat cache.
A missing SHARED contribution becomes silence or its own last valid logical
period, depending on the profile. Neither case requests an ALSA restart. Actual
ALSA XRUN recovery is counted separately and advances the hardware timeline
generation when the stream must be rebased.

## Reference Profile

[`profiles/topping-e1x2.toml`](profiles/topping-e1x2.toml) currently configures:

| Setting | Value |
| --- | --- |
| Device | Topping E1x2 OTG, `hw:OTG,0` |
| Sample format and rate | S32_LE, 48 kHz |
| Physical channels | 8 playback, 10 capture |
| Native protocol and PRO ALSA period | 64 frames |
| Physical ALSA period | 64 frames |
| Physical ALSA buffer | 256 frames |
| Direct PRO startup queue | 128 frames |
| Independent SHARED ring | 512 frames |
| SHARED ALSA/PipeWire period | 256 frames |
| SHARED ALSA buffer | 768 playback, 512 capture frames |
| PRO software output latency reported to clients | 64 frames |
| SHARED playback lookahead | 7 internal Q64 periods, 448 frames |

The reported PRO latency does not include USB transport, device firmware,
converters, or analog loopback delay.

The reference PRO loop primes the linked ALSA ring, then lets capture
and playback poll readiness jointly drive each Q64 transfer. The B256 ring is
capacity; Q128 is queued. Client playback completion wakes the daemon
through eventfd. There is no playback queue target, phase calibration, or live
delay calculation in this mode. Exact PRO playback has a bounded 1.0 ms handoff
deadline that is dynamically shortened from the post-poll ALSA availability so
fallback selection and the ALSA write retain a Q16 queue reserve.

The PRO latency acceptance target is zero frames added relative to an otherwise
identical direct ALSA capture-to-playback loop. This is a differential target:
USB transport, converter delay, the stable ALSA startup queue, and the device's
duplex phase remain in both measurements. `pro_latency_periods = 0` means the
client block for capture sequence N is committed to playback sequence N; no
extra process-ahead period is inserted by SideALSA.

The reference logical ports are:

| Direction | Port IDs | Physical channels |
| --- | --- | --- |
| Playback | `line1`, `line2`, `line3`, `line4` | 0/1, 2/3, 4/5, 6/7 |
| Capture | `mic1`, `mic2` | 0, 1 |
| Capture | `input34`, `input56`, `input78`, `input910` | 2/3, 4/5, 6/7, 8/9 |

Timing and realtime-priority values in this profile were selected for the
reference host. Review them for a different device or system rather than
assuming the same USB IRQ and scheduler topology.

## Requirements

The Rust workspace uses edition 2024. A normal full installation needs:

- A recent stable Rust toolchain and Cargo.
- C and C++17 toolchains, `pkg-config`, and ALSA development headers.
- CMake and Qt 6.5 or newer with Widgets development files for the control
  panel.
- polkit and `pkexec` to apply settings from the control panel.
- systemd for the supplied service installation.
- PipeWire and WirePlumber for desktop integration.
- Membership of the desktop user in the `audio` group when that group exists.
- ALSA utilities such as `aplay`, `arecord`, and `speaker-test` for the examples
  below.
- CMake, `winegcc`, and `winebuild` commands whenever the main installer is
  invoked with `--with-asio`, including `--no-build` due to its current checks.
- 64-bit Wine development headers when actually building ASIO, plus a host
  `wine` executable when registering it in prefixes.

Build-system errors report missing general dependencies. The installer performs
additional explicit checks for selected GUI and ASIO operations.

## Install

Run the installer as the normal desktop user, not with `sudo`:

```sh
./scripts/install.sh
```

It builds the Rust workspace and Qt control panel as that user, requests `sudo`
only for protected paths and systemd operations, installs the reference profile,
starts `sidealsad`, and installs the ALSA and PipeWire adapter configuration.

Important defaults are:

| Item | Path |
| --- | --- |
| Binaries | `/usr/local/bin` |
| Device profile | `/etc/sidealsa/profiles/topping-e1x2.toml` |
| Control socket | `/tmp/sidealsad.sock` |
| ALSA definitions | `/etc/alsa/conf.d/99-sidealsa.conf` |
| PipeWire definitions | `/etc/pipewire/pipewire.conf.d/99-sidealsa.conf` |
| systemd service | `sidealsad.service` |

When the `audio` group exists, the service creates the socket as `root:audio`
with mode `0770`; the desktop user, PipeWire, and Wine clients therefore need
that group in their active supplementary groups. Log out and back in after
adding membership. If no `audio` group exists, the installer warns and uses a
world-accessible socket instead.

The supplied PipeWire fragments do more than create SideALSA nodes. They set
PipeWire and PipeWire Pulse to nice level `-11`, realtime priority `10`, disable
the RT portal, and set WirePlumber's loop priority to `10`. These are process-wide
scheduling settings; review the files under `configs/` before installing them on
a differently configured host.

Common variants:

```sh
# Rust daemon, tools, and ALSA/PipeWire integration without the Qt GUI
./scripts/install.sh --no-gui

# Also build and install the experimental Wine ASIO binaries
./scripts/install.sh --with-asio

# Install and enable without starting or restarting the daemon
./scripts/install.sh --no-start

# Skip or remove installer-managed PipeWire integration
./scripts/install.sh --no-pipewire

# Deliberately replace the existing installed profile
./scripts/install.sh --replace-profile
```

An existing installed profile is preserved by default, including during an
upgrade or `--force` install. Review repository profile changes and use
`--replace-profile` only when replacement is intended.

Each installer invocation describes the desired complete optional feature set.
On a later upgrade, omitting `--with-asio` removes installer-managed system ASIO
files, `--no-gui` removes the managed GUI/helper files, and `--no-pipewire`
removes the managed PipeWire fragments. Repeat the desired feature flags.
`--no-start` leaves an already running daemon and user PipeWire session running;
it is not a safe substitute for cycling components during a protocol upgrade.
`--no-pipewire` also leaves active user services running after removing their
fragments, so restart PipeWire and WirePlumber manually to unload stale nodes.

### Prebuilt Installation

`--no-build` requires every selected artifact to exist already. Cargo does not
build the Qt control panel. For a Rust-only installation, use:

```sh
cargo build --release --workspace
./scripts/install.sh --no-build --no-gui
```

A full `--no-build` installation additionally requires
`build-gui/sidealsa-control`. `--with-asio --no-build` also requires the
artifacts under `build-asio/`, but the current installer still checks for CMake,
`winegcc`, and `winebuild` when that flag combination is used.

### Upgrade Compatibility

The daemon, client library, ALSA plugin, CLI tools, and ASIO frontend must come
from a compatible build. The current control protocol is version **16** and the
shared-memory layout is version **9**; both are checked exactly rather than
negotiated across incompatible versions.

The normal installer replaces its managed files, then stops active user audio
immediately before restarting the daemon. It waits for the new daemon socket
before restoring user audio. Installation is not transactional: a file-copy or
daemon-readiness failure does not restore previously installed binaries or a
profile explicitly replaced with `--replace-profile`. `--preserve-pipewire`
leaves the current PipeWire processes and installed adapter fragments untouched,
although it still replaces the ALSA plugin binary. Use that option only when
this is deliberate.

In particular, a running PipeWire process may still have an old
`libasound_module_pcm_sidealsa.so` mapped after the file is replaced. If the
plugin or protocol changed, restart the user audio services before testing:

```sh
systemctl --user restart \
  pipewire.service pipewire-pulse.service wireplumber.service
```

A stale plugin commonly surfaces as `Protocol error` on every SideALSA PCM even
though `sidealsad` itself is healthy. Direct native clients must reconnect after
a daemon restart as well.

### Uninstall

```sh
./scripts/uninstall.sh
```

The uninstaller removes files recorded in the install manifest and preserves
files modified after installation. It also preserves the device profile. Use
`--force` only when changed managed files should be removed. It prints, but does
not execute, the user PipeWire restart command. User-local Wine files, prefix
DLL copies, and registry entries created by `install-asio.sh` are outside the
main install manifest and remain installed.

See [Installation](docs/installation.md) for additional package-staging,
profile-replacement, timing, and control-panel lifecycle details. The canonical
option list is always available from `./scripts/install.sh --help`.

## Verify an Installation

Check the service, protocol, advertised PRO PCM, and PipeWire nodes:

```sh
systemctl status --no-pager sidealsad.service
sidealsa-stats --samples 1
aplay -L | grep sidealsa_pro
wpctl status
```

The installed reference ALSA PCMs are:

| PCM | Direction and scope |
| --- | --- |
| `sidealsa_pro` | Exclusive raw 8-output/10-input PRO interface |
| `sidealsa_line1` through `sidealsa_line4` | SHARED stereo playback |
| `sidealsa_mic1`, `sidealsa_mic2` | SHARED mono capture |
| `sidealsa_input34` through `sidealsa_input910` | SHARED stereo capture |

Only `sidealsa_pro` carries an ALSA discovery hint. The SHARED PCMs are valid
named definitions but do not appear in `aplay -L` or `arecord -L`; verify them by
opening the names directly.

Basic SHARED tests:

```sh
speaker-test -D sidealsa_line1 -c 2 -r 48000 -F S32_LE -t sine
arecord -D sidealsa_mic1 -f S32_LE -c 1 -r 48000 -d 5 /tmp/sidealsa-mic1.wav
```

The native smoke clients default to silence and do not require ALSA plugin
discovery:

```sh
sidealsa-pro-client-test --periods 3000
sidealsa-shared-test --port line1 --periods 3000
```

Only one PRO owner may exist. Close a native PRO client, `sidealsa_pro` user, or
ASIO application before opening another. The current ALSA plugin opens each PCM
direction as a separate SideALSA stream, so two-handle full-duplex PRO through
`sidealsa_pro` is not supported; use the native client API or ASIO for a single
duplex PRO session.

Each SHARED logical port also has one backend owner at a time. Different ports
can operate concurrently, and PipeWire can mix multiple desktop applications
above the one PipeWire owner for a port. A second direct opener of the same port
receives `BUSY`.

PipeWire creates these reference nodes:

```text
sidealsa-line1 .. sidealsa-line4
sidealsa-mic1
sidealsa-mic2
sidealsa-input34 .. sidealsa-input910
```

PipeWire graph status alone is not sufficient to diagnose the complete path.
Inspect SideALSA's SHARED counters as well because silence fallback and capture
loss can occur without a PipeWire graph XRUN.

## Diagnostics

Read live daemon counters without entering the real-time thread:

```sh
sidealsa-stats --samples 100 --interval-ms 100
journalctl -u sidealsad.service -f
```

The main counter groups are:

| Counter | Meaning |
| --- | --- |
| `hw_playback_xruns`, `hw_capture_xruns` | Actual ALSA hardware XRUNs |
| `pro_deadline_misses` | PRO fallback periods |
| `pro_client_deadline_misses` | Playback not supplied by the client in time |
| `pro_core_deadline_misses` | Core could not complete the period in time |
| `shared_underruns`, `shared_overruns` | Isolated SHARED data loss |
| `timeline_resets`, `generation` | Hardware stream restart or rebase |
| `periods_processed`, `sample_position` | Published hardware timeline progress |

`sidealsa-stats` also prints delays, client-wait timing, per-port SHARED playback
diagnostics, global SHARED capture overruns, and duplex pointer-phase
observations. `duplex_pointer_phase_nanos` compares separately timestamped ALSA
pointer observations. It is useful for debugging but is not a USB-link,
converter, or analog phase measurement.

## Control Panel

The default installation provides:

```sh
sidealsa-control
```

The Qt 6 application edits timing and scheduling fields through a small polkit
helper. Apply validates and atomically replaces the root-owned profile, restarts
the fixed system service, verifies the daemon and loaded profile fingerprint,
and rolls back on failure. It then restarts active user PipeWire services so the
static adapters reopen against the new daemon. Native and ASIO clients still
need to reconnect.

## Wine ASIO

The experimental x86_64 ASIO frontend connects directly to the exclusive PRO
path; it does not pass through ALSA or PipeWire. Install its system artifacts
with:

```sh
./scripts/install.sh --with-asio
```

The prefix helper performs a second, user-local deployment under
`$HOME/.local/lib/wine`, copies the PE DLL into each selected prefix, registers
it, and prints the required `WINEDLLPATH` launch setting. For one prefix:

```sh
./scripts/install-asio.sh --no-build --wine-prefix "$WINEPREFIX"
```

Other registration selections include:

```sh
./scripts/install-asio.sh --no-build --all-steam
./scripts/install-asio.sh --no-build --all-steam --appid APPID
./scripts/install-asio.sh --no-build --steam-prefix /path/to/pfx
```

If `sidealsad` was installed with a non-default `--socket`, launch Wine with the
same path in `SIDEALSA_SOCKET`; the ASIO frontend otherwise uses
`/tmp/sidealsad.sock` and cannot discover the installer's custom setting.

The runtime reads rate, period, channels, and latency from the daemon. Current
acceptance and probe coverage is limited to the reference 48 kHz/Q64 geometry.
Detailed build, lifecycle, probe, and analog loopback procedures are in
[ASIO Frontend](docs/milestone-asio.md); use
`./scripts/install-asio.sh --help` for deployment options.

## Configuration

Profiles are parsed, validated, and compiled before hardware streaming starts.
Validation covers channel ranges, IDs, mappings, timing geometry, formats, and
realtime settings. The real-time worker does not parse TOML or inspect port
strings.

Physical channels and logical ports remain separate. Every logical port is a
channel view into one physical playback or capture stream; SideALSA does not
create a hardware stream or real-time audio thread per port. The control plane
does use a thread for each connected client.

The core accepts profile-defined devices, but the installed ALSA and PipeWire
fragments currently assume the E1x2 reference IDs, directions, and channel
widths. `scripts/install.sh` performs only a literal text check for assignments
such as `id = "line1"`; it does not semantically verify that a custom profile is
compatible with those fragments. Equivalent TOML formatting may also fail that
text check. Automatic fragment generation and semantic adapter compatibility
validation are future work.

## Build and Test

Build the Rust workspace:

```sh
cargo build --release --workspace
```

Run the non-hardware verification suite:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Build the GUI separately because it is not a Cargo workspace member:

```sh
cmake -S crates/sidealsa-gui -B build-gui -DCMAKE_BUILD_TYPE=Release
cmake --build build-gui
```

Hardware, loopback, PipeWire, and ASIO acceptance tests require the reference
device and, for latency tests, a physical cable from playback channel 0 to the
selected capture channel. Do not run direct hardware tools while `sidealsad`
owns the device. See the milestone documents for each test setup and its current
acceptance criteria.

The system `alsa_delay` utility provides an independent canonical Q64/B128
reference. On the E1x2 it measures 375 frames, or 7.813 ms:

```sh
alsa_delay hw:OTG,0 hw:OTG,0 48000 64 2 5 1
```

Measure the same Q128 queue while retaining SideALSA's B256 capacity:

```sh
cargo build --release -p sidealsa-core --bin sidealsa-direct-loopback-test
chrt --fifo 48 target/release/sidealsa-direct-loopback-test \
  --profile profiles/topping-e1x2.toml \
  --periods 10000 \
  --buffer-frames 256 \
  --start-frames 128
```

`direct_min_frames` and `direct_max_frames` use the same app-visible coordinate
as `sidealsa-loopback-test`. `direct_physical_*` removes Q128 and reports the
device/USB/analog component. Separate hardware opens can select different
duplex phases, so parity means matching the direct distribution without a
SideALSA-only Q64 displacement, not equality between arbitrary paired opens.
The current Q128 verification measured 361 frames in a direct open and 373
frames in the final installed SideALSA smoke test; every pulse within each open
was exact, and there was no SideALSA-only Q64 displacement. A continuous
SideALSA session held 403 frames for 100000 RT PRO periods under delayed SHARED
load.

## Workspace

| Path | Responsibility |
| --- | --- |
| `crates/sidealsa-core` | ALSA hardware engine, routing, timeline, recovery |
| `crates/sidealsa-config` | TOML profile parsing, validation, fingerprinting |
| `crates/sidealsa-protocol` | Control and shared-memory protocol definitions |
| `crates/sidealsa-client` | Reusable native client library |
| `crates/sidealsa-daemon` | `sidealsad` process and client ownership |
| `crates/sidealsa-alsa` | ALSA external ioplug |
| `crates/sidealsa-asio` | Rust runtime and Wine ASIO build |
| `crates/sidealsa-cli` | Diagnostics and test clients |
| `crates/sidealsa-admin` | Restricted profile-apply helper |
| `crates/sidealsa-gui` | Qt 6 control panel, built with CMake |

## Known Limitations

- The E1x2 OTG is the only fully exercised device profile.
- The installed ALSA and PipeWire objects are static for the reference port IDs.
- Every SHARED port has one backend owner; multi-application desktop mixing
  occurs in PipeWire rather than inside SideALSA.
- The ALSA ioplug supports S32_LE, RW-interleaved access, and the profile sample
  rate. It does not provide mmap, resampling, or format conversion.
- ALSA `sidealsa_pro` does not currently provide a conventional two-handle
  full-duplex open because the second handle encounters exclusive PRO ownership.
- PipeWire integration uses static ALSA adapters. There is no custom PipeWire
  client, automatic node generation, or automatic reconnection after a daemon
  restart.
- The ASIO frontend is x86_64-only and experimental; non-reference stream
  geometry has not received acceptance coverage.
- Device hotplug, resampling, dynamic DSP, an arbitrary routing graph, and a GUI
  mixer are out of scope for the current implementation.
- Fixed analog loopback phase is not guaranteed across runtime load transitions
  or PRO release/reacquisition. Strict reference-device tests have observed a
  loopback move from 372 to 409 frames without a hardware XRUN, timeline reset,
  generation change, or core deadline miss. ALSA pointer diagnostics did not
  reliably predict that movement. The hardware timeline remained continuous,
  but the stricter analog-phase invariant is unresolved.
- The Q128 direct PRO path has native, delayed-PRO, simultaneous-SHARED, Wine
  ASIO, and release-build stress coverage. The complete Discord playback,
  microphone, and screen-sharing soak has not been repeated after this revision.

Current ASIO timing evidence and rejected phase-control experiments are recorded
in [ASIO Frontend](docs/milestone-asio.md). Earlier subsystem acceptance results
remain in the milestone documents and should be read with their stated profile
and test conditions.

## Documentation

- [Installation and lifecycle](docs/installation.md)
- [Direct ALSA engine](docs/milestone-1.md)
- [Profiles and channel splitting](docs/milestone-2.md)
- [Local PRO path](docs/milestone-3.md)
- [Daemon and protocol](docs/milestone-4.md)
- [SHARED path](docs/milestone-5.md)
- [Client library](docs/milestone-6.md)
- [ALSA ioplug](docs/milestone-7.md)
- [PipeWire integration](docs/milestone-8.md)
- [Wine ASIO frontend](docs/milestone-asio.md)

## License

SideALSA is licensed under [GPL-3.0-or-later](LICENSE).
