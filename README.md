# SideALSA

SideALSA is a userspace professional-audio layer built on ALSA. It owns one
physical USB audio device and exposes two independent client paths:

- **PRO**: exclusive, low-latency, complete physical channel layout.
- **SHARED**: buffered logical ports for PipeWire and desktop applications.

The hardware timeline keeps running when a client misses a deadline. A client
miss becomes silence and a diagnostic counter, not an ALSA restart.

Initial reference device: **Topping E1x2 OTG**.

Current profile: `48 kHz`, `S32_LE`, `64`-frame periods, `192`-frame hardware
buffer, 8 playback channels, 10 capture channels.

## Status

Working paths:

- Direct duplex ALSA ownership through `sidealsad`.
- PRO client protocol and exclusive ownership.
- Shared logical playback/capture ports.
- ALSA ioplug for raw `sidealsa_pro` and hidden PipeWire backing PCMs.
- PipeWire adapters backed by the ALSA ioplug.
- Experimental 64-bit Wine/Proton ASIO frontend.

Not implemented:

- ALSA ioplug mmap access.
- Resampling or format conversion.
- Advanced routing or DSP.
- 32-bit ASIO/WoW64 frontend.
- GUI control panel.

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
  rust cmake gcc pkgconf wine wine-tools
```

Build requirements:

- Rust toolchain with Cargo.
- ALSA development headers and `pkg-config`.
- CMake, GCC, and GNU make or Ninja.
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

Install daemon, ALSA plugin, initial profile, ALSA definitions, PipeWire adapters, and
systemd service:

```sh
sudo ./scripts/install.sh --no-build
```

The installer creates the selected profile only when it does not exist. Later
installs preserve `/etc/sidealsa/profiles/*.toml`, including with `--force`.

Installer defaults:

```text
daemon:       /usr/local/bin/sidealsad
profile:      /etc/sidealsa/profiles/topping-e1x2.toml
ALSA config:  /etc/alsa/conf.d/99-sidealsa.conf
PipeWire:     /etc/pipewire/pipewire.conf.d/99-sidealsa.conf
ALSA plugin:  /usr/lib/alsa-lib/libasound_module_pcm_sidealsa.so
socket:       /tmp/sidealsad.sock
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
period_size: 64
buffer_size: 192
```

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
consumed block, so startup silence does not count as an underrun. Genuine
missing blocks after arming increment `shared_underruns`.

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
`shared_underruns`, because a client can receive silence without a PipeWire
graph xrun.

## Build ASIO

SideALSA ASIO uses PRO directly. It does not use the ALSA ioplug.

Build 64-bit Wine ASIO artifacts:

```sh
cmake -S crates/sidealsa-asio \
  -B build-asio \
  -DCMAKE_BUILD_TYPE=Release
cmake --build build-asio --target sidealsa-asio sidealsa-asio-probe
```

Artifacts:

```text
build-asio/sidealsa-asio64.dll
build-asio/sidealsa-asio64.dll.so
build-asio/sidealsa-asio-probe.exe
```

The runtime layout also provides Wine 10+ `sidealsa-asio.dll` aliases.

The ASIO frontend expects `64`-frame ASIO buffers, `48 kHz`, 10 inputs, and 8
outputs. Only one PRO owner is allowed. Close any other PRO client before
starting a DAW or ASIO probe.

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
wine build-asio/sidealsa-asio-probe.exe
```

The probe checks COM activation, channel counts, `64`-frame buffer negotiation,
sample rate, buffer pointers, start/stop, and callback delivery.

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

## Development Checks

```sh
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release --workspace
```

ASIO build check:

```sh
cmake -S crates/sidealsa-asio -B build-asio -DCMAKE_BUILD_TYPE=Release
cmake --build build-asio --target sidealsa-asio sidealsa-asio-probe
```

## License

SideALSA is licensed under GPL-3.0-or-later. See [`LICENSE`](LICENSE).
