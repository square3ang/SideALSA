# SideALSA

> One hardware clock. Two isolated client domains. No client owns the timeline.

SideALSA is a userspace professional-audio layer built on ALSA. It owns one
physical USB audio device and exposes two independent client paths:

- **PRO**: exclusive, low-latency, complete physical channel layout.
- **SHARED**: buffered logical ports for PipeWire and desktop applications.

The hardware timeline keeps running when a client misses a deadline. A client
miss becomes silence and a diagnostic counter, not an ALSA restart.

Initial reference device: **Topping E1x2 OTG**.

| Path | Purpose | Failure behavior |
| --- | --- | --- |
| **PRO** | Exclusive DAW, native ALSA, and Wine/Proton ASIO | A missed exact-sequence block becomes silence |
| **SHARED** | PipeWire and desktop logical ports | Each late client is isolated and buffered |
| **Hardware** | One continuous duplex ALSA timeline | Only a real ALSA failure triggers XRUN recovery |

Current profile: `48 kHz`, `S32_LE`, `64`-frame client periods, `32`-frame
physical periods, `256`-frame hardware buffer, 8 playback channels, and 10
capture channels. The linked zero-lead PRO path reports 64 input frames and 64
output frames. Its `500 us` bounded handoff accommodates Wine callback overhead
without adding a whole period of PRO latency.

## Status

Working paths:

- Direct duplex ALSA ownership through `sidealsad`.
- PRO client protocol and exclusive ownership.
- Shared logical playback/capture ports.
- ALSA ioplug for raw `sidealsa_pro` and hidden PipeWire backing PCMs.
- PipeWire adapters backed by the ALSA ioplug.
- Experimental 64-bit Wine/Proton ASIO frontend.
- Qt 6 timing control panel with authenticated apply and automatic rollback.

Not implemented:

- ALSA ioplug mmap access.
- Resampling or format conversion.
- Advanced routing or DSP.
- 32-bit ASIO/WoW64 frontend.

## Architecture

```text
Topping E1x2 hw:OTG,0
          |
      sidealsad
       /      \
     PRO     SHARED
      |        |
   ASIO/DAW  ALSA ioplug
                 |
              PipeWire
```

Only `sidealsad` opens the physical ALSA device. Audio data does not travel
through the control socket. Clients use shared memory for audio and eventfd for
cycle notifications.

## Requirements

Arch/CachyOS example:

```sh
sudo pacman -S --needed \
  alsa-lib alsa-utils pipewire pipewire-pulse wireplumber \
  rust cmake gcc pkgconf qt6-base polkit wine wine-tools
```

Build requirements:

- Rust toolchain with Cargo.
- ALSA development headers and `pkg-config`.
- CMake, GCC, and GNU make or Ninja.
- Qt 6 Widgets and polkit for the control panel.
- Wine SDK headers, `winegcc`, and `winebuild` for ASIO.
- A 64-bit Wine executable for prefix registration.

Package names differ by distribution. Install equivalent ALSA, PipeWire, Rust,
CMake, and Wine development packages there.

## Build Core

From repository root:

```sh
cargo build --release --workspace
```

Run static profile validation without taking hardware:

```sh
target/release/sidealsa-hw-test \
  --profile profiles/topping-e1x2.toml \
  --list-ports
```

## Install Core

Install the daemon, control panel, ALSA plugin, initial profile, PipeWire
adapters, and systemd service:

```sh
./scripts/install.sh --no-build
```

The installer creates the selected profile only when it does not exist. Later
installs preserve `/etc/sidealsa/profiles/*.toml`, including with `--force`.
After reviewing local changes, use `--replace-profile` to adopt a new reference
profile. The current Q64/Q32 reference uses
`linked_playback_guard_frames = 48` and `pro_latency_periods = 0`.

Use `--no-gui` when Qt or polkit integration is not wanted. The privileged
helper is always installed at `/usr/libexec/sidealsa-admin`, outside a custom
binary prefix, and may only update root-owned profiles directly under
`/etc/sidealsa/profiles`.

Installer defaults:

```text
daemon:       /usr/local/bin/sidealsad
profile:      /etc/sidealsa/profiles/topping-e1x2.toml
ALSA config:  /etc/alsa/conf.d/99-sidealsa.conf
PipeWire:     /etc/pipewire/pipewire.conf.d/99-sidealsa.conf
ALSA plugin:  /usr/lib/alsa-lib/libasound_module_pcm_sidealsa.so
socket:       /tmp/sidealsad.sock
control UI:   /usr/local/bin/sidealsa-control
admin helper: /usr/libexec/sidealsa-admin
```

