# Independent ALSA PRO buffers and aligned starts

## Negotiation

The PRO ioplug application buffer is independent of the physical ALSA buffer.
It permits **one through `slot_count` logical periods**, in whole-period steps.
With the current eight-slot transport and Q64 this is B64–B512, even when the
physical device uses P64/B256. The application's period is still the daemon's
logical period; this does not implement a smaller callback quantum than that.

Configure the buffer in the ALSA application, not by reducing the physical
buffer in the SideALSA control panel. For example, for both duplex handles:

```c
snd_pcm_hw_params_set_period_size(pcm, params, 64, 0);
snd_pcm_hw_params_set_buffer_size(pcm, params, 64);
```

The plugin allocates its maximum FIFO once during open, then uses the negotiated
application size as its active capacity. Changing capacity while running or with
pending data is rejected. SHARED buffer geometry/behavior is unchanged.

## Startup and sequence semantics

PRO start no longer blocks while flushing prepared audio across hardware cycles.
It attempts a nonblocking flush and retains all remaining samples. Pointer queries
and normal I/O subsequently pump complete queued periods without blocking status
queries. Frames keep their assigned sequence; expired output remains an error.

The new optional `FEATURE_PRO_ALIGNED_START` handshake flag (bit 3, value 8) and
`StartProAligned { session_id }` control operation (opcode 10) let the daemon
coalesce starts of the two directional endpoints in an existing exclusive PRO
group. Only the connection's own session may be started. No sibling is started
without its own request.

- Starts arriving together, or during the one-logical-cycle pairing window,
  activate on a common hardware processing boundary.
- If the other endpoint is open but never starts, the requested direction starts
  independently after at most one additional logical cycle. There is no indefinite
  wait for another handle and no first-read-based postponement of capture.
- Starting/stopping one direction does not reset the already-running sibling.
- Capture and playback retain their distinct sequence domains when the configured
  PRO lead requires them; alignment does not mean falsifying equal sequence IDs.
- Widely separated starts or an application missing the small buffer's deadline
  can still produce EPIPE. Buffered capture is not silently skipped to conceal it.

The ALSA plugin uses aligned start when advertised. Older daemons retain ordinary
independent start semantics. Control protocol version 16 and SHM version 9 remain
unchanged; older clients can ignore the feature. Native clients can opt in via
`AudioStream::supports_aligned_pro_start()` / `start_aligned_pro()`.

### Playback pointer correction

The PRO playback pointer now advances from the first submitted sequence according
to the daemon's **consumption watermark**, rather than time elapsed since session
activation. It does not claim that unpublished startup data has already played.
After playback begins, missed output still advances the hardware watermark and
can produce a genuine client EPIPE; prepare/stop resets the local origin.

This allows a capture-driven application to explicitly start both PCMs with no
playback prefill, then read a capture period, process it, and write the output for
the current cycle. Set the playback start threshold to the ALSA boundary to use
explicit start. The application must still finish within the PRO handoff deadline.

Applications that prefill one or more periods retain that queued data and its
corresponding latency. The plugin does not throw away submitted silence or capture
samples to make a latency number smaller. Merely selecting B64 cannot remove a
prefill chosen by the application.

## Real hardware verification

The candidate daemon and locally loaded plugin were tested on the same Q64/P64/
B256 E1x2 hardware stream, with a native PRO loopback before and after. Only the
ALSA application buffer/prefill/start pattern changed within each comparison.
The device's internal output-0/input-4 digital return was used, not analog I/O.

| Application startup | Blocking | Nonblocking + poll | Result |
| --- | --- | --- | --- |
| B64, no prefill, capture first | 347 frames | 347 frames | Same as native PRO, 7.229 ms |
| B64, no prefill, playback first | 347 frames | 347 frames | Same as native PRO, 7.229 ms |
| B64, one-period prefill, capture first | 411 frames | 411 frames | Native +64 frames, 8.562 ms |
| B64, one-period prefill, playback first | 411 frames | 411 frames | Native +64 frames, 8.562 ms |

Each ALSA case returned all 45 pulses with identical min/max delay. All eight
cases had zero EPIPE, PRO miss, hardware XRUN, SHARED loss and generation deltas.
Native controls both measured 347 frames. Evidence:
`/tmp/opencode/aligned-alsa-29lgj5ye/`.

Earlier candidate runs also exercised B128/B256 and repeated B64 startup orders
(`/tmp/opencode/aligned-alsa-_49dqqw6/`, `aligned-alsa-67p5_0et/`).
Playback-only compatibility checks with the existing B256 probe, FIFO46 and
500 us work after polling passed with one/four-period prefill and zero EPIPE or
PRO/HW/SHARED miss deltas. Logs:
`/tmp/opencode/alsa-playback-b64-compat/` (despite its directory name this was
B256 with one-period prefill) and `alsa-playback-b256-compat/`.

These are controlled application patterns, not tests of every DAW's ALSA backend.
Applications must request the small buffer and choose an appropriate startup
pattern. Existing application-level mixers/converters may add their own latency.

Automated tests cover buffer negotiation through real libasound, negotiated FIFO
capacity, consumption-based pointers, nonblocking start, old-server fallback,
feature encoding/gating, aligned real control-server starts, sequence domains,
peer-start timeout, capture data integrity, pending-start cancellation on stop/
close, hardware-generation mismatch, and independent sibling lifecycle.
