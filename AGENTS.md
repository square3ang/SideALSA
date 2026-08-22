Project: SideALSA
Language: Rust
Initial reference device: Topping E1x2 OTG
Long-term goal: generic support for professional USB audio interfaces.

# 1. Project goal

Build a userspace professional-audio layer on top of ALSA, inspired by the architecture of professional Windows audio-interface drivers.

SideALSA must:

- Be the sole owner of the physical ALSA hardware device.
- Keep the hardware audio timeline running independently from client failures.
- Provide two logically separate audio paths:

  1. PRO
     - Exclusive: only one PRO client may own it at a time.
     - Intended for DAWs, JACK backends, and SideALSA ASIO.
     - Very small periods such as 32/64 frames.
     - Minimal buffering.
     - Expose the complete physical multichannel interface.

  2. SHARED
     - Intended for PipeWire / desktop audio.
     - Buffered independently from PRO.
     - Client scheduling problems must never stall PRO or the hardware.
     - Expose configurable logical split devices such as Line 1, Line 2, Mic 1, etc.

- Support device profiles and configurable channel splitting.
- Eventually include a Wine ASIO frontend.
- Be generic enough that adding another standard ALSA/UAC2 interface mostly requires a new profile.

Do NOT build a general audio graph or JACK replacement.


# 2. Core architecture

Target architecture:

                       Physical ALSA device
                              hw:X,Y
                                │
                         ┌──────▼──────┐
                         │ sidealsad   │
                         │            │
                         │ HW RT loop │
                         │ HW clock   │
                         │ xrun logic │
                         └───┬────┬───┘
                             │    │
                    exclusive│    │buffered
                             │    │
                          PRO     SHARED
                             │    │
                   ┌─────────┘    └─────────┐
                   │                        │
              SideALSA client          ALSA ioplug
                   │                        │
              SideALSA ASIO             PipeWire
              Linux DAW/JACK            Desktop apps


Important invariant:

    client deadline miss != hardware XRUN

The hardware loop must never stop just because a PRO or SHARED client missed a deadline.


# 3. Rust workspace

Create:

sidealsa/
├── Cargo.toml
├── crates/
│   ├── sidealsa-core/
│   │   └── hardware RT engine, routing, timeline, xrun logic
│   ├── sidealsa-config/
│   │   └── TOML profile/config parser + validation
│   ├── sidealsa-protocol/
│   │   └── daemon/client protocol definitions
│   ├── sidealsa-client/
│   │   └── reusable client library
│   ├── sidealsa-daemon/
│   │   └── sidealsad executable
│   └── sidealsa-cli/
│       └── diagnostics/test client
│
├── profiles/
│   └── topping-e1x2.toml
│
└── docs/

Do NOT create the ALSA ioplug or ASIO frontend yet.


# 4. RT programming rules

The RT hardware thread must obey:

- no allocation
- no deallocation
- no filesystem I/O
- no println!/eprintln!
- no tracing/log formatting
- no Mutex/RwLock
- no potentially blocking IPC
- no dynamic Vec growth
- no configuration parsing

Allowed:

- preallocated buffers
- atomics
- fixed-size structures
- SPSC queues
- ALSA mmap/poll operations
- eventfd-like wakeups
- incrementing atomic statistics counters

All configuration and routing tables must be compiled before the RT loop starts.


# 5. Milestone 1 — Direct ALSA engine

Implement only sidealsa-core first.

Open the real physical ALSA playback and capture PCM directly.

Initial E1x2 target:

- 48 kHz
- playback: 8 channels
- capture: 10 channels
- S32_LE
- period_size = 64
- initial buffer_size = 64

Do not hardcode these values into the engine API.
The device profile should provide them.

Implement:

- playback/capture open
- hw_params
- sw_params
- duplex start/stop
- mmap or similarly direct ALSA transfer
- period wait
- playback/capture position tracking
- monotonic sample counter
- clean shutdown
- ALSA XRUN detection/recovery

First test mode:

capture hardware
        ↓
discard

zero-filled playback
        ↓
hardware

No daemon.
No IPC.
No client.
No PipeWire.

Acceptance:

- device continuously streams at Q64
- counters can be inspected from a non-RT thread
- no allocations occur in the RT cycle
- genuine ALSA XRUNs are counted separately


# 6. Hardware timeline

Create a SideALSA-owned hardware timeline.

Example concept:

struct HardwareTimeline {
    generation: AtomicU64,
    sample_position: AtomicU64,
    playback_xruns: AtomicU64,
    capture_xruns: AtomicU64,
}

generation increments only when the actual ALSA hardware stream must be restarted/rebased.

A client failure must NOT increment generation.


# 7. Milestone 2 — Device profiles and splitting

Implement configuration before adding IPC.

Example profile:

