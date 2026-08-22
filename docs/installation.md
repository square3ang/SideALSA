# Installation

Install release binaries, the ALSA plugin, `sidealsad`, and PipeWire adapter
configuration system-wide:

```text
scripts/install.sh
```

Script builds as invoking user, then requests `sudo` only for protected install
paths and systemd actions. `sudo scripts/install.sh` is also accepted; script
re-enters as original user before building.

Installer defaults:

- binaries: `/usr/local/bin`
- user-owned profile: `/etc/sidealsa/profiles/topping-e1x2.toml`
- ALSA definitions: `/etc/alsa/conf.d/99-sidealsa.conf`
- PipeWire objects: `/etc/pipewire/pipewire.conf.d/99-sidealsa.conf`
- daemon service: `sidealsad.service`
- socket: `/tmp/sidealsad.sock`
- socket access: `audio` group when available
- realtime scheduling: profile-controlled, enabled by default

The profile is seeded only on first install. Reinstalling or upgrading never
overwrites it, including when `--force` is used. Uninstall also preserves it.

PipeWire adapter settings match the plugin and hardware at a `64`-frame period
and three periods (`192` ALSA frames). SideALSA SHARED playback consumes data
after three hardware periods (`192` frames), so client scheduling has buffering
without changing the hardware timeline. PipeWire's global clock quantum stays
distribution-managed.

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

Install Wine ASIO binaries when CMake, `winegcc`, and `winebuild` are available:

```text
scripts/install.sh --with-asio
```

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
