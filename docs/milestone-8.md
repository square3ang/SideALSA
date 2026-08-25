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
ASIO callback at `15` and SideALSA hardware workers at `87/88`. Heavy desktop
audio work can therefore lose SHARED data without preempting PRO or hardware.

The objects disable mmap and resampling because the current ioplug supports RW
interleaved S32_LE at the profile rate. Playback uses event-driven scheduling;
capture keeps PipeWire timer scheduling enabled because its graph must pace
bursty capture notifications. The E1x2 hardware profile keeps physical B192
while exposing an independent four-period `shared_buffer_size = 256`. The
ioplug stages arbitrary transfer sizes into this preallocated client buffer.
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

## Limitations

- Static PipeWire objects are currently listed per profile port.
- No automatic profile-to-PipeWire node generation.
- No WirePlumber policy or session-manager customization.
- No ASIO frontend.
