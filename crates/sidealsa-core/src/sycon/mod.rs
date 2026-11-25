//! Sycon mode: a direct USB Audio Class 2 backend over Linux usbfs.
//!
//! sidealsad detaches snd-usb-audio from the audio function, programs the
//! sampling clock itself and streams isochronous URBs. One USB I/O thread owns
//! every URB. The PRO cycle thread trades whole periods with it through two
//! SPSC frame rings, so a late client never delays USB servicing.
//!
//! Timeline rules:
//! - Capture frames enter the ring in device order; a failed IN packet is
//!   replaced by nominal-length silence so later frames keep their position.
//! - Playback frames leave the ring at device rate (explicit feedback). When
//!   the ring runs dry the gap is sent as silence and the same number of late
//!   frames is dropped once they arrive, so latency never drifts.
//! - Only an emptied URB queue (the host controller then restarts the stream
//!   at an unknown frame) restarts both streams and bumps `generation`.

mod clock;
mod ring;
mod uac2;
mod usbfs;

use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use sidealsa_config::{HardwareBackend, SyconConfig};

use crate::engine::{
    NoopPro, RealtimeGuard, ensure_supported_format, monotonic_nanos,
    routing_error_to_engine_error, sample_count, set_current_realtime,
};
use crate::{
    EngineError, HardwareConfig, HardwareStats, HardwareTimeline, ProCaptureSink,
    ProPlaybackSource, Profile, RoutingTable, StreamDirection,
};
use clock::PacketClock;
use ring::FrameRing;
use uac2::{AudioFunction, StreamFormat};
use usbfs::{IsoUrb, MAX_ISO_PACKETS, UsbDevice};

/// S32_LE: four bytes per sample in a four-byte UAC subslot.
const SUBSLOT_BYTES: u8 = 4;
const FEEDBACK_URBS: usize = 3;
const IO_POLL_MS: i32 = 10;
/// No completion at all for this long means the device stopped streaming.
const IO_STALL_MS: i32 = 1000;
const CYCLE_POLL_MS: i32 = 5;
const RESTART_ACK_TIMEOUT: Duration = Duration::from_millis(250);
const DRAIN_TIMEOUT_NANOS: u64 = 500_000_000;

pub struct SyconEngine {
    config: HardwareConfig,
    timeline: Arc<HardwareTimeline>,
    routing: RoutingTable,
    layout: Layout,
    started: bool,
    device: UsbDevice,
}

#[derive(Clone, Copy, Debug)]
struct Layout {
    playback: StreamFormat,
    capture: StreamFormat,
    high_speed: bool,
    packets_per_urb: usize,
    playback_urbs: usize,
    capture_urbs: usize,
    /// Silence queued ahead of the first block; the steady ring level at cycle start.
    lead_frames: u64,
    /// Ring frames that must remain when a block becomes due (one OUT URB plus safety).
    reserve_frames: u64,
    handoff_nanos: u64,
}

impl SyconEngine {
    pub fn open(profile: Profile) -> Result<Self, EngineError> {
        profile
            .validate()
            .map_err(|error| EngineError::InvalidConfig(error.to_string()))?;
        let routing = RoutingTable::compile(&profile).map_err(routing_error_to_engine_error)?;
        let config = profile.device;
        if config.backend != HardwareBackend::Sycon {
            return Err(EngineError::InvalidConfig(
                "profile does not select backend = \"sycon\"".into(),
            ));
        }
        let sycon = config.sycon.clone().ok_or_else(|| {
            EngineError::InvalidConfig("backend = \"sycon\" requires [device.sycon]".into())
        })?;
        if config.pro_latency_periods != 0 {
            return Err(EngineError::InvalidConfig(
                "sycon backend requires pro_latency_periods = 0".into(),
            ));
        }
        ensure_supported_format(&config.playback.format, StreamDirection::Playback)?;
        ensure_supported_format(&config.capture.format, StreamDirection::Capture)?;
        sample_count(config.period_size, config.playback.channels)?;
        sample_count(config.period_size, config.capture.channels)?;

        let located = uac2::locate_device(sycon.vendor_id, sycon.product_id, sycon.port.as_deref())
            .map_err(usb_error("locate device"))?;
        // Unconfigured (still enumerating, or not yet authorized) is a device
        // state worth retrying, not a profile error.
        if located.active_configuration == 0 {
            return Err(usb_error("locate device")(io::Error::other(
                "device has no active configuration",
            )));
        }
        let mut device = UsbDevice::open(&located.node).map_err(usb_error("open usbfs node"))?;
        let dump = device
            .read_descriptors()
            .map_err(usb_error("read descriptors"))?;
        if dump.is_empty() {
            return Err(usb_error("read descriptors")(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "device returned empty descriptor dump",
            )));
        }
        let function = uac2::active_configuration(&dump, located.active_configuration)
            .and_then(AudioFunction::parse)
            .map_err(EngineError::InvalidConfig)?;
        let playback = *function
            .find_stream(false, config.playback.channels, SUBSLOT_BYTES)
            .ok_or_else(|| {
                EngineError::InvalidConfig(format!(
                    "device has no UAC2 playback altsetting with {} channels of 32-bit samples",
                    config.playback.channels
                ))
            })?;
        let capture = *function
            .find_stream(true, config.capture.channels, SUBSLOT_BYTES)
            .ok_or_else(|| {
                EngineError::InvalidConfig(format!(
                    "device has no UAC2 capture altsetting with {} channels of 32-bit samples",
                    config.capture.channels
                ))
            })?;
        if playback.data.is_async() && playback.feedback.is_none() {
            return Err(EngineError::InvalidConfig(
                "asynchronous playback without a feedback endpoint (implicit feedback) is not supported yet".into(),
            ));
        }
        let control = function
            .control_interface
            .ok_or_else(|| EngineError::InvalidConfig("no audio control interface".into()))?;

        // The control interface first: releasing it tears the ALSA card down.
        for interface in [control, playback.interface, capture.interface] {
            device
                .detach_and_claim(interface)
                .map_err(usb_error("detach kernel driver and claim interface"))?;
        }
        // Hold the rest of the function (MIDI, unused streams) too, so the
        // control probe on release finds them free and binds them again.
        for &interface in &function.interfaces {
            if ![control, playback.interface, capture.interface].contains(&interface) {
                // Not needed for streaming: only take it from snd-usb-audio;
                // HID or vendor drivers inside the association keep theirs.
                let _ = device.claim_from(interface, b"snd-usb-audio");
            }
        }
        for interface in [capture.interface, playback.interface] {
            set_interface(&device, interface, 0)?;
        }
        let playback_clock = uac2::resolve_clock_source(&device, &function, playback.terminal_link)
            .map_err(EngineError::InvalidConfig)?;
        let capture_clock = uac2::resolve_clock_source(&device, &function, capture.terminal_link)
            .map_err(EngineError::InvalidConfig)?;
        let mut clocks = vec![playback_clock];
        if capture_clock.0 != playback_clock.0 {
            clocks.push(capture_clock);
        }
        for (clock, controls) in clocks {
            uac2::check_rate_supported(&device, control, clock, config.rate)
                .map_err(EngineError::InvalidConfig)?;
            uac2::set_sample_rate(&device, control, clock, controls, config.rate).map_err(
                |message| EngineError::Usb {
                    operation: "set sample rate",
                    source: io::Error::other(message),
                },
            )?;
        }