Restart user audio after installation:

```sh
systemctl --user restart pipewire.service pipewire-pulse.service wireplumber.service
```

Verify daemon and hardware parameters:

```sh
systemctl status sidealsad.service
target/release/sidealsa-stats --samples 3
cat /proc/asound/card*/pcm0p/sub0/hw_params
```

Expected physical playback parameters:

```text
format: S32_LE
channels: 8
rate: 48000
period_size: 32
buffer_size: 256
```

The reference profile exposes a separate `shared_buffer_size = 512`. The ioplug
keeps daemon transfers and that ring at Q64/B512, while PipeWire playback
negotiates Q256/B768. The third external period provides startup capacity for a
Q256 PipeWire start delay; the steady buffer target remains Q256. Playback uses
seven internal periods (`448` frames) of SHARED lookahead. This does not change
the physical B256 queue or Q64 PRO block size.

## Control Panel

Launch **SideALSA Control** from the desktop application menu or run:

```sh
sidealsa-control
```

The Qt control panel edits the complete hardware timing set: sample rate,
logical and physical periods, hardware and SHARED buffers, playback queue,
duplex/link controls, PRO and SHARED lead, handoff budget, and realtime
priorities.

Apply is transactional:

```text
validate candidate -> atomically save -> restart sidealsad -> verify new PID
       failure      -> restore original -> restart -> verify rollback
```

Authentication is handled by polkit. The helper verifies that the socket peer is
root-owned and matches the `sidealsad.service` MainPID, then compares the full
loaded profile fingerprint. Concurrent edits are rejected by revision rather
than overwritten. Comments and unrelated profile sections are preserved.

Applying timing restarts the physical stream. After a successful restart or a
reported failure that may have restarted the daemon, including a verified
rollback, the control panel restarts active user PipeWire services so their
static SideALSA adapters are recreated; desktop audio pauses briefly. Direct PRO
and SHARED clients must reconnect. If `systemctl --user` cannot restart
PipeWire, the control panel reports the manual recovery command without treating
an otherwise successful daemon configuration as failed. Unsupported sample
rates or ALSA geometries are rolled back automatically. The E1x2 has only been
exercised at `48 kHz`; other rates remain hardware-dependent.

An unexpected `sidealsad` restart disconnects existing ioplug handles. Restart
the user PipeWire services with the command above to recreate static adapters;
automatic ioplug reconnection is not implemented yet.

The card number can differ. Find it with:

```sh
aplay -l
arecord -l
```

## Test ALSA

List logical PCMs:

```sh
aplay -L | grep sidealsa
```

Direct playback test:

```sh
speaker-test -D sidealsa_line1 -c 2 -r 48000 -F S32_LE -t sine
```

Direct capture test:

```sh
arecord -D sidealsa_mic1 -f S32_LE -c 1 -r 48000 -d 5 /tmp/sidealsa-capture.wav
```

Watch counters while testing:

```sh
target/release/sidealsa-stats --samples 100 --interval-ms 100
```

Expected hardware counters stay at zero. Shared startup is armed by its first
consumed block, so startup silence does not count as an underrun. The first
missing block after arming increments `shared_underruns` and disarms that
episode; the next valid block rearms it. A paused client therefore does not add
one underrun per hardware period.

### osu! Exclusive ALSA

`sidealsa_pro` is advertised directly to ALSA device enumerators as
`SideALSA PRO`. Select that device in osu! to open the raw 8-channel PRO PCM.
This claims the exclusive PRO slot; ASIO, DAWs, and other PRO clients will
receive `BUSY` until osu! releases it. No ALSA plug, route, dmix, or resampler
is inserted.

## Test PipeWire

Check adapters:

```sh
wpctl status
pw-top
```

Play raw S32_LE data into a logical port:

```sh
pw-cat --playback \
  --target sidealsa-line1 \
  --raw --format s32 --rate 48000 --channels 2 \
  /path/to/stereo-s32le.raw
```

Record a logical input:

```sh
pw-cat --record \
  --target sidealsa-mic1 \
  --raw --format s32 --rate 48000 --channels 1 \
  /tmp/sidealsa-mic1.raw
```

PipeWire `ERR=0` is not sufficient proof by itself. Also inspect SideALSA
`shared_underruns` and `shared_overruns`, because a client can receive silence
or lose capture blocks without a PipeWire graph xrun.

