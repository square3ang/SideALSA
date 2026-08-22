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

The objects disable mmap and resampling because the current ioplug supports
RW interleaved S32_LE at the profile rate. The E1x2 hardware profile uses a
`64`-frame period and a three-period `192`-frame buffer; the ioplug stages
arbitrary transfer sizes into this preallocated client buffer. The reference
shared path consumes playback after three hardware periods (`192` frames),
absorbing normal PipeWire scheduling jitter without changing hardware period
timing.

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

## Limitations

- Static PipeWire objects are currently listed per profile port.
- No automatic profile-to-PipeWire node generation.
- No WirePlumber policy or session-manager customization.
- No ASIO frontend.
