# Automatic SHARED Reconnection

`sidealsa-reconnect` is a non-RT, unprivileged user-session worker. It detects a
lost daemon control connection, waits for a new successful handshake and a
running hardware timeline, then refreshes the existing SHARED PipeWire links.
It never opens physical ALSA hardware, switches to another sound device, or
restarts PipeWire, pipewire-pulse, WirePlumber, or applications.

## Enable

A normal installation with PipeWire integration installs and enables
`sidealsa-reconnect.service` for the invoking desktop user. Updating restarts
only this small worker, not PipeWire. `--no-start`, staged installations and
installations without a running user service manager print activation
instructions instead. For those cases, run as your desktop user:

```bash
systemctl --user enable --now /usr/local/lib/systemd/user/sidealsa-reconnect.service
systemctl --user status sidealsa-reconnect.service
journalctl --user -u sidealsa-reconnect.service -f
```

Use the actual installation prefix if it differs from `/usr/local`. Each desktop
user needs their own worker and permission to access the SideALSA socket. The
worker requires `pw-dump`, `pw-cli`, `pw-link`, and coreutils `timeout`.

To run from a checkout without installing:

```bash
cargo build --release -p sidealsa-cli --bin sidealsa-reconnect
target/release/sidealsa-reconnect --socket /tmp/sidealsad.sock
```

Run only one worker per session/socket. `--once` performs a manual one-shot link
refresh. The installed service uses `--initial-refresh` so starting or updating
the worker after a daemon restart also recovers already-stale connections.

## Recovery procedure

1. Poll the persistent daemon control connection every 500 ms outside all audio
   threads. On disconnect, retry the handshake until hardware is processing.
2. Read the current PipeWire graph. Select ALSA nodes whose `api.alsa.path`
   matches a SHARED port in the daemon's current device information.
3. Snapshot the existing links and node/port identities, then remove only those
   links. Wait one second for the adapters to become idle and close stale PCMs.
4. Restore the original port-to-port links. Existing policy-restored links are
   left alone. Missing/reused ports, a changed PipeWire core cookie, or a newly
   selected application route invalidate the old link instead of redirecting
   audio back against the user's choice. Pre-existing fan-out is retained.

Each external command has a five-second timeout. Link restoration retries are
bounded; unrecoverable errors appear in the user service journal. A link refresh
is not an acoustic health check. Configuration negotiation failures still need
diagnosis, and changed channel geometry/rate may require application-side
reconfiguration. PRO and ASIO clients are not automatically reconnected.

The hardware restart and brief relinking gap remain audible interruptions. This
feature preserves processes and ongoing application streams; it does not promise
gapless hardware reconfiguration. New PipeWire configuration files are also not
dynamically loaded by the worker.

## Verification

The first daemon-only restart test stalled existing playback/capture, while
manually unlinking/relinking recovered the same processes. The automatic worker
then reproduced that recovery on the reference E1x2:

- Daemon PID: 207702 to 233613.
- PipeWire/Pulse/WirePlumber PIDs stayed 1053/1303/1055.
- Worker reported refreshing ten SideALSA links after detecting the new daemon.
- Original `pw-cat` playback and recording processes remained alive.
- The Line 1 / Input 5/6 internal digital return recording resumed, containing
  777,216 frames and the expected 0.005-peak test signal across the observation.
- No manual relink or desktop audio service restart was used in this run.

Evidence: `/tmp/opencode/daemon-reconnect-5kwbc48k/`, including `watcher.log`,
PID/graph snapshots and the digital return. This tested the checkout release
worker against the installed audio stack, not a full system reinstall. It did
not test channel-count/rate changes or every PipeWire/session-manager version.

Unit tests cover selection scope, object-ID reuse, vanished ports, playback and
capture retargeting, duplicate links and fan-out. Offline installer fixtures
verify the managed executable/service, socket argument and staging isolation.
