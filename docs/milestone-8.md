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

The objects disable mmap and resampling because the current ioplug supports RW
interleaved S32_LE at the profile rate. Playback uses event-driven scheduling;
capture keeps PipeWire timer scheduling enabled because its graph must pace
bursty capture notifications. The E1x2 hardware profile keeps physical B192
while exposing an independent eight-period `shared_buffer_size = 512`. Playback
advertises a four-period minimum, so PipeWire negotiates Q64/B256. The ioplug
stages arbitrary transfer sizes into the preallocated B512 client ring.
The reference shared path consumes playback after three hardware periods
(`192` frames), absorbing normal PipeWire scheduling jitter without changing
hardware or PRO timing.

If a PipeWire callback is late, the ioplug uses all queued catch-up periods. If
no catch-up block exists, it leaves one explicit sequence gap and immediately
realigns later blocks to the three-period target. The daemon substitutes silence
for that one gap; lateness does not permanently reduce SHARED lookahead.

Capture notifications are level-triggered through eventfd. The client drains a
coalesced notification whenever it accepts a ready slot, scans past free ring
holes after a delayed callback, and uses an ALSA boundary-scale hardware
pointer. A full capture ring still advances its timeline and notification, so a
late PipeWire source can recover instead of remaining permanently stalled.

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

Frame-exact native device-loopback testing covered later Discord transitions
with the E1x2 `250 us` PRO handoff. Activation produced 13 exact-sequence
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

## Limitations

- Static PipeWire objects are currently listed per profile port.
- No automatic profile-to-PipeWire node generation.
- No WirePlumber policy or session-manager customization.
- No custom PipeWire client; integration remains through the ALSA ioplug.