[device]
name = "Topping E1x2 OTG"
playback = "hw:OTG,0"
capture = "hw:OTG,0"
rate = 48000
period_size = 64
buffer_size = 64

[device.playback]
channels = 8
format = "S32_LE"

[device.capture]
channels = 10
format = "S32_LE"

[[ports.playback]]
id = "line1"
name = "Line 1"
channels = [0, 1]

[[ports.playback]]
id = "line2"
name = "Line 2"
channels = [2, 3]

[[ports.playback]]
id = "line3"
name = "Line 3"
channels = [4, 5]

[[ports.playback]]
id = "line4"
name = "Line 4"
channels = [6, 7]

[[ports.capture]]
id = "mic1"
name = "Mic 1"
channels = [0]

[[ports.capture]]
id = "mic2"
name = "Mic 2"
channels = [1]

[[ports.capture]]
id = "input34"
name = "Input 3/4"
channels = [2, 3]

etc.

Parsing pipeline:

TOML
 ↓
deserialize
 ↓
validate
 ↓
compile
 ↓
immutable routing table
 ↓
start RT engine

Validate:

- channel indices are in range
- IDs are unique
- names are valid
- direction is valid
- duplicate mappings are detected
- malformed profiles fail before hardware starts

The RT loop must never inspect strings or TOML.


# 8. Splitting semantics

Keep physical and logical layers separate.

Physical:

Playback = one 8-channel stream
Capture  = one 10-channel stream

Logical:

Line1 = playback channels 0,1
Line2 = playback channels 2,3
Mic1  = capture channel 0
etc.

Do NOT create one RT thread per split.

Do NOT create loopbacks per split.

A split must only be a channel mapping/view into the one physical hardware stream.

PRO initially exposes the complete raw multichannel device.

SHARED later uses logical split ports.


# 9. Milestone 3 — Local fake PRO client

Before writing a daemon, create an in-process fake PRO client.

Flow:

hardware period begins
       ↓
capture block ready
       ↓
PRO callback
       ↓
PRO produces playback block
       ↓
hardware playback

Test normal operation first.

Then deliberately make the PRO callback late.

For example occasionally sleep for 2 ms.

Required behavior:

PRO misses deadline
       ↓
SideALSA records pro_deadline_miss
       ↓
SideALSA substitutes a fallback playback block
       ↓
hardware continues
       ↓
next valid PRO block resumes normally

Do NOT restart ALSA.

Do NOT convert this into a HW XRUN.

Late/stale blocks must carry sequence numbers and must not shift the entire timeline by one period.

Initial fallback can simply be zero-fill.
More advanced concealment can come later.


# 10. Statistics

Implement at minimum:

hw_playback_xruns
hw_capture_xruns

pro_deadline_misses

shared_underruns
shared_overruns

timeline_resets

periods_processed

Expose them only from a non-RT diagnostics path.


# 11. Milestone 4 — sidealsad

Only after the previous milestones are stable, move the hardware engine into:

    sidealsad

sidealsad must be the ONLY process that opens hw:X,Y.

Create a client protocol.

Use:

- Unix domain socket for control/handshake
- shared memory for audio buffers
- lightweight notification primitive such as eventfd for cycle notifications

Do NOT send audio data through a Unix socket.


# 12. Client protocol

Conceptual operations:

HELLO
OPEN_PRO
OPEN_SHARED
CLOSE
START
STOP
GET_INFO
GET_STATS

PRO must be exclusive.

Pseudo policy:

if pro_owner == NONE:
    OPEN_PRO succeeds
else:
    OPEN_PRO returns BUSY

When the owning client disconnects or crashes:

    pro_owner = NONE

The daemon must remain running.


# 13. Shared-memory audio protocol

Use fixed preallocated buffers with sequence numbers.

Conceptually:

struct AudioSlot {
    sequence
    state
    audio [...]
}

The RT thread must never wait indefinitely for a client.

Each period has a deadline.

If data is unavailable:

PRO:
    count pro_deadline_miss
    use fallback

SHARED:
    count shared_underrun
    treat missing contribution as silence

Hardware continues in both cases.


# 14. Milestone 5 — SHARED path

Add the second client domain.

The intended model is:

       PRO
        │
        ├──────┐
        │      ▼
        │   final output
        │      ▲
        └──────┤
             SHARED

PRO should NOT be a general-purpose mixer.

SHARED may have multiple logical split endpoints.

Example:

shared Line1 → HW 0/1
shared Line2 → HW 2/3

A shared client failure must be isolated.

If PipeWire disappears for 50 ms:

- shared audio may drop
- PRO continues
- hardware continues
- hw_xruns should remain zero


# 15. Mixer constraints

The mixer must remain extremely simple.

Initial requirements only:

- same sample rate
- same hardware format
- channel mapping
- sum PRO + SHARED where both target the same physical channel
- clipping/saturation as appropriate

