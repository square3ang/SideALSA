# Milestone 8: PipeWire integration

## Scope

PipeWire uses the existing ALSA ioplug. No PipeWire client or custom graph code
was added.

`configs/pipewire/pipewire.conf.d/sidealsa.conf` creates PipeWire adapter
objects for the profile's shared playback and capture PCMs:

```text
api.alsa.pcm.sink   -> sidealsa_line1 .. sidealsa_line4
api.alsa.pcm.source -> sidealsa_mic1 .. sidealsa_input910
```

The PipeWire process must be able to resolve those names through
`configs/asound.sidealsa.conf`, either by setting `ALSA_CONFIG_PATH` or by
installing the ALSA definitions in the normal ALSA configuration path. The
plugin directory must likewise be installed or exposed through
`ALSA_PLUGIN_DIR`.

The PipeWire and PipeWire Pulse fragments set realtime priority `10`, below the
ASIO callback at `86` and SideALSA linked hardware worker at `88`. Heavy desktop
audio work can therefore lose SHARED data without preempting PRO or hardware.

The objects disable mmap; the current ioplug supports RW interleaved S32_LE at
the profile rate and SideALSA performs no resampling. Playback and capture keep
PipeWire timer scheduling enabled, separating desktop graph pacing from the
daemon's internal Q64 notifications. The E1x2 hardware profile uses physical
B256 while exposing an independent eight-Q64-period `shared_buffer_size = 512`.
The ioplug aggregates four internal blocks per Q256 external period. Playback
offers three periods and PipeWire negotiates B768; `api.alsa.start-delay = 256`
uses the extra period only to keep startup primed while the first graph block
arrives. The steady target remains Q256. Transfers remain Q64 at the daemon
boundary and use the preallocated B512 SHARED ring.
The reference shared path consumes playback after seven logical periods
(`448` frames), absorbing desktop scheduling jitter without changing hardware
or PRO timing.

If a PipeWire callback is late, the ioplug uses all queued catch-up periods. If
no catch-up block exists, it leaves one explicit sequence gap and immediately
realigns later blocks to the seven-period target. The daemon substitutes silence
for that one gap; lateness does not permanently reduce SHARED lookahead.

Capture notifications are level-triggered through eventfd. The client drains a
coalesced notification whenever it accepts a ready slot, scans past free ring
holes after a delayed callback, and uses an ALSA boundary-scale hardware
pointer. A full capture ring still advances its timeline and notification, so a
late PipeWire source can recover instead of remaining permanently stalled.

An ioplug handle that observes a closed daemon control socket enters ALSA's
`DISCONNECTED` state. Existing PipeWire poll registrations cannot be retargeted
to a new stream, so the control panel restarts active user PipeWire services
after profile apply or rollback and lets the static adapters be created again.
An unexpected daemon restart still requires the same manual user-service
restart; automatic ioplug reconnection is not implemented yet.

## Local Test

Use the project config directory as `XDG_CONFIG_HOME` for a temporary session:

```text
XDG_CONFIG_HOME="$PWD/configs" \
ALSA_PLUGIN_DIR="$PWD/target/debug" \
ALSA_CONFIG_PATH="$PWD/configs/asound.sidealsa.conf" \
pipewire
```

Then inspect nodes with `pw-cli list-objects Node`. Raw test streams can target
the named nodes:

```text
pw-cat --playback --raw --format s32 --rate 48000 --channels 2 \
  --target sidealsa-line1 - < /dev/zero
pw-cat --record --raw --format s32 --rate 48000 --channels 1 \
  --target sidealsa-mic1 - > /dev/null
```

## Verification

Temporary PipeWire session checks passed:

- All ten configured SideALSA nodes appeared in `pw-cli`.
- Playback through `sidealsa-line1` reached PipeWire and SideALSA.
- Capture through `sidealsa-mic1` reached PipeWire and SideALSA.
- Each run processed 4668 hardware periods.
- Hardware playback/capture XRUNs, PRO misses, shared misses, and timeline
  resets stayed at zero.

Raw capture processed 960000 frames over 20.020 seconds with PipeWire timer
scheduling. Source and client graph errors, hardware XRUNs, shared overruns, and
timeline resets stayed at zero.

A live Discord WebRTC session used `sidealsa-line2` for stereo playback and
`sidealsa-mic1` for mono capture. PipeWire reported zero graph errors for both
SideALSA nodes and both WebRTC streams. Playback recorded one shared underrun
during activation, then the counter remained unchanged; shared overruns,
hardware XRUNs, and timeline resets stayed at zero. A simultaneous 7500-period
normal-priority native PRO test recorded one client deadline miss without a
core miss or hardware discontinuity. Six Wine ASIO probe runs under the same
Discord load completed 4496 callbacks with no new PRO deadline misses, shared
misses, hardware XRUNs, or timeline resets. A longer probe then completed two
15-second start legs, reporting 11260 callbacks after its restart, with the same
zero counter deltas.

