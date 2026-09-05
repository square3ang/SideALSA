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

The current reference uses `hardware_period_size = 32` with logical Q64
transfers. Existing P64 profiles remain unchanged on upgrade. After installing
the engine with divided-period direct-mode support, explicitly edit that field
or use `--replace-profile` to adopt the new reference. This changes transport
batching, not the ASIO buffer size or the Q128 startup reserve.

Optional [digital loopback startup normalization](startup-loopback.md) was
tested at 376 frames but is disabled in the reference after a later runtime
shift exceeded 8 ms. It requires the E1x2's exact internal output-0/input-4 return;
do not copy that routing to another interface without verifying it. Update the
daemon, validation helper, service unit, and profile together. Failed
qualification exits 78 and is not automatically restarted by the installed unit.

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

The E1x2 PRO pipeline uses Q64 client blocks and P32 physical ALSA periods with a
B256 hardware ring. `pro_latency_periods = 0`, timer scheduling is disabled, and
`linked_phase_max_attempts = 0`. Linked startup primes the ALSA playback ring
with a Q128 base of silence selected by `playback_queue_periods = 2` and starts
playback and capture together. When explicitly enabled, digital loopback
qualification may append up to Q64 of silence before clients are admitted.
Capture block N is returned as playback target
N, so native PRO and ASIO use the same callback timeline.

ALSA playback and capture poll readiness jointly start each cycle, with
`avail_min = 64` in both directions and whole-Q64 transfers. The
playback-ready eventfd ends the client wait early; `pro_handoff_us = 1000`
bounds that wait. The engine uses the post-poll `snd_pcm_avail_update()` value
and reserves Q16 for fallback selection, mixing, and the ALSA write, shortening
the deadline further after a late hardware wake. It does not call the deprecated
`snd_pcm_hwsync()` in this period-driven path.
The reference path does not use live-delay budgeting,
`linked_playback_guard_frames`, or a userspace queue-target sleep. If exact PRO
playback is unavailable, the daemon
repeats the last valid PRO period without moving the sequence. It uses silence
before the first valid block and after lifecycle or hardware-generation changes.
Current SHARED playback is mixed after the PRO selection.

The ALSA ioplug keeps SideALSA SHARED transfers at Q64 and uses the independent
B512 daemon ring. It aggregates four internal blocks per Q256 external period.
PipeWire playback negotiates B768, uses `api.alsa.start-delay = 256`, and keeps
`128` frames of headroom. This preserves the Q256 graph cadence while raising
the steady playback target to Q384, adding 2.67 ms of SHARED-only scheduling
margin. Capture does not add this playback margin. SideALSA SHARED playback
consumes data after seven internal periods (`448` frames), so desktop scheduling
remains isolated from the physical B256 timeline. Playback and capture adapters
keep PipeWire timer scheduling enabled.
PipeWire's global clock quantum stays distribution-managed. The installed
PipeWire, PipeWire Pulse, and WirePlumber fragments cap their realtime priority
at `10`. On the reference PREEMPT_RT host the xHCI IRQ thread runs at `50`, so
the priority order is xHCI IRQ `50`, linked hardware `48`, ASIO callback `46`,
and desktop audio/session-manager work `10`. USB completion IRQs must preempt
every userspace audio worker; assigning PRO or hardware above the xHCI IRQ can
move physical duplex phase when another isochronous endpoint starts or stops.
Correct priority ordering removes that known inversion but does not guarantee
fixed analog phase: strict concurrent ASIO/build and no-PRO reacquisition tests
have still observed sub-Q64 moves without an ALSA XRUN or timeline reset.
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