NO:

- resampling
- effects
- arbitrary routing graph
- plugin system
- dynamic DSP
- TotalMix-style matrix

Those are explicitly out of scope for MVP.


# 16. Milestone 6 — sidealsa-client crate

Extract all client-side protocol logic into a reusable Rust library.

Desired conceptual API:

SideAlsaClient::connect()
client.open_pro()
client.open_shared(port_id)

stream.start()
stream.wait_period()

stream.capture_buffer()
stream.playback_buffer()

stream.stop()

This API will later be reused by:

- ALSA ioplug
- SideALSA ASIO
- test utilities


# 17. Milestone 7 — ALSA ioplug

Do NOT begin until sidealsad + sidealsa-client are reliable.

Build:

    libasound_module_pcm_sidealsa.so

Prefer a very small C ABI/FFI layer and keep actual logic in sidealsa-client.

Expose logical ALSA PCMs such as:

    sidealsa_pro
    sidealsa_line1
    sidealsa_line2
    sidealsa_line3
    sidealsa_line4
    sidealsa_mic1
    sidealsa_mic2

Conceptually:

pcm.sidealsa_pro {
    type sidealsa
    mode pro
}

pcm.sidealsa_line1 {
    type sidealsa
    mode shared
    port "line1"
}

PRO exposes the complete raw interface.

Split shared PCMs expose logical profile ports.


# 18. Milestone 8 — PipeWire integration

Do not implement a custom PipeWire client initially.

Make PipeWire open the SideALSA shared ALSA PCMs.

Target:

Discord / browser
      ↓
PipeWire
      ↓
sidealsa shared PCM
      ↓
sidealsad

This is intentionally the buffered/non-critical path.

Test:

- PRO client at Q64
- Discord active
- Discord screen sharing active
- intentionally stress SHARED path

Expected:

pro_deadline_misses should remain near zero
hw_xruns should remain zero
shared misses must not propagate to PRO


# 19. Future milestone — SideALSA ASIO

Do NOT implement yet.

Eventually integrate the useful ASIO/Wine portions of PipeASIO into the SideALSA repository.

Target:

Wine application
      ↓
ASIO
      ↓
SideALSA ASIO frontend
      ↓
sidealsa-client
      ↓ shared memory
sidealsad

Do NOT route SideALSA ASIO through the ALSA ioplug.

It should use the SideALSA client protocol directly.

Reuse/reimplement as appropriate:

- ASIO COM interface
- createBuffers
- bufferSwitch
- timing/sample position
- Wine thread/TEB handling
- WoW64 support

The SideALSA ASIO frontend occupies the same exclusive PRO slot as a Linux native DAW.


# 20. Generic-device design requirement

Do not put Topping-specific logic in the core.

The E1x2 is only the first reference profile.

Core code should depend on concepts such as:

PhysicalDevice
HardwareConfig
CompiledPort
Direction
ProStream
SharedStream

not:

ToppingE1x2
Line1OfE1x2

Most normal UAC2 interfaces should eventually be portable through a profile.


# 21. MVP definition of done

The first useful SideALSA MVP is complete when all of these work:

1. sidealsad exclusively owns the E1x2 hardware.
2. 48 kHz / 64-frame period duplex operation works reliably.
3. A PRO client can access the complete 8-out / 10-in interface.
4. Only one PRO client can exist at once.
5. Config-defined logical channel splits work.
6. A SHARED logical client can operate simultaneously with PRO.
7. A deliberately late SHARED client never causes a PRO or HW XRUN.
8. A deliberately late PRO client causes a PRO deadline miss, not a hardware restart.
9. Real ALSA hardware XRUNs are recovered by sidealsad.
10. Statistics distinguish HW XRUN, PRO miss, and SHARED miss.
11. sidealsad cleanly releases the physical device when stopped.


# 22. Agent workflow

Implement ONE milestone at a time.

For every milestone:

1. implement the smallest working version
2. add unit/integration tests where practical
3. run cargo fmt
4. run cargo clippy
5. run cargo test
6. document current limitations
7. do not begin the next milestone until the current one works

Do not prematurely implement:

- ASIO
- PipeWire-specific code
- JACK server
- resampling
- GUI
- device hotplug
- advanced routing
- SIMD optimization
- CPU pinning
- realtime scheduler tweaking

First make the architecture correct and observable.

The FIRST task is only:

    Create the workspace and implement a minimal
    sidealsa-core direct-duplex ALSA test that can open the
    profile-selected physical device, run 48kHz / 64-frame periods,
    capture continuously, output silence, count real hardware XRUNs,
    and shut down cleanly.

Stop after that first milestone and report:
- files created
- architecture used
- how to build
- how to run
- current limitations
- measured/tested behavior
