# Milestone 7: ALSA ioplug

## Scope

`sidealsa-alsa` builds `libasound_module_pcm_sidealsa.so`. ALSA owns only the
logical PCM handle; `sidealsad` remains the sole owner of the physical ALSA
device.

The small C layer in `crates/sidealsa-alsa/src/plugin.c` handles the ALSA
external-plugin ABI, configuration, hw constraints, poll events, and ioplug
callbacks. Rust in `crates/sidealsa-alsa/src/lib.rs` owns the
`sidealsa-client` stream, event waits, sequence numbers, and S32_LE frame
copies.

Supported configuration modes:

```text
pcm.sidealsa_pro {
    type sidealsa
    mode pro
}

pcm.sidealsa_line1 {
    type sidealsa
    mode shared
    port "line1"
}
```

`configs/asound.sidealsa.conf` contains the current profile's PRO, playback,
and capture examples.

## Build

```text
cargo build -p sidealsa-alsa
```

The plugin is written to `target/debug/libasound_module_pcm_sidealsa.so`.

For local testing, point ALSA at the build output and example configuration:

```text
ALSA_PLUGIN_DIR="$PWD/target/debug" \
ALSA_CONFIG_PATH="$PWD/configs/asound.sidealsa.conf"
```

## Verification

Current Topping E1x2 checks passed with raw S32_LE streams:

- PRO playback through `aplay`: status zero, zero hardware XRUNs.
- PRO capture through `arecord`: status zero, zero hardware XRUNs.
- SHARED `line1` playback and `mic1` capture: status zero, zero hardware XRUNs.
- Simultaneous PRO and SHARED playback: status zero, one startup PRO miss,
  one shared underrun, zero hardware XRUNs, zero timeline resets.

Unit tests cover the ioplug buffer minimum and interleaved S32_LE area copies.
SHARED playback also realigns its sequence after a late callback. If PipeWire
has no catch-up block, one missing sequence becomes one daemon underrun; the
endpoint then disarms until the next valid block resumes at the configured
lookahead. A paused endpoint does not increment the counter every period.

## Limitations

- S32_LE only.
- RW interleaved access only.
- Playback and capture transfers are staged into preallocated period buffers, so
  arbitrary ALSA transfer sizes are accepted up to the configured client buffer.
- No resampling, format conversion, mmap access, or PipeWire-specific code.
- Current ioplug client buffer is at least two periods because ALSA ioplug
  rejects a one-period client buffer. E1x2 SHARED playback permits a third Q256
  period, so PipeWire negotiates B768 and can prime startup without changing its
  steady Q256 target. Daemon staging remains eight Q64 blocks in the independent
  B512 SHARED ring, separate from the physical B256 hardware queue.
