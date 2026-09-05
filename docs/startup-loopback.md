# Digital Loopback Startup Normalization

An optional normalizer can align the E1x2's internal digital return from playback
channel 0 to capture channel 4 to **376 frames (7.833 ms at 48 kHz)** at startup. The user
confirmed this routing. USB-payload inspection found a bit-perfect delayed copy
of all 64 test impulses, with exact zero in every other valid captured sample.
This is not an analog DAC/cable/ADC roundtrip measurement.

**Not enabled by default:** startup/recovery repeatability passed, but a later
measurement on the same installed PID 154767/generation 0 found 407 frames
(8.479 ms), with all hardware XRUN/reset counters still zero. The live reference
was restored to the unnormalized P32/Q64/Q128 configuration rather than leaving
extra padding that could exceed the requested sub-8-ms latency after a shift.
This is a tested startup-alignment option, not a complete phase-stability fix.

Historical tests that called this channel pair "analog loopback" measured this
device loopback path; they do not establish analog converter latency. The raw
sample-ordinal measurements were independently reproduced from USB payloads.

## Configuration

```toml
[device.startup_loopback]
playback_channel = 0
capture_channel = 4
target_frames = 376
```

The table is optional and profile-specific. It requires direct linked zero-lead
PRO and a known digital return of the configured output. Other profiles do not
emit calibration signals unless they explicitly configure this table. Channel
bounds and the nonzero, at-most-100-ms target are validated before hardware
opens. The full profile fingerprint includes these settings. GUI timing edits
preserve the table; it is currently edited through TOML, not GUI controls.

## Operation

1. Start the linked hardware with its normal base silence prime.
2. Before admitting clients, continuously read and write complete logical blocks
   while sending three distinct quiet markers on the configured output.
3. Require all three markers to return exactly at the same measured delay.
4. Append `target - measured` actual playback silence frames. Do not subtract
   from measurements, relabel sequences, or discard normal client samples.
5. Verify the resulting delay with three different markers before publishing
   hardware readiness and establishing the client timeline origin.

The markers are 24-bit-exact S32_LE values, peaking below -98 dBFS. This is not a
full-scale impulse or an ongoing test tone. Exact marker matching detects the
reference mixer's 1 dB gain steps even after 24-bit requantization. It is a
qualification of these markers, not a proof of bit-perfect transfer for every
possible input signal or arbitrarily small gain change.

Normalization runs once per actual hardware start, including genuine ALSA
recovery. It does not run on PRO start/stop/reconnect or deadline misses. The
RT implementation uses fixed state, mmap, bounded polling, a shared two-second
deadline, and no allocation, logging, locking, or client callbacks. Startup
capture is discarded; during recovery hardware continues calibration streaming
before the new generation is exposed. The reference normally takes about half
a second to qualify. A real XRUN still increments generation exactly once.

Q64 client blocks, P32 transport, B256 capacity, and the maximum 1 ms client
handoff are retained. With a Q128 base prime, padding is limited to 64 frames,
preserving one complete Q64 writable block. Thus this reference can normalize
an unpadded delay of 312-376 frames. Positive partial mmap progress is rechecked
immediately instead of unnecessarily waiting at a ring wrap.

## Failure Policy

Missing/ambiguous markers, an unstable return, an already-too-large delay,
insufficient padding capacity, timeout, or a failed transfer reject startup or
recovery. The daemon exits with status 78 and the shipped systemd unit uses
`RestartPreventExitStatus=78`. It must not retry hardware starts until one
happens to meet the target. Correct the profile/routing before starting again,
or remove the table to run without normalization. An existing installation must
update the daemon, validation helper, profile, and unit together.

## Verification

The initial direct prototype completed 16 hardware starts/restarts at exactly
376 frames, adding 11-47 actual silence frames to differing initial delays.
The production path then completed six new daemon starts and three confirmed
ALSA XRUN recoveries, with 24/24 independent native test pulses at 376 frames in
every case. After strengthening the quiet markers and failure classification,
the same six-start/three-recovery test passed again. These tests deliberately
caused hardware faults only between measurement legs; subsequent normal legs
had no new PRO miss, hardware XRUN, or timeline reset.

Negative hardware tests confirmed exit 78 and zero automatic restarts for an
incorrect capture route (ambiguous markers) and an unattainable one-frame
target. They did not establish the missing-marker failure branch on hardware;
constant-input rejection and bounded waiting are covered by unit tests.

After installation, four consecutive service restarts each returned 47/47
independent native pulses at exactly 376 frames, with no PRO miss or hardware
XRUN. On the last start (PID 154767), a 45000-period native run with 24 CPU
workers and simultaneous SHARED playback returned all 2810 pulses at 376 frames.
Two 20-second ASIO legs under 24 host workers, 512 MiB memory traffic, 350 us
callback work, and SHARED playback returned 250+252 pulses at the same target.
PRO, SHARED, hardware-XRUN, and reset counters remained zero during normal load.
Wine printed a sendmsg warning after that probe's PASS; a separate two-leg
five-second stress probe verified 129 further pulses and explicit exit status 0.
A 1500-period intentional 2 ms PRO-delay test produced 89 expected client misses
and two lost pulses, but all 22 detected pulses remained at 376 frames with no
hardware XRUN, generation change, or reset. This is not a normal-load XRUN.
Host-local logs are in `/tmp/opencode/normalized-installed.k0UPa892`.

## Limits

This normalizes the configured device-loopback path, not every independent ADC,
DAC, or firmware route. It adds delay to faster starts rather than speeding up
slower starts. It does not repair the separately observed xHCI startup packet
scheduling anomaly or compensate later device-side phase changes during normal
streaming. Runtime phase stability and normal-load hardware-XRUN counts remain
separate acceptance checks. ASIO still reports its 64-frame callback buffers,
not this complete device-loopback RTT.
