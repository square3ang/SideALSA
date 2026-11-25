# Sycon Mode (Direct UAC2 Backend)

Sycon mode replaces the ALSA hardware engine with a userspace USB Audio Class 2
driver. `sidealsad` opens the device's usbfs node, detaches `snd-usb-audio` from
the audio interfaces and streams isochronous URBs itself. Everything above the
engine (PRO, SHARED, ioplug, ASIO, statistics) is unchanged.

Select it per profile:

```toml
[device]
# ...existing rate / period / channel settings...
backend = "sycon"          # default: "alsa"

[device.sycon]
vendor_id = 0x152a
product_id = 0x8755
# port = "1-11"            # optional sysfs port; required only with two identical devices
packets_per_urb = 1        # 1..=8; one packet = one bInterval (125 us at high speed)
playback_urbs = 12         # 2..=16 OUT URBs in flight
capture_urbs = 12          # 2..=16 IN URBs in flight
# safety_frames = 6        # extra OUT ring margin; default one nominal packet, max 10 ms
```

[`profiles/topping-e1x2-sycon.toml`](../profiles/topping-e1x2-sycon.toml) is the
E1x2 reference. ALSA-only keys (`device = "hw:..."`, buffer, queue, timer and
linked-duplex tuning) must still form a valid ALSA profile but are ignored at run
time, so the same file switches back by setting `backend = "alsa"`. `pro_handoff_auto`, `pro_handoff_us`,
realtime priorities and the SHARED settings apply as usual; `pro_latency_periods` must be `0`.

## Architecture

```
            usbfs fd (/dev/bus/usb/BBB/DDD)
                 |  iso URBs, poll()
          +------v------+
          |  sycon-usb  |  SCHED_FIFO realtime_priority
          |  I/O thread |  submit/reap, feedback, packet sizing
          +--+-------^--+
   capture   |       |   playback
   FrameRing v       |   FrameRing      (SPSC, preallocated)
          +--+-------+--+
          | PRO cycle   |  SCHED_FIFO realtime_priority - 1
          | thread      |  whole periods -> daemon bridge
          +-------------+
```

- **Startup.** Claim the audio control interface first, then the streaming
  interfaces. Select alt 0, resolve each terminal's clock source through clock
  selectors/multipliers, `SET_CUR` the rate (checked against `GET_RANGE`,
  validity polled, then read back), then select the 32-bit altsettings whose channel
  count matches the profile.
- **Capture.** IN packets are decoded into the capture ring. A failed packet is
  replaced by the nominal number of silent frames, so the timeline never shifts.
- **Playback.** OUT packet sizes follow the explicit feedback endpoint (16.16
  frames per microframe at high speed, 10.14 at full speed, with the same
  shift detection and sanity window as `snd-usb-audio`). Packets are filled from
  the playback ring; a short ring is padded with silence and counted as debt that
  later late frames repay, so capture and playback stay sample-aligned. Such an
  underrun means the cycle thread itself was late, so it counts as
  `pro_core_deadline_misses`, not as a hardware XRUN. Failed USB packets count as
  hardware XRUNs.
- **PRO cycle.** Each period the cycle thread waits until a whole capture period
  is available, hands it to the bridge, renders playback and queues it. The
  playback ring starts with `lead` silent frames:
  `lead = handoff + packets_per_urb * (nominal_packet + 1) + safety`.
  At 48 kHz / P64 / 1 ms handoff / 1 packet that is 61 frames (1.27 ms).
- **Restart.** If the OUT or IN URB queue drains (the xHCI ring then has an
  unmeasurable gap), the I/O thread cancels everything, bumps `generation`,
  counts a hardware XRUN in the affected direction, and restarts after the cycle
  thread acknowledges the new epoch. Blocks rendered for the old epoch are dropped.
  The same rebase runs when playback debt outlives any capture backlog that
  could repay it (about `lead + 2 * period` device frames): capture time itself
  slipped against playback. Debt that recurs every period counts too; only a
  debt-free window of that length clears it. Clean zero-length IN packets are
  skipped without padding and are not counted as XRUNs; failed or corrupt
  sub-frame packets are padded with nominal silence and counted as capture XRUNs.
  Client deadline misses never trigger this.
- **Exclusivity.** Interfaces are taken with `USBDEVFS_DISCONNECT_CLAIM`, which
  refuses (`EBUSY`) an interface another usbfs user holds. A second Sycon process
  fails to open instead of stealing the device from a running `sidealsad`. Every
  interface of the audio function (MIDI, unused streams) is held, so all of them
  come back with the ALSA card.
- **Clock.** A clock selector whose current input reports invalid (an unplugged
  external clock) is switched to the first valid input, like snd-usb-audio's
  autoclock. A rate outside the clock's `GET_RANGE` is a profile error: sidealsad
  exits 78 and systemd does not retry, so the card does not flap.

No allocation, lock, log or formatting happens in either steady-state loop. URB
buffers and rings are allocated before streaming starts.

## Operation

The ALSA card for the device disappears while `sidealsad` runs and returns when
it stops. `sidealsad` releases the interfaces on clean exit. A crash cannot run
that cleanup, so the systemd unit runs:

```
ExecStopPost=-/usr/local/bin/sidealsad --profile ... --socket ... --release-usb
```

`--release-usb` finds the profile's device and asks the kernel to rebind
`snd-usb-audio`. It does nothing for ALSA profiles or an absent device. Run it by
hand if the card stays missing:

```bash
sudo sidealsad --profile /etc/sidealsa/profiles/<profile>.toml --release-usb
```

Direct hardware check without the daemon:

```bash
sudo systemctl stop sidealsad.service
sudo target/release/sidealsa-hw-test --profile profiles/topping-e1x2-sycon.toml --periods 7500
sudo systemctl start sidealsad.service
```

## Limitations

- Asynchronous playback without an explicit feedback endpoint (implicit
  feedback) is rejected. Synchronous and adaptive endpoints work without feedback.
- Only S32_LE with 4-byte subslots, matching the rest of SideALSA.
- Only zero-lead PRO (`pro_latency_periods = 0`) is supported; non-zero values
  and `startup_loopback` calibration are rejected.
- The device's ALSA mixer controls are unavailable while streaming.
- `sidealsa-setup` cannot discover the device while `sidealsad` holds it;
  stop the service first.
- `sidealsa-direct-loopback-test` stays ALSA-only.
- Client-reported latency is the static profile value; USB, firmware and
  converter latency are not included.
- After a PRO client stops, up to `lead + period` frames it already rendered
  still play.
- A failed IN packet is replaced by the nominal frame count. With a device that
  varies its packet sizes, each such error can shift capture against playback by
  about one frame. Slips toward lower latency end in the rebase; growth toward
  higher latency is not detected or corrected.
- Capture ring overflow (cycle thread stalled for more than ~0.5 s) stops the
  engine instead of resynchronising.
- No in-process hotplug: unplugging the device stops `sidealsad` with a
  retryable exit code; the systemd unit (`StartLimitIntervalSec=0`,
  `RestartSec=2`) retries until the device is plugged back in, and
  `sidealsa-reconnect` refreshes PipeWire links.