## Build ASIO

SideALSA ASIO uses PRO directly. It does not use the ALSA ioplug.

Build 64-bit Wine ASIO artifacts:

```sh
cmake -S crates/sidealsa-asio \
  -B build-asio \
  -DCMAKE_BUILD_TYPE=Release
cmake --build build-asio --target \
  sidealsa-asio sidealsa-asio-probe sidealsa-asio-loopback-test
```

Artifacts:

```text
build-asio/sidealsa-asio64.dll
build-asio/sidealsa-asio64.dll.so
build-asio/sidealsa-asio-probe.exe
build-asio/sidealsa-asio-loopback-test.exe
```

The runtime layout also provides Wine 10+ `sidealsa-asio.dll` aliases.

The ASIO frontend expects `64`-frame ASIO buffers, `48 kHz`, 10 inputs, and 8
outputs. It reports 64 input frames and 64 output frames in the zero-lead
reference profile. Only one PRO owner is allowed. Close any other PRO client
before starting a DAW or ASIO probe.

## Install ASIO Into Steam Prefixes

PipeASIO uses a user-local Wine library root, copies the PE DLL into each Wine
prefix, registers it with `regsvr32`, and sets `WINEDLLPATH` for Proton. SideALSA
uses the same deployment pattern. Reference:

<https://github.com/M0n7y5/pipeasio>

Install and register in every discovered Steam compatdata prefix:

```sh
./scripts/install-asio.sh --all-steam
```

Register one game by AppID:

```sh
./scripts/install-asio.sh --all-steam --appid APPID
```

Register an explicit prefix:

```sh
./scripts/install-asio.sh \
  --steam-prefix "$HOME/.local/share/Steam/steamapps/compatdata/APPID/pfx"
```

The helper installs files under:

```text
$HOME/.local/lib/wine/x86_64-windows/sidealsa-asio64.dll
$HOME/.local/lib/wine/x86_64-unix/sidealsa-asio64.dll.so
```

It copies the PE half into each prefix's `system32` directory and registers
`HKLM\Software\ASIO\SideALSA` plus the SideALSA CLSID. It does not modify
Steam files or install anything system-wide.

Reuse an existing ASIO build:

```sh
./scripts/install-asio.sh --no-build --all-steam
```

When the daemon protocol changes, rebuild the Rust workspace and ASIO artifacts
before using `--no-build`. Replace both system and user-local Wine copies, then
restart `wineserver` so it does not retain an old Unix DLL.

Use a different user-local install root:

```sh
./scripts/install-asio.sh \
  --install-root "$HOME/.local/share/sidealsa" \
  --all-steam
```

The helper needs a host `wine` executable for registration. Override it when
needed:

```sh
./scripts/install-asio.sh \
  --wine /opt/wine-staging/bin/wine \
  --steam-prefix "$HOME/.local/share/Steam/steamapps/compatdata/APPID/pfx"
```

## Steam Launch Options

For a default user-local install, set this per-game Steam launch option:

```text
SIDEALSA_SOCKET=/tmp/sidealsad.sock WINEDLLPATH=/home/USER/.local/lib/wine %command%
```

Use an absolute path. Replace `USER`. `~` and `$HOME` are not reliably expanded
inside Steam launch options.

Start the daemon before launching the game:

```sh
systemctl is-active sidealsad.service
test -S /tmp/sidealsad.sock
```

After registration, select **SideALSA** in the DAW's ASIO device list.

For native Wine, use the same helper with a normal Wine prefix:

```sh
./scripts/install-asio.sh --wine-prefix "$HOME/.wine"
```

For Proton prefixes, host `wine` registration is normally enough because the
registry is stored inside the selected prefix. If registration fails because
host Wine and Proton use incompatible prefix formats, run `regsvr32` through
the matching Proton/Wine runtime while preserving `WINEDLLPATH`.

## ASIO Probe

Run the built probe against the running daemon:

```sh
SIDEALSA_SOCKET=/tmp/sidealsad.sock \
WINEDLLPATH="$HOME/.local/lib/wine" \
WINEPREFIX="$HOME/.wine" \
WINELOADER=wine build-asio/sidealsa-asio-probe.exe
```

The probe checks COM activation, channel counts, `64`-frame buffer negotiation,
64-frame output latency, sample rate, buffer pointers, callback quiescence after
`Stop`, and reuse of the same Wine callback thread after the next `Start`.