Earlier Q256 acceptance used the B256/guard48/500-us/SHARED7 profile. Direct
`aplay` and `arecord` both negotiated `period_size=256`, `periods=2`, and
`buffer_size=512`. Under 24 in-process ASIO stress workers, 512 MiB of memory
traffic, a 350 us callback workload, and simultaneous PipeWire playback and
capture, two consecutive runs completed 31723 PRO playback blocks. Analog
loopback remained fixed at 374 frames, all PipeWire adapter and client graph
errors remained zero, and PRO, SHARED, hardware-XRUN, generation, and reset
counter deltas remained zero. A simultaneous PipeWire Pulse `pacat` playback and
record run added 15877 PRO blocks with the same fixed phase and zero error
deltas. The two absolute SHARED underruns visible afterward came from an earlier
rejected Q256 IRQ-scheduling trial and did not increase during acceptance.

Frame-exact native device-loopback testing covered later Discord transitions
with the then-current E1x2 `250 us` PRO handoff. Activation produced 13 exact-sequence
silence fallbacks and one lost test pulse, but all 233 detected pulses remained
at 413 frames. Disconnect produced one fallback and all 156 pulses remained at
413 frames. Neither transition produced a hardware XRUN, core miss, timeline
reset, or latency phase change.

Earlier Q32 packet-pipeline revisions were verified separately from those older
Discord results. The following results predate the current zero-lead profile;
current zero-lead acceptance is documented in `milestone-asio.md`:

- PipeWire raw playback negotiated `period-size = 64`, `period-num = 4`, ran at
  Q64/48 kHz, and reported graph `ERR = 0`.
- A 12000-period PRO run remained exactly 373 frames (`7.771 ms`) across real
  PipeWire activation with zero PRO miss, hardware XRUN, or timeline reset.
- A 12000-period delayed SHARED run produced 7291 expected shared underruns while
  concurrent PRO remained exactly 373 frames with zero PRO or hardware failure.
- A 12000-period run combining PipeWire playback and a `2 ms` PRO delay every
  17th sequence produced 705 PRO fallbacks, but every detected pulse remained
  exactly 373 frames and the hardware timeline did not reset.
- The accepted three-Q32 startup reserve completed a live 15000-period Discord
  playback, microphone, and screen-capture run with 234 of 234 pulses at exactly
  347 frames (`7.229 ms`). PRO misses, SHARED misses, hardware XRUNs, and
  timeline resets stayed at zero; PipeWire's SideALSA and WebRTC audio nodes
  reported `ERR = 0`.
- Under the same live Discord load, a `2 ms` PRO delay every 17th sequence
  produced 883 exact-sequence fallbacks and 14 lost test pulses. All 220 detected
  pulses remained exactly 347 frames, with zero core miss, SHARED miss, hardware
  XRUN, or timeline reset.
- A post-Q32-wakeup experiment reducing startup reserve from three Q32 packets
  to two was rejected. Discord activation produced three genuine playback XRUNs
  and three timeline resets. Restoring the 96-frame reserve removed them.
- SHARED playback arms on its first valid block and records only the first miss
  in an outage episode. Inactive quantum-zero nodes therefore no longer add one
  underrun per hardware period; the next valid block rearms accounting.
- Pre-refill zero-lead acceptance ran simultaneous PipeWire playback and capture
  beside an 8000-period RT PRO loopback. All 125 pulses remained exactly 436
  frames; PRO deadline misses, hardware XRUNs, and timeline resets stayed at
  zero. A separate 12000-period delayed-SHARED run retained all 188 PRO pulses
  at the same fixed phase with the same zero failure deltas.
- With refill scheduled two Q32 periods earlier and SHARED work removed from the
  critical write interval, a fresh 6000-period RT PRO run remained exactly 362
  frames before and during simultaneous PipeWire playback and capture. All 94
  pulses were detected with zero PRO miss, hardware XRUN, or timeline reset.
- A controlled 15-second 1 kHz PipeWire playback isolated a later startup-clock
  defect. With Q256/B512, all 2779 ioplug transfers were exact Q256 writes with
  no short transfer or `EAGAIN`, but 37 discontinuities were already present at
  the ioplug input. PipeWire reported `delay=128`, `target=256`, and a DLL rate
  correction that rose past `1.02`; its monitor signal before the ALSA adapter
  remained sample-perfect. Q256/B768 plus a Q256 start delay retained the same
  steady target and removed the error: 721152 transferred frames had zero large
  deltas, the maximum adjacent delta matched the source at `24792408`, and DLL
  correction remained within `0.999891..1.000133`. The production plugin then
  repeated playback with PipeWire `ERR=0` and no SHARED, hardware-XRUN,
  generation, or timeline-reset delta.

## Limitations

- Static PipeWire objects are currently listed per profile port.
- No automatic profile-to-PipeWire node generation.
- No WirePlumber policy or session-manager customization.
- No custom PipeWire client; integration remains through the ALSA ioplug.
- The Q256 start delay is static reference-profile configuration. A different
  external period geometry must update the PipeWire adapter fragment.
- The B768 startup correction has controlled-sine coverage but still needs a
  longer browser/Discord desktop soak.