        let layout = Layout::new(&config, &sycon, playback, capture, located.high_speed);
        eprintln!(
            "sycon: {:04x}:{:04x} at {} ({} speed): playback if{}/alt{} ep {:#04x} {}-bit (feedback {}), capture if{}/alt{} ep {:#04x}, clock {} at {} Hz, {} packets/URB, {}+{} URBs, playback lead {} frames",
            sycon.vendor_id,
            sycon.product_id,
            located.port,
            if located.high_speed { "high" } else { "full" },
            playback.interface,
            playback.altsetting,
            playback.data.address,
            playback.bit_resolution,
            playback
                .feedback
                .map_or("none".to_string(), |endpoint| format!(
                    "{:#04x}",
                    endpoint.address
                )),
            capture.interface,
            capture.altsetting,
            capture.data.address,
            playback_clock.0,
            config.rate,
            layout.packets_per_urb,
            layout.playback_urbs,
            layout.capture_urbs,
            layout.lead_frames,
        );
        Ok(Self {
            config,
            timeline: Arc::new(HardwareTimeline::default()),
            routing,
            layout,
            started: false,
            device,
        })
    }

    pub fn config(&self) -> &HardwareConfig {
        &self.config
    }

    pub fn timeline(&self) -> &HardwareTimeline {
        &self.timeline
    }

    pub fn timeline_handle(&self) -> Arc<HardwareTimeline> {
        Arc::clone(&self.timeline)
    }

    pub fn routing(&self) -> &RoutingTable {
        &self.routing
    }

    pub fn stats(&self) -> HardwareStats {
        self.timeline.snapshot()
    }

    /// Selects the streaming altsettings; URBs start with the run loop.
    pub fn start(&mut self) -> Result<(), EngineError> {
        if self.started {
            return Ok(());
        }
        set_interface(
            &self.device,
            self.layout.capture.interface,
            self.layout.capture.altsetting,
        )?;
        set_interface(
            &self.device,
            self.layout.playback.interface,
            self.layout.playback.altsetting,
        )?;
        self.started = true;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), EngineError> {
        if !self.started {
            return Ok(());
        }
        self.started = false;
        let playback = set_interface(&self.device, self.layout.playback.interface, 0);
        let capture = set_interface(&self.device, self.layout.capture.interface, 0);
        playback.and(capture)
    }

    pub fn run(&mut self, stop: &AtomicBool, max_periods: Option<u64>) -> Result<(), EngineError> {
        self.run_inner(stop, max_periods, NoopPro, NoopPro, None)
    }

    pub fn run_pro<C: ProCaptureSink, P: ProPlaybackSource>(
        &mut self,
        stop: &AtomicBool,
        max_periods: Option<u64>,
        capture_sink: C,
        playback_source: P,
    ) -> Result<(), EngineError> {
        self.run_inner(stop, max_periods, capture_sink, playback_source, None)
    }

    pub fn run_pro_with_ready<C: ProCaptureSink, P: ProPlaybackSource>(
        &mut self,
        stop: &AtomicBool,
        max_periods: Option<u64>,
        capture_sink: C,
        playback_source: P,
        hardware_ready: &AtomicBool,
    ) -> Result<(), EngineError> {
        self.run_inner(
            stop,
            max_periods,
            capture_sink,
            playback_source,
            Some(hardware_ready),
        )
    }

    fn run_inner<C: ProCaptureSink, P: ProPlaybackSource>(
        &mut self,
        stop: &AtomicBool,
        max_periods: Option<u64>,
        mut capture_sink: C,
        mut playback_source: P,
        hardware_ready: Option<&AtomicBool>,
    ) -> Result<(), EngineError> {
        self.start()?;
        let layout = self.layout;
        let config = &self.config;
        let period = u64::from(config.period_size);
        let playback_channels = config.playback.channels as usize;
        let capture_channels = config.capture.channels as usize;
        // Everything the RT threads touch is allocated here, before they start.
        let capture_frames = (config.rate as usize / 2).max(period as usize * 16);
        let playback_capacity = capture_frames + (layout.lead_frames + period * 2) as usize;
        let shared = Shared {
            capture: FrameRing::new(capture_channels, capture_frames),
            // A cycle catching up on a full capture ring must never overflow
            // playback: dropped frames would leave debt nothing can repay.
            playback: FrameRing::new(playback_channels, playback_capacity),
            armed: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            ack: AtomicU64::new(0),
            out_inflight_frames: AtomicU64::new(0),
            resync_requested: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            wake: EventFd::new().map_err(usb_error("create eventfd"))?,
        };
        let mut io = IoLoop::new(&self.device, &shared, &self.timeline, config, layout)?;
        let mut capture_scratch = vec![0_i32; period as usize * capture_channels];
        let mut playback_scratch = vec![0_i32; period as usize * playback_channels];
        let cycle = CycleConfig {
            period,
            rate: config.rate,
            playback_channels,
            playback_capacity: playback_capacity as u64,
            lead_frames: layout.lead_frames,
            reserve_frames: layout.reserve_frames,
            handoff_nanos: layout.handoff_nanos,
        };
        let io_priority = config.realtime.then_some(config.realtime_priority as i32);
        let cycle_priority = config
            .realtime
            .then(|| (config.realtime_priority as i32 - 1).max(1));
        let timeline = &*self.timeline;

        thread::scope(|scope| {
            let shared = &shared;
            let io_handle = thread::Builder::new()
                .name("sycon-usb".into())
                .spawn_scoped(scope, move || {
                    let _exit = SignalOnExit {
                        shared,
                        flag: &shared.failed,
                    };
                    if let Some(priority) = io_priority {
                        set_current_realtime(priority, "set Sycon USB thread SCHED_FIFO")?;
                    }
                    io.run(stop)
                })
                .map_err(|source| EngineError::WorkerSpawn {
                    worker: "sycon-usb",
                    source,
                })?;
            let cycle_result = {
                let _exit = SignalOnExit {
                    shared,
                    flag: &shared.shutdown,
                };
                cycle_priority
                    .map(|priority| RealtimeGuard::enter(priority, "set Sycon cycle SCHED_FIFO"))
                    .transpose()
                    .map(|_realtime| {
                        run_cycles(
                            shared,
                            timeline,
                            cycle,
                            stop,
                            max_periods,
                            hardware_ready,
                            &mut capture_sink,
                            &mut playback_source,
                            &mut capture_scratch,
                            &mut playback_scratch,
                        );
                    })
            };
            let io_result = io_handle.join().map_err(|_| EngineError::WorkerPanic)?;
            io_result.and(cycle_result)
        })
    }
}

impl Drop for SyconEngine {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl Layout {
    fn new(
        config: &HardwareConfig,
        sycon: &SyconConfig,
        playback: StreamFormat,
        capture: StreamFormat,
        high_speed: bool,
    ) -> Self {
        let rate = u64::from(config.rate);
        let units_per_second = if high_speed { 8000 } else { 1000 };
        let nominal_packet_frames =
            (rate << playback.data.interval_log2).div_ceil(units_per_second);
        let packets_per_urb = sycon.packets_per_urb as usize;
        let urb_frames = packets_per_urb as u64 * (nominal_packet_frames + 1);
        let safety_frames = sycon.safety_frames.map_or(nominal_packet_frames, u64::from);
        let handoff_nanos = config.pro_handoff_nanos();
        let handoff_frames = (handoff_nanos * rate).div_ceil(1_000_000_000);
        let reserve_frames = urb_frames + safety_frames;
        Self {
            playback,
            capture,
            high_speed,
            packets_per_urb,
            playback_urbs: sycon.playback_urbs as usize,
            capture_urbs: sycon.capture_urbs as usize,
            lead_frames: handoff_frames + reserve_frames,
            reserve_frames,
            handoff_nanos,
        }
    }
}

/// Locates a Sycon profile's device and asks the kernel to rebind its audio
/// interfaces. Covers a sidealsad that was killed before its own cleanup ran.
/// Returns false when the profile is not Sycon or the device is absent.
pub fn release_sycon_device(profile: &Profile) -> Result<bool, EngineError> {
    let Some(sycon) = profile
        .device
        .sycon
        .as_ref()
        .filter(|_| profile.device.backend == HardwareBackend::Sycon)
    else {
        return Ok(false);
    };
    let located =
        match uac2::locate_device(sycon.vendor_id, sycon.product_id, sycon.port.as_deref()) {
            Ok(located) if located.active_configuration == 0 => return Ok(false),
            Ok(located) => located,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(usb_error("locate device")(error)),
        };
    let mut device = match UsbDevice::open(&located.node) {
        Ok(device) => device,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ENODEV) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(usb_error("open usbfs node")(error)),
    };
    let dump = device
        .read_descriptors()
        .map_err(usb_error("read descriptors"))?;
    if dump.is_empty() {
        return Ok(false);
    }
    let function = uac2::active_configuration(&dump, located.active_configuration)
        .and_then(AudioFunction::parse)
        .map_err(EngineError::InvalidConfig)?;
    let mut interfaces: Vec<u8> = function
        .streams
        .iter()
        .map(|stream| stream.interface)
        .chain(function.interfaces.iter().copied())
        .filter(|&interface| Some(interface) != function.control_interface)
        .collect();
    interfaces.sort_unstable();
    interfaces.dedup();
    // Control interface first: its probe claims the streaming interfaces.
    interfaces.splice(0..0, function.control_interface);
    for interface in interfaces {
        device
            .connect_driver(interface)
            .map_err(usb_error("reconnect kernel driver"))?;
    }
    Ok(true)
}

