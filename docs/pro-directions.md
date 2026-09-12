# Separate PRO Input and Output Opens

PRO remains exclusive to one owner group. A program may now open one capture
endpoint and one playback endpoint separately through the same client-library
instance. They have independent control connections, shared-memory regions,
notifications, start/stop state and local buffer cursors.

## Ownership

The new `OpenProDirection` operation is gated by `FEATURE_PRO_DIRECTIONS`.
Protocol version 16 and existing message encodings remain unchanged. Old classic
`OPEN_PRO` continues to reserve the complete duplex interface exclusively.

Directional pairing requires all of:

- Matching kernel `SO_PEERCRED` PID and UID.
- Matching nonzero 128-bit process token generated with `getrandom`.
- An unoccupied complementary direction.

A second capture or second playback open returns Busy. Another program cannot
claim the unused half; classic native/ASIO PRO is also Busy until the last
direction closes. Closing one half preserves the other. Each connection may
control only its own returned session ID. Reopening a closed half creates a new
mapping; old mappings cannot write into a new owner's endpoint.

The token is local to a client-library instance. Separate `aplay` and `arecord`
processes are not implicitly paired, and mixing independently loaded ALSA/ASIO
client libraries is not a supported attachment mechanism. Directional tokens,
connections and streams inherited across fork are rejected; exec a new process.
As with other Unix-socket ownership, deliberately retaining inherited/passed
descriptors can retain a reservation until those connections close.

## Independent Lifecycles

- Playback waits on its own early cycle notification and cannot discard capture.
- Capture is buffered independently of the playback deadline. Overflow reports
  capture discontinuity and wakes the reader rather than silently stalling it.
- Playback stop/prepare/close disables playback contribution and repeat-cache
  use without resetting capture. Capture lifecycle operations do not reset output.
- The remaining half keeps the exclusive group reservation, even if stopped.
- Actual hardware-generation changes still require client recovery; restarting
  either directional session affects its own mapping/lifecycle, not the sibling.
- Neither directional client failure nor closing a half restarts physical ALSA.

No hardware stream is aggregated or resampled by pairing. The profile still
describes one physical playback PCM and one capture PCM. Independent device
clocks are not synchronized by opening two handles.

## ALSA

Applications can open `sidealsa_pro` once for playback and once for capture in
either order. On a capable daemon, the plugin requests the matching directional
endpoints. Directional capture uses buffered availability/forward accounting;
playback retains its existing FIFO sequence and expiry rules.

With an older daemon the plugin falls back to the existing classic single-open
behavior. Separate opens therefore require an updated daemon and plugin. Do not
assume unrelated processes can split the same PRO reservation, or that
`snd_pcm_link()` is implemented for the two ioplug handles.

### Start alignment

The daemon advertises `FEATURE_PRO_ALIGNED_START` (bit 3). `StartProAligned`
(request opcode 10) starts only the caller's directional PRO session, allowing
starts within one logical cycle to share an activation boundary. An unstarted
peer never blocks hardware or an independent direction indefinitely. Playback
and capture sequence domains remain distinct when PRO lead is nonzero.
See [ALSA PRO buffers](alsa-pro-buffers.md) for negotiation and startup semantics.

## ASIO and RTL-Style Hosts

ASIO `Init` now discovers device information without reserving PRO. Reservation
occurs at `CreateBuffers`, once the host's selected directions are known:

- Input-only object: directional capture.
- Output-only object: directional playback.
- One object using both directions: classic exclusive duplex.

Two same-process COM objects may consequently select input and output separately.
Capture-only workers never submit playback, including while stopped. Output-only
workers use their own cycle notification/readiness hint rather than capture.
`DisposeBuffers` releases the reservation; live-worker timeout safety is retained.

Fresh reservation checks current device metadata against `Init` metadata. A
changed rate/profile/timing configuration requires driver reinitialization rather
than running with stale values. Existing old-daemon single-instance fallback is
retained, but cannot provide paired opens without the new feature.

This addresses the separate-open topology used by RTL-style tools. **RTL Utility
itself was not run**, and selecting input/output in its UI does not establish
which Wine audio backend it uses. The feature applies to direct SideALSA ASIO or
same-process ALSA PRO, not Wine PulseAudio routed to SHARED.

## Tests

```sh
cargo test --workspace
cargo test -p sidealsa-daemon --test directional_pro
bash scripts/test-asio-split.sh --run
```

The daemon integration test uses the real control listener, real client and SHM
with synthetic periods, including a genuinely separate process rejected as Busy.
State tests separately check same-token/different-credential rejection. ALSA
tests use real libasound handles/current plugin code against a mock server and
verify input samples and independent lifecycle operations.

The Wine test creates two real COM objects against a noninstalled synthetic
daemon, checks both start orders, sibling progress, duplicate rejection and fresh
duplex reopening. It uses a private socket/prefix and opens no physical PCM.
Wine initialization may start services within that private prefix.

These tests do not establish physical-device realtime performance, linked-start
semantics or end-to-end latency. The old hardware owner should not be replaced
mid-session merely to enable the feature; deploy matching components and reopen
clients in an explicitly authorized maintenance window.
