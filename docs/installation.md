# Installation

Install release binaries, the Qt control panel, ALSA plugin, `sidealsad`, and
PipeWire adapter configuration system-wide:

```text
scripts/install.sh
```

Script builds as invoking user, then requests `sudo` only for protected install
paths and systemd actions. Run the script as the normal desktop user so Wine,
PipeWire, and build paths retain that user's environment.

Installer defaults:

- binaries: `/usr/local/bin`
- root-owned profile: `/etc/sidealsa/profiles/topping-e1x2.toml`
- ALSA definitions: `/etc/alsa/conf.d/99-sidealsa.conf`
- PipeWire objects: `/etc/pipewire/pipewire.conf.d/99-sidealsa.conf`
- PipeWire Pulse scheduling: `/etc/pipewire/pipewire-pulse.conf.d/99-sidealsa.conf`
- daemon service: `sidealsad.service`
- socket: `/tmp/sidealsad.sock`
- control panel: `/usr/local/bin/sidealsa-control`
- privileged helper: `/usr/libexec/sidealsa-admin`
- polkit action: `org.sidealsa.configure`
- socket access: `audio` group when available
- realtime scheduling: profile-controlled, enabled by default

The profile is seeded only on first install. Reinstalling or upgrading never
overwrites it, including when `--force` is used. Uninstall also preserves it.
Profile defaults therefore do not migrate an existing installation. Review the
profile diff first, then explicitly install the repository version when wanted:

```text
scripts/install.sh --replace-profile
```

The installed ALSA and PipeWire adapter fragments are currently static for the
reference E1x2 port IDs (`line1` through `line4`, `mic1`, `mic2`, and
`input34` through `input910`). The installer rejects a custom profile missing
any of those IDs instead of installing adapters that cannot open their ports.
Automatic adapter generation for arbitrary validated profiles is not yet
implemented.

The Q64/Q32 E1x2 packet pipeline uses `pro_latency_periods = 0`,
`linked_playback_guard_frames = 32`, and `pro_handoff_us = 500`. Capture block N
is returned as playback block N after the bounded client handoff, so native PRO
and ASIO use the same callback timeline. The profile reports 64 frames of PRO
output latency. The 500-us handoff covers observed Wine callback overhead under
desktop capture load while retaining the validator's Q32 hardware-write reserve.

PipeWire adapter settings use a `64`-frame period and negotiate at least four
periods (`256` ALSA frames). SideALSA SHARED playback consumes data after three
logical periods (`192` frames) from an independent eight-period B512 ring, so
client scheduling has buffering without changing the physical B192 timeline.
PipeWire's global clock quantum stays distribution-managed. The installed
PipeWire and PipeWire Pulse fragments cap their realtime priority at `10`. The
reference priority order is linked hardware `88`, ASIO callback `86`,
WirePlumber `83`, and PipeWire/Pulse `10`.
The callback keeps normal scheduling and reports `pro_realtime_failures` when
the Wine process lacks realtime scheduling rights. Existing profiles that omit
`pro_realtime_priority` derive it as two below `realtime_priority`.

Restart user PipeWire and WirePlumber after installation:

```text
systemctl --user enable --now pipewire.socket pipewire-pulse.socket
systemctl --user restart pipewire.service pipewire-pulse.service wireplumber.service
```

WirePlumber does not need a separate Lua rule. PipeWire creates SideALSA ALSA
adapter nodes from the installed fragment; WirePlumber manages those nodes and
does not receive default-sink or default-source changes from the installer.

Install without starting the daemon:

```text
scripts/install.sh --no-start
```

Skip the Qt control panel and polkit helper:

```text
scripts/install.sh --no-gui
```

The control panel edits all timing fields. Applying a change authenticates with
polkit, validates and atomically replaces the root-owned profile, restarts the
fixed system service, and verifies the new root-owned socket peer against
systemd's `MainPID` and the complete loaded profile fingerprint. On failure it
restores the original profile and verifies the rollback restart. Active clients
disconnect during either restart and must reconnect.

Install Wine ASIO binaries when CMake, `winegcc`, and `winebuild` are available:

```text
scripts/install.sh --with-asio
```

This installs the system Wine artifacts only. Register them in each Wine or
Proton prefix with `scripts/install-asio.sh --no-build --wine-prefix PATH` (or
the corresponding Steam-prefix options).

Remove files owned by the installer:

```text
scripts/uninstall.sh
```

Changed files are preserved during uninstall. Use `--force` only when removal
of changed files is intended.

Package staging is supported:

```text
DESTDIR="$PWD/stage" scripts/install.sh --no-build
DESTDIR="$PWD/stage" scripts/uninstall.sh
```