fn set_interface(device: &UsbDevice, interface: u8, altsetting: u8) -> Result<(), EngineError> {
    // snd-usb-audio retries EPROTO the same way.
    let mut delay = Duration::from_millis(5);
    for _ in 0..5 {
        match device.set_interface(interface, altsetting) {
            Err(error) if error.raw_os_error() == Some(libc::EPROTO) => {
                thread::sleep(delay);
                delay *= 2;
            }
            result => return result.map_err(usb_error("set interface")),
        }
    }
    device
        .set_interface(interface, altsetting)
        .map_err(usb_error("set interface"))
}

fn usb_error(operation: &'static str) -> impl Fn(io::Error) -> EngineError {
    move |source| EngineError::Usb { operation, source }
}

struct Shared {
    capture: FrameRing,
    playback: FrameRing,
    /// Set by the cycle once lead silence and the first block are queued.
    armed: AtomicBool,
    /// Bumped by the I/O thread for every stream restart.
    epoch: AtomicU64,
    /// The cycle's acknowledgement that it dropped everything from older epochs.
    ack: AtomicU64,
    out_inflight_frames: AtomicU64,
    resync_requested: AtomicBool,
    shutdown: AtomicBool,
    failed: AtomicBool,
    wake: EventFd,
}

/// Raises `flag` and wakes the cycle when a thread leaves, even by panic.
struct SignalOnExit<'a> {
    shared: &'a Shared,
    flag: &'a AtomicBool,
}

impl Drop for SignalOnExit<'_> {
    fn drop(&mut self) {
        self.flag.store(true, Ordering::Release);
        self.shared.wake.signal();
    }
}

struct EventFd(OwnedFd);

impl EventFd {
    fn new() -> io::Result<Self> {
        // SAFETY: plain syscall; the returned descriptor is owned below.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor nobody else owns.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn signal(&self) {
        let one = 1_u64;
        // SAFETY: writes eight bytes from a live u64.
        unsafe {
            let _ = libc::write(self.0.as_raw_fd(), (&raw const one).cast(), 8);
        }
    }

