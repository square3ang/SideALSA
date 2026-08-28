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
- WirePlumber scheduling: `/etc/wireplumber/wireplumber.conf.d/99-sidealsa.conf`
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
profile diff first, then explicitly install the repository version when wanted.
Profiles created before the USB IRQ-order fix retain their saved priorities;
set `realtime_priority = 48` and `pro_realtime_priority = 46`, or replace an
otherwise unmodified profile:

```text
scripts/install.sh --replace-profile
```

Use `--preserve-pipewire` when updating SideALSA binaries or profiles without
rewriting the installed PipeWire adapter files or restarting the user PipeWire
services. This mode requires the three adapter files to be present in the
existing SideALSA install manifest and retains their previous ownership hashes.

The reference profile enables
`shared_playback_repeat_on_underrun = true`. Existing profiles that omit it
retain the previous silence fallback. The control panel exposes the setting as
a checkbox. When enabled, a SHARED playback port repeats its last valid logical
period until exact-sequence playback resumes; a long outage can therefore
produce a repeating tone.

The installed ALSA and PipeWire adapter fragments are currently static for the
reference E1x2 port IDs (`line1` through `line4`, `mic1`, `mic2`, and
`input34` through `input910`). The installer rejects a custom profile missing
any of those IDs instead of installing adapters that cannot open their ports.
Automatic adapter generation for arbitrary validated profiles is not yet
implemented.

The Q64/Q32 E1x2 packet pipeline uses `pro_latency_periods = 0`,
`linked_playback_guard_frames = 32`, `linked_phase_max_attempts = 8`, and
`pro_handoff_us = 500`. Capture block N
is returned as playback block N after the bounded client handoff, so native PRO
and ASIO use the same callback timeline. The profile reports 64 frames of PRO
output latency. The 500-us handoff covers observed Wine callback overhead under
desktop capture load. A delayed hardware wake shortens that handoff when needed
to retain one Q64 period for the next ALSA write. Linked startup primes 216
frames: one Q64 capture interval, a 32-frame guard, three Q32 refill-headroom
periods, and the 24 frames consumed by the handoff.

Before the control socket opens, each startup attempt runs 750 silence cycles,
one second at 48 kHz/Q64, using the normal handoff and playback-write cadence.
Any capture-to-playback write interval shorter than the configured handoff less
one eighth of a physical period rejects that start. A rejected attempt restarts
the linked hardware before clients exist; eight failed attempts fail daemon
startup instead of exposing an unqualified stream. These maintenance transfers
do not advance the published hardware timeline. Runtime client misses and
normal XRUN recovery never invoke the startup qualifier.

The ALSA ioplug keeps SideALSA SHARED transfers at Q64 and uses the independent
B512 daemon ring. It aggregates four internal blocks per Q256 external period.
PipeWire playback negotiates B768 and uses `api.alsa.start-delay = 256`, allowing
startup silence and the first graph block to coexist without moving the
steady-state Q256 target. Capture does not add this startup period. SideALSA
SHARED playback consumes data after seven internal periods (`448` frames), so
desktop scheduling remains isolated from the physical B256 timeline. Playback
and capture adapters keep PipeWire timer scheduling enabled.
PipeWire's global clock quantum stays distribution-managed. The installed
PipeWire, PipeWire Pulse, and WirePlumber fragments cap their realtime priority
at `10`. On the reference PREEMPT_RT host the xHCI IRQ thread runs at `50`, so
the priority order is xHCI IRQ `50`, linked hardware `48`, ASIO callback `46`,
and desktop audio/session-manager work `10`. USB completion IRQs must preempt
every userspace audio worker; assigning PRO or hardware above the xHCI IRQ can
move physical duplex phase when another isochronous endpoint starts or stops.
The callback continues with normal scheduling and reports
`pro_realtime_failures` when the Wine process lacks realtime scheduling rights.
Existing profiles that omit
`pro_realtime_priority` derive it as two below `realtime_priority`.

With `device.realtime = true`, `sidealsad` locks current and future mappings with
`mlockall` and prefaults 64 KiB of the hardware-thread stack before streaming.
Startup fails instead of running an unprotected RT loop when memory locking is
unavailable. The installed service supplies `LimitMEMLOCK=infinity`.

After a normal installation, restart user PipeWire and WirePlumber if needed:

```text
systemctl --user enable --now pipewire.socket pipewire-pulse.socket
systemctl --user restart pipewire.service pipewire-pulse.service wireplumber.service
```

WirePlumber does not need a Lua rule. PipeWire creates SideALSA ALSA adapter
nodes from the installed fragment; the WirePlumber fragment only caps its data
loop priority and does not change default-sink or default-source policy.

Install without starting the daemon:

```text
scripts/install.sh --no-start
```

Skip the Qt control panel and polkit helper:

```text
scripts/install.sh --no-gui
```

The control panel groups timing fields into buffer, duplex, and scheduling tabs
with persistent status and apply actions. Direct numeric inputs avoid detached
platform step controls and ignore unfocused wheel events. Applying a change
authenticates with polkit, validates and atomically replaces the root-owned
profile, restarts the fixed system service, and verifies the new root-owned
socket peer against systemd's `MainPID` and the complete loaded profile
fingerprint. On failure it restores the original profile, verifies the rollback
restart, and keeps pending GUI values available for correction. After a
successful restart or a reported failure that may have restarted the daemon,
including rollback, the control panel restarts active user PipeWire services so
static adapters reopen against the current daemon. Direct PRO and SHARED clients
still need to reconnect. A warning with the manual `systemctl --user restart`
command is shown if the user-service restart fails or times out; an otherwise
verified daemon change remains applied.

Unless `--preserve-pipewire` is used, the installer also stops active user audio
before restarting `sidealsad` and waits for the new socket before restoring
those services. This prevents static PipeWire adapters from opening before the
daemon is ready. Preserve mode leaves those user services running.

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