With playback channel 0 physically looped to capture channel 4, run strict
latency and abrupt-reacquisition checks:

```sh
WINELOADER=wine WINEDLLPATH="$PWD/build-asio" \
  build-asio/sidealsa-asio-loopback-test.exe
scripts/test-asio-reacquire.sh
```

The first command rejects pulse loss or frame variation across two Start legs.
The harness records a baseline, kills a streaming process without `Stop`, then
requires stable reacquisition, parity with an immediately following native PRO
loopback, and unchanged hardware timeline counters. Absolute analog phase may
move between processes on explicit-feedback USB hardware. The harness derives
the serving daemon PID from `SO_PEERCRED` on the tested socket.
`SIDEALSA_DAEMON_PID` may be set as an additional assertion.

The harness analyzes `raw loopback = common SideALSA/hardware path + ASIO
frontend residual`. It prints raw ASIO and paired native values separately and
accepts only the `ASIO - native` residual. A baseline-to-reacquisition shift in
the common path is reported separately and is not misclassified as either an
ASIO regression or a hardware-only event. The native reference runs at the same
default realtime priority (`86`) as the ASIO worker.

Probe result `77` means daemon or Wine driver unavailable. Check:

```sh
systemctl status sidealsad.service
test -S /tmp/sidealsad.sock
ls -l "$HOME/.local/lib/wine/x86_64-windows/sidealsa-asio64.dll"
ls -l "$HOME/.local/lib/wine/x86_64-unix/sidealsa-asio64.dll.so"
```

## Troubleshooting

### ASIO driver missing in DAW

Check prefix registration and launch environment:

```sh
WINEPREFIX=/path/to/pfx wine reg query 'HKLM\Software\ASIO\SideALSA'
```

Confirm Steam launch options contain absolute `WINEDLLPATH` and
`SIDEALSA_SOCKET`.

### `regsvr32` fails with `c0000135`

Wine cannot load the Unix half. Check that `WINEDLLPATH` points to the directory
containing `x86_64-unix/sidealsa-asio64.dll.so`, not to the `x86_64-unix`
directory itself.

### ASIO reports busy

PRO is exclusive. Stop another DAW, `sidealsa-pro-client-test`, or ASIO host
before starting the new host.

### No sound but ASIO callbacks run

Check daemon socket, hardware counters, and current PRO ownership:

```sh
target/release/sidealsa-stats --samples 10
ps -ef | grep sidealsad
```

ASIO output is raw physical multichannel output. SideALSA ASIO does not pass
through PipeWire or logical shared ports.

### ASIO glitches while Discord or OBS is active

First distinguish a client miss from a hardware failure:

```sh
sidealsa-stats --samples 20 --interval-ms 100
```

- Rising `client` with stable `core`, `hw_playback`, and `hw_capture` means the
  ASIO callback missed the bounded PRO handoff; the hardware timeline stayed
  intact.
- Rising `core` with stable hardware counters means a delayed hardware wake
  shortened the client handoff to preserve one playback period. The current
  sequence may fall back to silence, but ALSA continues.
- Rising `callback_overruns` means the host callback exceeded the full Q64
  period.
- Rising `rt_failures` means Wine could not promote the ASIO worker to the
  configured `pro_realtime_priority`.

The reference Q64/Q32 profile uses `pro_handoff_us = 500`. This leaves the
validated physical-write reserve while covering callback durations above the
old `250 us` budget. The hardware loop clamps that budget against current ALSA
delay and keeps one Q64 period reserved for an emergency write. Larger values
are not automatically safer: the profile validator rejects a handoff that would
consume the Q32 write deadline. For more margin, increase
`pro_latency_periods` to `1` in the control panel at the cost of one additional
client period.

## Development Checks

```sh
cargo fmt --all
cargo test --release --workspace --all-targets
cargo clippy --release --workspace --all-targets -- -D warnings
cargo build --release --workspace
```

Qt control-panel build check:

```sh
cmake -S crates/sidealsa-gui -B build-gui -DCMAKE_BUILD_TYPE=Release
cmake --build build-gui
```

ASIO build check:

```sh
cmake -S crates/sidealsa-asio -B build-asio -DCMAKE_BUILD_TYPE=Release
cmake --build build-asio --target \
  sidealsa-asio sidealsa-asio-probe sidealsa-asio-loopback-test
```

## License

SideALSA is licensed under GPL-3.0-or-later. See [`LICENSE`](LICENSE).