    fn wait(&self, timeout_ms: i32) {
        let mut descriptor = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd; the read drains at most eight bytes.
        unsafe {
            if libc::poll(&mut descriptor, 1, timeout_ms) > 0 {
                let mut value = 0_u64;
                let _ = libc::read(self.0.as_raw_fd(), (&raw mut value).cast(), 8);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct CycleConfig {
    period: u64,
    rate: u32,
    playback_channels: usize,
    playback_capacity: u64,
    lead_frames: u64,
    reserve_frames: u64,
    handoff_nanos: u64,
}

/// The PRO cycle: one period per iteration, in the zero-lead bridge order of
/// the direct ALSA engine. Never touches USB.
#[allow(clippy::too_many_arguments)]
fn run_cycles<C: ProCaptureSink, P: ProPlaybackSource>(
    shared: &Shared,
    timeline: &HardwareTimeline,
    config: CycleConfig,
    stop: &AtomicBool,
    max_periods: Option<u64>,
    hardware_ready: Option<&AtomicBool>,
    capture_sink: &mut C,
    playback_source: &mut P,
    capture: &mut [i32],
    playback: &mut [i32],
) {
    let period = config.period;
    let mut hw_sequence = 0_u64;
    let mut periods = 0_u64;
    let mut capture_position = 0_u64;
    let mut playback_position = 0_u64;
    // Shared starts at epoch 0; the I/O thread may already have restarted.
    let mut epoch_seen = 0_u64;
    let mut arm_pending = true;
    loop {
        let epoch = shared.epoch.load(Ordering::Acquire);
        if epoch != epoch_seen {
            // The I/O thread restarted both streams: drop stale capture and
            // queue fresh lead silence before the next block.
            shared.capture.skip(u64::MAX);
            epoch_seen = epoch;
            arm_pending = true;
            shared.ack.store(epoch, Ordering::Release);
        }
        if stop.load(Ordering::Relaxed)
            || shared.failed.load(Ordering::Acquire)
            || max_periods.is_some_and(|limit| periods >= limit)
        {
            return;
        }
        if shared.capture.len() < period {
            shared.wake.wait(CYCLE_POLL_MS);
            continue;
        }
        let observed = monotonic_nanos();
        shared.capture.pop(capture);
        let capture_backlog = shared.capture.len();
        let queued = if arm_pending {
            config.lead_frames.saturating_sub(capture_backlog)
        } else {
            shared.playback.len()
        };
        let in_flight = shared.out_inflight_frames.load(Ordering::Relaxed);

        capture_position = capture_position.wrapping_add(period);
        timeline.update_capture_position(capture_position);
        timeline.record_pro_capture_read(hw_sequence, observed);
        playback_source.prepare_playback(hw_sequence);
        capture_sink.process_capture_for_playback(hw_sequence, hw_sequence, capture);

        let budget_frames = queued.saturating_sub(config.reserve_frames);
        let cutoff = observed.saturating_add(
            config
                .handoff_nanos
                .min(budget_frames * 1_000_000_000 / u64::from(config.rate)),
        );
        let wait = cutoff.saturating_sub(monotonic_nanos());
        timeline.record_pro_wait_budget(wait);
        if wait == 0 {
            playback_source.mark_playback_budget_exhausted(hw_sequence);
        }
        playback_source.wait_for_playback_before(hw_sequence, cutoff);

        playback_source.begin_playback_commit();
        playback_source.process_playback_before(hw_sequence, cutoff, playback);
        playback_source.prepare_playback_mix(hw_sequence);
        playback_source.commit_playback(hw_sequence, playback);
        let write_started = monotonic_nanos();
        // A restart while this block was rendered makes it stale.
        if shared.epoch.load(Ordering::Acquire) == epoch_seen {
            if arm_pending {
                // Capture already queued plus time spent rendering both count against lead.
                let elapsed =
                    write_started.saturating_sub(observed) * u64::from(config.rate) / 1_000_000_000;
                let total = capture_backlog.saturating_add(elapsed);
                if total <= config.lead_frames {
                    let silence = config.lead_frames - total;
                    let free = config
                        .playback_capacity
                        .saturating_sub(shared.playback.len());
                    if free >= silence + period {
                        shared.playback.push_silence(silence);
                        shared.playback.push(playback);
                        shared.armed.store(true, Ordering::Release);
                        arm_pending = false;
                    } else {
                        shared.resync_requested.store(true, Ordering::Release);
                    }
                } else {
                    let excess = total - config.lead_frames;
                    if excess < period {
                        let keep = period - excess;
                        let free = config
                            .playback_capacity
                            .saturating_sub(shared.playback.len());
                        if free >= keep {
                            let start = excess as usize * config.playback_channels;
                            shared.playback.push(&playback[start..]);
                            shared.armed.store(true, Ordering::Release);
                            arm_pending = false;
                        } else {
                            shared.resync_requested.store(true, Ordering::Release);
                        }
                    }
                    // Otherwise this whole block is already past lead: drop it and stay unarmed.
                }
            } else {
                let free = config
                    .playback_capacity
                    .saturating_sub(shared.playback.len());
                if free >= period {
                    shared.playback.push(playback);
                } else {
                    shared.resync_requested.store(true, Ordering::Release);
                }
            }
        }
        let write_finished = monotonic_nanos();
        timeline.record_playback_write(write_finished.saturating_sub(write_started));
        playback_source.end_playback_commit();
        playback_position = playback_position.wrapping_add(period);
        timeline.update_playback_position(playback_position);
        timeline.record_capture_to_playback_write_duration(observed, write_finished);
        capture_sink.process_deferred_capture(hw_sequence, capture);

        timeline.update_pcm_delay(
            StreamDirection::Playback,
            (queued + in_flight) as i64,
            period,
        );
        timeline.update_pcm_delay(
            StreamDirection::Capture,
            shared.capture.len() as i64,
            period,
        );
        hw_sequence = hw_sequence.wrapping_add(1);
        periods = periods.wrapping_add(1);
        timeline.processed_frames(period, 1);
        if let Some(ready) = hardware_ready
            && !ready.load(Ordering::Relaxed)
            && !stop.load(Ordering::Relaxed)
        {
            ready.store(true, Ordering::Release);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PoolKind {
    Capture,
    Playback,
    Feedback,
}

#[repr(C)]
struct UrbSlot {
    /// First field: the reaped URB pointer is also the slot pointer.
    urb: IsoUrb,
    frames: u32,
    in_flight: bool,
}

/// URBs and their data stay at fixed addresses from creation to drop; usbfs
/// writes results into both during REAPURB.
struct UrbPool {
    slots: *mut UrbSlot,
    count: usize,
    data: *mut u8,
    data_len: usize,
    urb_bytes: usize,
    packets: usize,
    packet_bytes: u32,
    in_flight: usize,
}

impl UrbPool {
    fn new(kind: PoolKind, endpoint: u8, count: usize, packets: usize, packet_bytes: u32) -> Self {
        let urb_bytes = packets * packet_bytes as usize;
        let data_len = urb_bytes * count;
        let data = Box::into_raw(vec![0_u8; data_len].into_boxed_slice()).cast::<u8>();
        let tag_base = (kind as usize) << 16;
        let slots: Box<[UrbSlot]> = (0..count)
            .map(|index| UrbSlot {
                // SAFETY: `index * urb_bytes` stays inside the data allocation.
                urb: IsoUrb::new(
                    endpoint,
                    unsafe { data.add(index * urb_bytes) },
                    tag_base | index,
                ),
                frames: 0,
                in_flight: false,
            })
            .collect();
        Self {
            slots: Box::into_raw(slots).cast::<UrbSlot>(),
            count,
            data,
            data_len,
            urb_bytes,
            packets,
            packet_bytes,
            in_flight: 0,
        }
    }

    fn slot(&self, index: usize) -> *mut UrbSlot {
        debug_assert!(index < self.count);
        // SAFETY: index is below `count`.
        unsafe { self.slots.add(index) }
    }

    /// Byte range of one URB's packets.
    #[allow(clippy::mut_from_ref)]
    fn data(&self, index: usize) -> &mut [u8] {
        // SAFETY: disjoint per index, only used while the URB is not in flight.
        unsafe {
            std::slice::from_raw_parts_mut(self.data.add(index * self.urb_bytes), self.urb_bytes)
        }
    }
}

impl Drop for UrbPool {
    fn drop(&mut self) {
        if self.in_flight > 0 {
            // Kernel usbfs still holds pointers into slots/data on the open fd;
            // leak the pool instead of freeing live DMA/reap targets.
            return;
        }
        // SAFETY: both pointers came from Box::into_raw with these lengths,
        // and no URB in this pool is in flight.
        unsafe {
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(
                self.slots, self.count,
            )));
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(
                self.data,
                self.data_len,
            )));
        }
    }
}

struct IoLoop<'a> {
    device: &'a UsbDevice,
    shared: &'a Shared,
    timeline: &'a HardwareTimeline,
    capture: UrbPool,
    playback: UrbPool,
    feedback: Option<UrbPool>,
    out_clock: PacketClock,
    /// Nominal-rate stand-in for frames of IN packets that failed.
    capture_clock: PacketClock,
    capture_frame_bytes: usize,
    playback_frame_bytes: usize,
    period: u64,
    /// Playback frames sent as silence while the ring was dry; dropped from
    /// the ring as soon as they arrive.
    debt: u64,
    underrun: bool,
    /// Device frames sent while debt was owed and no capture backlog could
    /// produce the burst that repays it.
    stuck_frames: u64,
    /// Device frames sent since debt was last owed.
    clean_frames: u64,
    /// Past this, the device and cycle timelines diverged: rebase both.
    stuck_limit: u64,
    resync: bool,
    reaped: Vec<*mut IsoUrb>,
}

// SAFETY: the raw pointers reference allocations owned by the pools, which
// move into the I/O thread together with them.
unsafe impl Send for IoLoop<'_> {}

impl<'a> IoLoop<'a> {
    fn new(
        device: &'a UsbDevice,
        shared: &'a Shared,
        timeline: &'a HardwareTimeline,
        config: &HardwareConfig,
        layout: Layout,
    ) -> Result<Self, EngineError> {
        let capture_frame_bytes = config.capture.channels as usize * SUBSLOT_BYTES as usize;
        let playback_frame_bytes = config.playback.channels as usize * SUBSLOT_BYTES as usize;
        let playback_max_frames =
            layout.playback.data.max_packet_bytes / playback_frame_bytes as u32;
        let capture_max_frames = layout.capture.data.max_packet_bytes / capture_frame_bytes as u32;
        if playback_max_frames == 0 || capture_max_frames == 0 {
            return Err(EngineError::InvalidConfig(
                "endpoint max packet size is smaller than one frame".into(),
            ));
        }
        let packets = layout.packets_per_urb.min(MAX_ISO_PACKETS);
        if layout
            .playback
            .feedback
            .is_some_and(|endpoint| endpoint.max_packet_bytes < 3)
        {
            return Err(EngineError::InvalidConfig(
                "feedback endpoint packets are shorter than three bytes".into(),
            ));
        }
        let feedback = layout.playback.feedback.map(|endpoint| {
            UrbPool::new(
                PoolKind::Feedback,
                endpoint.address,
                FEEDBACK_URBS,
                1,
                endpoint.max_packet_bytes,
            )
        });
        let total_urbs = layout.capture_urbs + layout.playback_urbs + FEEDBACK_URBS;
        Ok(Self {
            device,
            shared,
            timeline,
            capture: UrbPool::new(
                PoolKind::Capture,
                layout.capture.data.address,
                layout.capture_urbs,
                packets,
                layout.capture.data.max_packet_bytes,
            ),
            playback: UrbPool::new(
                PoolKind::Playback,
                layout.playback.data.address,
                layout.playback_urbs,
                packets,
                playback_max_frames * playback_frame_bytes as u32,
            ),
            feedback,
            out_clock: PacketClock::new(
                config.rate,
                layout.high_speed,
                layout.playback.data.interval_log2,
                playback_max_frames,
            ),
            capture_clock: PacketClock::new(
                config.rate,
                layout.high_speed,
                layout.capture.data.interval_log2,
                capture_max_frames,
            ),
            capture_frame_bytes,
            playback_frame_bytes,
            period: u64::from(config.period_size),
            debt: 0,
            underrun: false,
            stuck_frames: 0,
            clean_frames: 0,
            // Covers one full handoff plus a period of a cycle still rendering.
            stuck_limit: layout.lead_frames + 2 * u64::from(config.period_size),
            resync: false,
            reaped: Vec::with_capacity(total_urbs),
        })
    }

    fn run(&mut self, stop: &AtomicBool) -> Result<(), EngineError> {
        let result = self.stream(stop);
        let drained = match self.cancel_all() {
            Err(EngineError::Usb { source, .. })
                if result.is_ok()
                    && matches!(source.raw_os_error(), Some(libc::ENODEV | libc::ESHUTDOWN)) =>
            {
                Ok(())
            }
            other => other,
        };
        result.and(drained)
    }

    fn stream(&mut self, stop: &AtomicBool) -> Result<(), EngineError> {
        self.submit_all()?;
        let mut idle_ms = 0;
        loop {
            if self.shared.shutdown.load(Ordering::Acquire) || stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if !self
                .device
                .wait_reapable(IO_POLL_MS)
                .map_err(usb_error("wait for URB completion"))?
            {
                idle_ms += IO_POLL_MS;
                if idle_ms >= IO_STALL_MS {
                    return Err(EngineError::Usb {
                        operation: "stream",
                        source: io::Error::new(
                            io::ErrorKind::TimedOut,
                            "no isochronous completion for 1 s",
                        ),
                    });
                }
                continue;
            }
            idle_ms = 0;
            self.reap_batch()?;
            // Disconnect completes every URB at once; surface it before treating
            // the emptied queues as a recoverable XRUN.
            for &urb in &self.reaped {
                // SAFETY: every pointer in `reaped` was retired and is not in flight.
                let status = unsafe { (*urb).header.status };
                if status == -libc::ENODEV || status == -libc::ESHUTDOWN {
                    return Err(EngineError::Usb {
                        operation: "stream",
                        source: io::Error::from_raw_os_error(-status),
                    });
                }
            }
            // An emptied queue means the controller lost our place in the
            // stream, and unrepayable debt means capture time slipped against
            // playback; nothing can be realigned, so restart both directions.
            let restart = if self.capture.in_flight == 0 {
                Some(Some(StreamDirection::Capture))
            } else if self.playback.in_flight == 0 {
                Some(Some(StreamDirection::Playback))
            } else if self.resync || self.shared.resync_requested.swap(false, Ordering::AcqRel) {
                Some(None)
            } else {
                None
            };
            if let Some(xrun) = restart {
                if !self.restart(xrun, stop)? {
                    return Ok(());
                }
                continue;
            }
            for index in 0..self.reaped.len() {
                let urb = self.reaped[index];
                self.complete(urb)?;
            }
        }
    }

    /// Reaps every completed URB without resubmitting any, so an emptied
    /// queue is visible in the in-flight counts.
    fn reap_batch(&mut self) -> Result<(), EngineError> {
        self.reaped.clear();
        while self.reaped.len() < self.reaped.capacity() {
            let Some(urb) = self
                .device
                .reap_nonblocking()
                .map_err(usb_error("reap URB"))?
            else {
                break;
            };
            self.retire(urb)?;
            self.reaped.push(urb);
        }
        Ok(())
    }

    fn pool(&mut self, kind: PoolKind) -> Option<&mut UrbPool> {
        match kind {
            PoolKind::Capture => Some(&mut self.capture),
            PoolKind::Playback => Some(&mut self.playback),
            PoolKind::Feedback => self.feedback.as_mut(),
        }
    }

    fn locate(&mut self, urb: *mut IsoUrb) -> Result<(PoolKind, usize), EngineError> {
        // SAFETY: usbfs only returns URBs this thread submitted.
        let tag = unsafe { (*urb).tag() };
        let kind = match tag >> 16 {
            0 => PoolKind::Capture,
            1 => PoolKind::Playback,
            2 => PoolKind::Feedback,
            _ => return Err(unknown_urb()),
        };
        let index = tag & 0xffff;
        let pool = self.pool(kind).ok_or_else(unknown_urb)?;
        if index >= pool.count || pool.slot(index).cast::<IsoUrb>() != urb {
            return Err(unknown_urb());
        }
        Ok((kind, index))
    }

    fn retire(&mut self, urb: *mut IsoUrb) -> Result<(), EngineError> {
        let (kind, index) = self.locate(urb)?;
        let shared = self.shared;
        let pool = self.pool(kind).ok_or_else(unknown_urb)?;
        let slot = pool.slot(index);
        // SAFETY: the slot is ours again after reap.
        unsafe {
            if (*slot).in_flight {
                (*slot).in_flight = false;
                pool.in_flight -= 1;
                if kind == PoolKind::Playback {
                    shared
                        .out_inflight_frames
                        .fetch_sub(u64::from((*slot).frames), Ordering::Relaxed);
                }
            }
        }
        Ok(())
    }

    fn complete(&mut self, urb: *mut IsoUrb) -> Result<(), EngineError> {
        let (kind, index) = self.locate(urb)?;
        // SAFETY: reaped URB, not in flight.
        let status = unsafe { (*urb).header.status };
        if status == -libc::ENODEV || status == -libc::ESHUTDOWN {
            return Err(EngineError::Usb {
                operation: "stream",
                source: io::Error::from_raw_os_error(-status),
            });
        }
        match kind {
            PoolKind::Capture => self.complete_capture(index, status != 0)?,
            PoolKind::Feedback => self.complete_feedback(index),
            PoolKind::Playback => {
                // SAFETY: reaped URB, not in flight.
                let failed = status != 0
                    || unsafe { &(*urb).packets }[..self.playback.packets]
                        .iter()
                        .any(|packet| packet.status != 0);
                if failed {
                    self.timeline
                        .record_hardware_xrun(StreamDirection::Playback);
                }
            }
        }
        self.submit(kind, index)
    }

    fn complete_capture(&mut self, index: usize, mut failed: bool) -> Result<(), EngineError> {
        let slot = self.capture.slot(index);
        let packet_bytes = self.capture.packet_bytes as usize;
        for packet in 0..self.capture.packets {
            // SAFETY: reaped URB, not in flight.
            let descriptor = unsafe { (*slot).urb.packets[packet] };
            // A clean zero-length packet carries no frames (valid UAC2 async IN).
            if descriptor.status == 0 && descriptor.actual_length == 0 {
                continue;
            }
            let (requested, pushed) = if descriptor.status == 0
                && descriptor.actual_length as usize >= self.capture_frame_bytes
            {
                let length = (descriptor.actual_length as usize).min(packet_bytes);
                failed |= !length.is_multiple_of(self.capture_frame_bytes);
                let bytes = length - length % self.capture_frame_bytes;
                let offset = packet * packet_bytes;
                let data = &self.capture.data(index)[offset..offset + bytes];
                (
                    (bytes / self.capture_frame_bytes) as u64,
                    self.shared.capture.push_le_bytes(data),
                )
            } else {
                failed = true;
                let frames = u64::from(self.capture_clock.next_packet_frames());
                (frames, self.shared.capture.push_silence(frames))
            };
            if pushed < requested {
                return Err(EngineError::Usb {
                    operation: "capture",
                    source: io::Error::other("capture ring overflow: the PRO cycle stalled"),
                });
            }
        }
        if failed {
            self.timeline.record_hardware_xrun(StreamDirection::Capture);
        }
        if self.shared.capture.len() >= self.period {
            self.shared.wake.signal();
        }
        Ok(())
    }

    fn complete_feedback(&mut self, index: usize) {
        let Some(pool) = self.feedback.as_ref() else {
            return;
        };
        let slot = pool.slot(index);
        // SAFETY: reaped URB, not in flight.
        let descriptor = unsafe { (*slot).urb.packets[0] };
        let length = (descriptor.actual_length as usize).min(pool.urb_bytes);
        if descriptor.status == 0 && length >= 3 {
            let payload = &pool.data(index)[..length.min(4)];
            self.out_clock.apply_feedback(payload);
        }
    }

    /// Prepares one URB's packets and hands it to the kernel.
    fn submit(&mut self, kind: PoolKind, index: usize) -> Result<(), EngineError> {
        let mut lengths = [0_u32; MAX_ISO_PACKETS];
        let frames = match kind {
            PoolKind::Playback => self.fill_playback(index, &mut lengths),
            _ => {
                let pool = self.pool(kind).ok_or_else(unknown_urb)?;
                lengths[..pool.packets].fill(pool.packet_bytes);
                0
            }
        };
        let shared = self.shared;
        let device = self.device;
        let pool = self.pool(kind).ok_or_else(unknown_urb)?;
        let slot = pool.slot(index);
        // SAFETY: the slot is not in flight; its URB and data outlive the
        // submission because pools are only dropped after cancel_all.
        unsafe {
            (*slot).urb.prepare(&lengths[..pool.packets]);
            (*slot).frames = frames;
            device
                .submit(ptr::addr_of_mut!((*slot).urb))
                .map_err(usb_error("submit URB"))?;
            (*slot).in_flight = true;
        }
        pool.in_flight += 1;
        if kind == PoolKind::Playback {
            shared
                .out_inflight_frames
                .fetch_add(u64::from(frames), Ordering::Relaxed);
        }
        Ok(())
    }

    /// Fills an OUT URB at device rate from the playback ring.
    fn fill_playback(&mut self, index: usize, lengths: &mut [u32; MAX_ISO_PACKETS]) -> u32 {
        let mut frames = 0_u32;
        for length in &mut lengths[..self.playback.packets] {
            let packet_frames = self.out_clock.next_packet_frames();
            *length = packet_frames * self.playback_frame_bytes as u32;
            frames += packet_frames;
        }
        let bytes = frames as usize * self.playback_frame_bytes;
        let data = &mut self.playback.data(index)[..bytes];
        let ring = &self.shared.playback;
        let mut filled = 0;
        if self.shared.armed.load(Ordering::Acquire) {
            if self.debt > 0 {
                self.debt -= ring.skip(self.debt);
            }
            if self.debt == 0 {
                filled = ring.pop_le_bytes(data) as usize * self.playback_frame_bytes;
            }
            if filled < bytes {
                self.debt += ((bytes - filled) / self.playback_frame_bytes) as u64;
                if !self.underrun {
                    self.underrun = true;
                    // The USB stream kept running on silence: the cycle thread,
                    // not the hardware, missed this deadline.
                    self.timeline.record_pro_core_deadline_miss();
                }
            } else {
                self.underrun = false;
            }
            // A late cycle repays debt with its capture backlog. Debt that
            // outlives any backlog means capture time itself slipped. Only a
            // whole debt-free window clears the count, so debt that recurs
            // every period (a partial slip) still adds up to a rebase.
            if self.debt > 0 && self.shared.capture.len() < self.period {
                self.stuck_frames += u64::from(frames);
                self.clean_frames = 0;
                self.resync |= self.stuck_frames > self.stuck_limit;
            } else if self.debt == 0 {
                self.clean_frames += u64::from(frames);
                if self.clean_frames > self.stuck_limit {
                    self.stuck_frames = 0;
                }
            }
        }
        data[filled..].fill(0);
        frames
    }

    fn submit_all(&mut self) -> Result<(), EngineError> {
        // Capture and OUT queue back to back so both start on the same frame.
        for kind in [PoolKind::Capture, PoolKind::Feedback, PoolKind::Playback] {
            let count = self.pool(kind).map_or(0, |pool| pool.count);
            for index in 0..count {
                self.submit(kind, index)?;
            }
        }
        Ok(())
    }

    /// Cancels every URB in flight and waits until usbfs returns all of them.
    fn cancel_all(&mut self) -> Result<(), EngineError> {
        for kind in [PoolKind::Playback, PoolKind::Feedback, PoolKind::Capture] {
            let device = self.device;
            let Some(pool) = self.pool(kind) else {
                continue;
            };
            for index in 0..pool.count {
                let slot = pool.slot(index);
                // SAFETY: discard only looks the pointer up.
                if unsafe { (*slot).in_flight } {
                    let _ = device.discard(unsafe { ptr::addr_of_mut!((*slot).urb) });
                }
            }
        }
        let deadline = monotonic_nanos().saturating_add(DRAIN_TIMEOUT_NANOS);
        while self.in_flight() > 0 {
            match self.device.wait_reapable(IO_POLL_MS) {
                Ok(true) => {}
                Ok(false) => {
                    if monotonic_nanos() >= deadline {
                        return Err(EngineError::Usb {
                            operation: "drain URBs",
                            source: io::Error::new(io::ErrorKind::TimedOut, "URBs did not return"),
                        });
                    }
                    continue;
                }
                Err(error) => return Err(usb_error("drain URBs")(error)),
            }
            loop {
                match self.device.reap_nonblocking() {
                    Ok(Some(urb)) => self.retire(urb)?,
                    Ok(None) => break,
                    Err(error) => return Err(usb_error("drain URBs")(error)),
                }
            }
            if self.in_flight() > 0 && monotonic_nanos() >= deadline {
                return Err(EngineError::Usb {
                    operation: "drain URBs",
                    source: io::Error::new(io::ErrorKind::TimedOut, "URBs did not return"),
                });
            }
        }
        Ok(())
    }

    fn in_flight(&self) -> usize {
        self.capture.in_flight
            + self.playback.in_flight
            + self.feedback.as_ref().map_or(0, |pool| pool.in_flight)
    }

    /// Restarts both streams from silence. Returns false if asked to stop.
    fn restart(
        &mut self,
        xrun: Option<StreamDirection>,
        stop: &AtomicBool,
    ) -> Result<bool, EngineError> {
        if let Some(direction) = xrun {
            self.timeline.record_hardware_xrun(direction);
        }
        self.cancel_all()?;
        let epoch = self.shared.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        self.shared.wake.signal();
        let started = std::time::Instant::now();
        while self.shared.ack.load(Ordering::Acquire) != epoch {
            if self.shared.shutdown.load(Ordering::Acquire) || stop.load(Ordering::Relaxed) {
                return Ok(false);
            }
            if started.elapsed() > RESTART_ACK_TIMEOUT {
                return Err(EngineError::Usb {
                    operation: "restart",
                    source: io::Error::new(
                        io::ErrorKind::TimedOut,
                        "PRO cycle did not acknowledge the stream restart",
                    ),
                });
            }
            thread::sleep(Duration::from_micros(100));
        }
        // The cycle is parked until capture flows again: the ring is ours.
        self.shared.playback.skip(u64::MAX);
        self.shared.armed.store(false, Ordering::Release);
        self.debt = 0;
        self.underrun = false;
        self.stuck_frames = 0;
        self.clean_frames = 0;
        self.resync = false;
        self.shared.resync_requested.store(false, Ordering::Release);
        if xrun.is_some() {
            self.timeline.reset_after_hardware_restart();
        } else {
            self.timeline.reset_after_hardware_rebase();
        }
        self.submit_all()?;
        Ok(true)
    }
}

impl Drop for IoLoop<'_> {
    fn drop(&mut self) {
        if self.in_flight() > 0 {
            let _ = self.cancel_all();
        }
    }
}

fn unknown_urb() -> EngineError {
    EngineError::Usb {
        operation: "reap URB",
        source: io::Error::other("usbfs returned an unknown URB"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_for_profile(profile: &Profile) -> Layout {
        let dump = uac2::e1x2_descriptor_dump();
        let function = AudioFunction::parse(uac2::active_configuration(&dump, 1).unwrap()).unwrap();
        let playback = *function.find_stream(false, 8, 4).unwrap();
        let capture = *function.find_stream(true, 10, 4).unwrap();
        Layout::new(
            &profile.device,
            profile.device.sycon.as_ref().unwrap(),
            playback,
            capture,
            true,
        )
    }

    fn layout_for(profile: &str) -> Layout {
        layout_for_profile(&Profile::from_toml(profile).unwrap())
    }

    const SYCON_PROFILE: &str = include_str!("../../../../profiles/topping-e1x2-sycon.toml");

    #[test]
    fn e1x2_layout_budgets_handoff_ahead_of_one_urb() {
        let layout = layout_for(SYCON_PROFILE);
        // Q64 auto handoff is 48 frames (1 ms); one 1-packet URB is 7 frames;
        // safety defaults to one microframe (6 frames).
        assert_eq!(layout.handoff_nanos, 1_000_000);
        assert_eq!(layout.reserve_frames, 7 + 6);
        assert_eq!(layout.lead_frames, 48 + 7 + 6);
    }

    #[test]
    fn rejects_nonzero_pro_latency_periods() {
        let mut profile = Profile::from_toml(SYCON_PROFILE).unwrap();
        profile.device.pro_latency_periods = 1;
        let error = SyconEngine::open(profile)
            .err()
            .expect("pro_latency_periods != 0 must fail");
        assert!(matches!(error, EngineError::InvalidConfig(_)), "{error}");
    }

    /// Records bridge calls so the cycle can run without USB.
    #[derive(Default)]
    struct Recorder {
        calls: Vec<(&'static str, u64)>,
    }

    impl ProCaptureSink for Recorder {
        fn process_capture(&mut self, sequence: u64, _capture: &[i32]) {
            self.calls.push(("capture", sequence));
        }

        fn process_deferred_capture(&mut self, sequence: u64, _capture: &[i32]) {
            self.calls.push(("deferred", sequence));
        }
    }

    struct Playback(u64);

    impl ProPlaybackSource for Playback {
        fn process_playback(&mut self, sequence: u64, playback: &mut [i32]) {
            playback.fill(sequence as i32 + 1);
            self.0 += 1;
        }
    }

    fn test_shared(lead: u64) -> Shared {
        Shared {
            capture: FrameRing::new(2, 1024),
            playback: FrameRing::new(2, 1024 + lead as usize),
            armed: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            ack: AtomicU64::new(0),
            out_inflight_frames: AtomicU64::new(0),
            resync_requested: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            wake: EventFd::new().unwrap(),
        }
    }

    #[test]
    fn cycle_queues_lead_then_whole_blocks_and_signals_ready() {
        let shared = test_shared(40);
        let timeline = HardwareTimeline::default();
        shared.capture.push(&[7; 2 * 8 * 3]);
        let config = CycleConfig {
            period: 8,
            rate: 48_000,
            playback_channels: 2,
            playback_capacity: 1024 + 40,
            lead_frames: 40,
            reserve_frames: 4,
            handoff_nanos: 0,
        };
        let stop = AtomicBool::new(false);
        let ready = AtomicBool::new(false);
        let mut recorder = Recorder::default();
        let mut playback = Playback(0);
        let mut capture = [0; 16];
        let mut out = [0; 16];
        run_cycles(
            &shared,
            &timeline,
            config,
            &stop,
            Some(3),
            Some(&ready),
            &mut recorder,
            &mut playback,
            &mut capture,
            &mut out,
        );
        assert!(ready.load(Ordering::Acquire));
        assert!(shared.armed.load(Ordering::Acquire));
        assert_eq!(playback.0, 3);
        assert_eq!(timeline.snapshot().periods_processed, 3);
        // Lead silence (at most 40 frames), then blocks 0, 1, 2 in order.
        let queued = shared.playback.len();
        assert!((3 * 8..=40 + 3 * 8).contains(&queued));
        let mut drained = vec![0; queued as usize * 2];
        shared.playback.pop(&mut drained);
        let blocks = &drained[drained.len() - 3 * 16..];
        assert!(blocks[..16].iter().all(|&sample| sample == 1));
        assert!(blocks[16..32].iter().all(|&sample| sample == 2));
        assert!(blocks[32..].iter().all(|&sample| sample == 3));
        assert_eq!(
            recorder.calls,
            vec![
                ("capture", 0),
                ("deferred", 0),
                ("capture", 1),
                ("deferred", 1),
                ("capture", 2),
                ("deferred", 2)
            ]
        );
    }

    #[test]
    fn restart_before_the_cycle_starts_is_acknowledged() {
        let shared = test_shared(16);
        shared.epoch.store(1, Ordering::Release);
        shared.capture.push(&[1; 2 * 8]);
        let config = CycleConfig {
            period: 8,
            rate: 48_000,
            playback_channels: 2,
            playback_capacity: 1024 + 16,
            lead_frames: 16,
            reserve_frames: 4,
            handoff_nanos: 0,
        };
        let stop = AtomicBool::new(false);
        run_cycles(
            &shared,
            &HardwareTimeline::default(),
            config,
            &stop,
            Some(0),
            None,
            &mut Recorder::default(),
            &mut Playback(0),
            &mut [0; 16],
            &mut [0; 16],
        );
        assert_eq!(shared.ack.load(Ordering::Acquire), 1);
    }

    #[test]
    fn debt_without_capture_backlog_requests_a_rebase() {
        let profile = Profile::from_toml(SYCON_PROFILE).unwrap();
        let layout = layout_for(SYCON_PROFILE);
        // fill_playback issues no ioctls, so any file stands in for usbfs.
        let device = UsbDevice::open(std::path::Path::new("/dev/null")).unwrap();
        let mut shared = test_shared(0);
        shared.capture = FrameRing::new(10, 1024);
        shared.playback = FrameRing::new(8, 1024);
        shared.armed.store(true, Ordering::Release);
        let timeline = HardwareTimeline::default();
        let mut io = IoLoop::new(&device, &shared, &timeline, &profile.device, layout).unwrap();
        let mut lengths = [0; MAX_ISO_PACKETS];

        // A late cycle owes a burst from its capture backlog: keep waiting.
        shared.capture.push_silence(64);
        for _ in 0..40 {
            io.fill_playback(0, &mut lengths);
        }
        assert!(io.debt > 0);
        assert!(!io.resync);

        // Backlog gone, debt still owed: capture time slipped against playback.
        shared.capture.skip(u64::MAX);
        for _ in 0..40 {
            io.fill_playback(0, &mut lengths);
        }
        assert!(io.resync);
        assert_eq!(timeline.snapshot().pro_core_deadline_misses, 1);
        assert_eq!(timeline.snapshot().hw_playback_xruns, 0);
    }

    #[test]
    fn debt_recurring_every_period_still_requests_a_rebase() {
        let mut profile = Profile::from_toml(SYCON_PROFILE).unwrap();
        profile.device.sycon.as_mut().unwrap().packets_per_urb = 2;
        let layout = layout_for_profile(&profile);
        let device = UsbDevice::open(std::path::Path::new("/dev/null")).unwrap();
        let mut shared = test_shared(0);
        shared.capture = FrameRing::new(10, 1024);
        shared.playback = FrameRing::new(8, 1024);
        shared.armed.store(true, Ordering::Release);
        let timeline = HardwareTimeline::default();
        let mut io = IoLoop::new(&device, &shared, &timeline, &profile.device, layout).unwrap();
        let mut lengths = [0; MAX_ISO_PACKETS];
        // Each block lands three 12-frame URBs late and is repaid right away:
        // debt clears once per period, yet the timeline stays slipped.
        for _ in 0..20 {
            for _ in 0..3 {
                io.fill_playback(0, &mut lengths);
            }
            shared.playback.push(&[1; 8 * 60]);
            for _ in 0..2 {
                io.fill_playback(0, &mut lengths);
            }
        }
        assert!(io.resync);
    }

    /// Simulates the I/O thread restarting the streams while block 0 renders.
    struct RestartDuringRender<'a>(&'a AtomicU64);

    impl ProPlaybackSource for RestartDuringRender<'_> {
        fn process_playback(&mut self, _sequence: u64, playback: &mut [i32]) {
            self.0.store(1, Ordering::Release);
            playback.fill(9);
        }
    }

    #[test]
    fn restart_during_render_drops_the_stale_block_and_capture() {
        let shared = test_shared(16);
        let timeline = HardwareTimeline::default();
        shared.capture.push(&[1; 2 * 8 * 2]);
        let config = CycleConfig {
            period: 8,
            rate: 48_000,
            playback_channels: 2,
            playback_capacity: 1024 + 16,
            lead_frames: 16,
            reserve_frames: 4,
            handoff_nanos: 0,
        };
        let stop = AtomicBool::new(false);
        let mut recorder = Recorder::default();
        let mut playback = RestartDuringRender(&shared.epoch);
        let mut capture = [0; 16];
        let mut out = [0; 16];
        run_cycles(
            &shared,
            &timeline,
            config,
            &stop,
            Some(1),
            None,
            &mut recorder,
            &mut playback,
            &mut capture,
            &mut out,
        );
        assert_eq!(shared.ack.load(Ordering::Acquire), 1);
        assert_eq!(
            shared.playback.len(),
            0,
            "stale block must not reach the ring"
        );
        assert!(!shared.armed.load(Ordering::Acquire));
        assert_eq!(shared.capture.len(), 0, "pre-restart capture is dropped");
        assert_eq!(timeline.snapshot().periods_processed, 1);
    }

    #[test]
    fn complete_capture_skips_clean_zlp_and_pads_errors() {
        let mut profile = Profile::from_toml(SYCON_PROFILE).unwrap();
        profile.device.sycon.as_mut().unwrap().packets_per_urb = 2;
        let layout = layout_for_profile(&profile);
        let device = UsbDevice::open(std::path::Path::new("/dev/null")).unwrap();
        let mut shared = test_shared(0);
        shared.capture = FrameRing::new(10, 1024);
        shared.playback = FrameRing::new(8, 1024);
        let timeline = HardwareTimeline::default();
        let mut io = IoLoop::new(&device, &shared, &timeline, &profile.device, layout).unwrap();

        let packet_bytes = io.capture.packet_bytes as usize;
        let frame_bytes = io.capture_frame_bytes;
        // Packet 0: clean zero-length packet; packet 1: two valid frames.
        let sample_bytes = 0x1234_5678_i32.to_le_bytes();
        for chunk in io.capture.data(0)[packet_bytes..packet_bytes + 2 * frame_bytes]
            .as_chunks_mut::<4>()
            .0
        {
            *chunk = sample_bytes;
        }
        unsafe {
            let slot = io.capture.slot(0);
            (*slot).urb.packets[0] = usbfs::IsoPacketDesc {
                length: packet_bytes as u32,
                actual_length: 0,
                status: 0,
            };
            (*slot).urb.packets[1] = usbfs::IsoPacketDesc {
                length: packet_bytes as u32,
                actual_length: (2 * frame_bytes) as u32,
                status: 0,
            };
        }
        io.complete_capture(0, false).unwrap();
        assert_eq!(shared.capture.len(), 2);
        assert_eq!(timeline.snapshot().hw_capture_xruns, 0);
        let mut read = [0_i32; 20];
        assert_eq!(shared.capture.pop(&mut read), 2);
        assert!(read.iter().all(|&sample| sample == 0x1234_5678));

        // Packet 0: USB error; packet 1: corrupt sub-frame packet. Both pad silence.
        unsafe {
            let slot = io.capture.slot(0);
            (*slot).urb.packets[0] = usbfs::IsoPacketDesc {
                length: packet_bytes as u32,
                actual_length: 0,
                status: (-libc::EPROTO) as u32,
            };
            (*slot).urb.packets[1] = usbfs::IsoPacketDesc {
                length: packet_bytes as u32,
                actual_length: 4,
                status: 0,
            };
        }
        io.complete_capture(0, false).unwrap();
        assert_eq!(shared.capture.len(), 12);
        assert_eq!(timeline.snapshot().hw_capture_xruns, 1);
        let mut silence = [1_i32; 120];
        assert_eq!(shared.capture.pop(&mut silence), 12);
        assert!(silence.iter().all(|&sample| sample == 0));
    }

    #[test]
    fn urb_pool_drop_retains_buffers_while_in_flight() {
        let mut pool = UrbPool::new(PoolKind::Capture, 0x81, 2, 2, 64);
        let (slots, count, data, data_len) = (pool.slots, pool.count, pool.data, pool.data_len);
        pool.data(0)[0] = 0x5a;
        pool.in_flight = 1;
        drop(pool);
        // SAFETY: Drop leaked the allocations because in_flight > 0; reclaim them
        // here to verify they were not freed and avoid leaking in the test process.
        unsafe {
            assert_eq!(*data, 0x5a);
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(slots, count)));
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(data, data_len)));
        }
    }

    struct IndexedPlayback;

    impl ProPlaybackSource for IndexedPlayback {
        fn process_playback(&mut self, _sequence: u64, playback: &mut [i32]) {
            for (index, sample) in playback.iter_mut().enumerate() {
                *sample = (index / 2) as i32 + 1;
            }
        }
    }

    #[test]
    fn arm_pending_clamps_capture_backlog_and_overshoot() {
        let config = CycleConfig {
            period: 8,
            rate: 48_000,
            playback_channels: 2,
            playback_capacity: 1024 + 20,
            lead_frames: 20,
            reserve_frames: 4,
            handoff_nanos: 0,
        };
        let stop = AtomicBool::new(false);

        // Backlog within lead: capture + playback matches lead + period.
        let shared = test_shared(20);
        shared.capture.push_silence(8 + 10);
        run_cycles(
            &shared,
            &HardwareTimeline::default(),
            config,
            &stop,
            Some(1),
            None,
            &mut Recorder::default(),
            &mut IndexedPlayback,
            &mut [0; 16],
            &mut [0; 16],
        );
        assert!(shared.armed.load(Ordering::Acquire));
        assert_eq!(shared.capture.len(), 10);
        assert_eq!(shared.capture.len() + shared.playback.len(), 20 + 8);

        // Moderate overshoot (lead < backlog < lead + period): trims leading frames.
        let shared = test_shared(20);
        shared.capture.push_silence(8 + 23);
        run_cycles(
            &shared,
            &HardwareTimeline::default(),
            config,
            &stop,
            Some(1),
            None,
            &mut Recorder::default(),
            &mut IndexedPlayback,
            &mut [0; 16],
            &mut [0; 16],
        );
        assert!(shared.armed.load(Ordering::Acquire));
        assert_eq!(shared.capture.len(), 23);
        assert_eq!(shared.playback.len(), 5);
        assert_eq!(shared.capture.len() + shared.playback.len(), 20 + 8);
        let mut drained = [0_i32; 10];
        shared.playback.pop(&mut drained);
        assert_eq!(drained, [4, 4, 5, 5, 6, 6, 7, 7, 8, 8]);

        // Severe overshoot (backlog >= lead + period): drops stale block, arms on next.
        let shared = test_shared(20);
        shared.capture.push_silence(8 + 30);
        run_cycles(
            &shared,
            &HardwareTimeline::default(),
            config,
            &stop,
            Some(1),
            None,
            &mut Recorder::default(),
            &mut IndexedPlayback,
            &mut [0; 16],
            &mut [0; 16],
        );
        assert!(!shared.armed.load(Ordering::Acquire));
        assert_eq!(shared.playback.len(), 0);
        run_cycles(
            &shared,
            &HardwareTimeline::default(),
            config,
            &stop,
            Some(1),
            None,
            &mut Recorder::default(),
            &mut IndexedPlayback,
            &mut [0; 16],
            &mut [0; 16],
        );
        assert!(shared.armed.load(Ordering::Acquire));
        assert_eq!(shared.capture.len(), 22);
        assert_eq!(shared.playback.len(), 6);
        assert_eq!(shared.capture.len() + shared.playback.len(), 20 + 8);
    }

    #[test]
    fn playback_ring_full_drops_whole_block_and_requests_resync() {
        let mut shared = test_shared(0);
        shared.playback = FrameRing::new(2, 12);
        shared.capture.push_silence(16);
        let timeline = HardwareTimeline::default();
        let config = CycleConfig {
            period: 8,
            rate: 48_000,
            playback_channels: 2,
            playback_capacity: 12,
            lead_frames: 8,
            reserve_frames: 0,
            handoff_nanos: 0,
        };
        let stop = AtomicBool::new(false);
        run_cycles(
            &shared,
            &timeline,
            config,
            &stop,
            Some(2),
            None,
            &mut Recorder::default(),
            &mut Playback(0),
            &mut [0; 16],
            &mut [0; 16],
        );
        // Block 0 pushed 8 frames (leaving 4 free < 8); block 1 dropped whole.
        assert_eq!(shared.playback.len(), 8);
        assert!(shared.resync_requested.load(Ordering::Acquire));
        assert_eq!(timeline.snapshot().hw_playback_xruns, 0);
    }
}
