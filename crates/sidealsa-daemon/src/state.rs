use sidealsa_config::{PortConfig, Profile};
use sidealsa_core::{HardwareStats, HardwareTimeline, ProCaptureSink, ProPlaybackSource};
use sidealsa_protocol::{
    DeviceInfo, PortDirection, PortInfo, SHARED_CLIENT_IDLE, SHARED_CLIENT_RUNNING,
    SHARED_CLIENT_STARTING, SharedRegionInfo, Stats,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
use std::time::Instant;
use thiserror::Error;

use crate::shared::{PlaybackWaitResult, SharedError, SharedEvents, SharedRegion};

const SESSION_CLOSING: u64 = u64::MAX;

struct SessionState {
    region: Arc<SharedRegion>,
    events: Arc<SharedEvents>,
    owner: Arc<AtomicU64>,
    active: Arc<AtomicU64>,
    lifecycle_generation: Arc<AtomicU64>,
    armed: Arc<AtomicU64>,
    warmup_blocks: Arc<AtomicU64>,
    rt_activity: Arc<AtomicU32>,
}

struct RtActivity<'a>(&'a AtomicU32);

impl<'a> RtActivity<'a> {
    fn enter(counter: &'a AtomicU32) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}

impl Drop for RtActivity<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

impl SessionState {
    fn new(
        period_frames: u32,
        playback_channels: u32,
        capture_channels: u32,
    ) -> Result<Self, SharedError> {
        Ok(Self {
            region: Arc::new(SharedRegion::create(
                period_frames,
                playback_channels,
                capture_channels,
            )?),
            events: Arc::new(SharedEvents::new()?),
            owner: Arc::new(AtomicU64::new(0)),
            active: Arc::new(AtomicU64::new(0)),
            lifecycle_generation: Arc::new(AtomicU64::new(0)),
            armed: Arc::new(AtomicU64::new(0)),
            warmup_blocks: Arc::new(AtomicU64::new(0)),
            rt_activity: Arc::new(AtomicU32::new(0)),
        })
    }

    fn try_open(&self, session_id: u64) -> bool {
        if self
            .owner
            .compare_exchange(0, session_id, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.region.reset_slots();
        self.events.drain();
        true
    }

    fn start(&self, session_id: u64) -> bool {
        if self.owner.load(Ordering::Acquire) != session_id
            || self.active.load(Ordering::Acquire) != 0
        {
            return false;
        }
        self.region.reset_slots();
        self.events.drain();
        let generation = self.lifecycle_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.region.set_lifecycle_generation(generation);
        self.region.reset_activation();
        self.region.set_client_state(SHARED_CLIENT_STARTING);
        if self
            .active
            .compare_exchange(0, session_id, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.region.set_client_state(SHARED_CLIENT_IDLE);
            return false;
        }
        self.armed.store(0, Ordering::Release);
        self.warmup_blocks.store(0, Ordering::Release);
        true
    }

    fn stop(&self, session_id: u64) -> bool {
        if self.active.load(Ordering::Acquire) != session_id {
            return false;
        }
        self.armed.store(0, Ordering::Release);
        self.warmup_blocks.store(0, Ordering::Release);
        let stopped = self
            .active
            .compare_exchange(session_id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if stopped {
            self.wait_for_rt_idle();
            self.region.reset_slots();
            self.events.drain();
        }
        stopped
    }

    fn close(&self, session_id: u64) -> bool {
        if self
            .owner
            .compare_exchange(
                session_id,
                SESSION_CLOSING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        if self.active.load(Ordering::Acquire) == session_id {
            self.armed.store(0, Ordering::Release);
            self.warmup_blocks.store(0, Ordering::Release);
        }
        let stopped =
            self.active
                .compare_exchange(session_id, 0, Ordering::AcqRel, Ordering::Acquire);
        let _ = stopped;
        self.wait_for_rt_idle();
        self.region.reset_slots();
        self.events.drain();
        self.owner.store(0, Ordering::Release);
        true
    }

    fn info(&self) -> SharedRegionInfo {
        self.region.info()
    }

    fn fds(&self) -> [std::os::fd::RawFd; 4] {
        [
            self.region.fd(),
            self.events.capture_fd(),
            self.events.playback_fd(),
            self.events.playback_ready_fd(),
        ]
    }

    fn wait_for_rt_idle(&self) {
        while self.rt_activity.load(Ordering::Acquire) != 0 {
            std::thread::yield_now();
        }
    }
}

struct SharedPortState {
    id: Box<str>,
    direction: PortDirection,
    channels: Box<[usize]>,
    logical_samples: usize,
    session: SessionState,
}

impl SharedPortState {
    fn new(
        port: &PortConfig,
        direction: PortDirection,
        period_frames: u32,
    ) -> Result<Self, SharedError> {
        let channels = port
            .channels
            .iter()
            .copied()
            .map(|channel| {
                usize::try_from(channel).map_err(|_| {
                    SharedError::Protocol(sidealsa_protocol::ProtocolError::LayoutOverflow)
                })
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_boxed_slice();
        let channel_count = u32::try_from(channels.len())
            .map_err(|_| SharedError::Protocol(sidealsa_protocol::ProtocolError::LayoutOverflow))?;
        let logical_samples = usize::try_from(u64::from(period_frames) * u64::from(channel_count))
            .map_err(|_| SharedError::Protocol(sidealsa_protocol::ProtocolError::LayoutOverflow))?;
        let (playback_channels, capture_channels) = match direction {
            PortDirection::Playback => (channel_count, 0),
            PortDirection::Capture => (0, channel_count),
        };
        Ok(Self {
            id: port.id.clone().into_boxed_str(),
            direction,
            channels,
            logical_samples,
            session: SessionState::new(period_frames, playback_channels, capture_channels)?,
        })
    }
}

pub struct DaemonState {
    info: DeviceInfo,
    timeline: Arc<HardwareTimeline>,
    hardware_ready: Arc<AtomicBool>,
    pro: SessionState,
    shared: Box<[SharedPortState]>,
    next_session: AtomicU64,
    period_frames: usize,
    playback_channels: usize,
    capture_channels: usize,
    shared_buffer_periods: usize,
    shared_latency_periods: u32,
}

#[derive(Debug, Error)]
pub enum OpenSharedError {
    #[error("unknown shared port '{0}'")]
    UnknownPort(String),
}

pub struct SharedOpen {
    pub session_id: u64,
    pub direction: PortDirection,
    pub shared: SharedRegionInfo,
    pub fds: [std::os::fd::RawFd; 4],
}

impl DaemonState {
    pub fn new(profile: &Profile, timeline: Arc<HardwareTimeline>) -> Result<Self, SharedError> {
        let period_frames = usize::try_from(profile.device.period_size)
            .map_err(|_| SharedError::Protocol(sidealsa_protocol::ProtocolError::LayoutOverflow))?;
        let playback_channels = usize::try_from(profile.device.playback.channels)
            .map_err(|_| SharedError::Protocol(sidealsa_protocol::ProtocolError::LayoutOverflow))?;
        let capture_channels = usize::try_from(profile.device.capture.channels)
            .map_err(|_| SharedError::Protocol(sidealsa_protocol::ProtocolError::LayoutOverflow))?;
        let shared_buffer_periods = usize::try_from(
            profile.device.effective_shared_buffer_size() / profile.device.period_size,
        )
        .map_err(|_| SharedError::Protocol(sidealsa_protocol::ProtocolError::LayoutOverflow))?;
        let pro = SessionState::new(
            profile.device.period_size,
            profile.device.playback.channels,
            profile.device.capture.channels,
        )?;
        let mut shared =
            Vec::with_capacity(profile.ports.playback.len() + profile.ports.capture.len());
        for port in &profile.ports.playback {
            shared.push(SharedPortState::new(
                port,
                PortDirection::Playback,
                profile.device.period_size,
            )?);
        }
        for port in &profile.ports.capture {
            shared.push(SharedPortState::new(
                port,
                PortDirection::Capture,
                profile.device.period_size,
            )?);
        }
        Ok(Self {
            info: device_info(profile),
            timeline,
            hardware_ready: Arc::new(AtomicBool::new(false)),
            pro,
            shared: shared.into_boxed_slice(),
            next_session: AtomicU64::new(1),
            period_frames,
            playback_channels,
            capture_channels,
            shared_buffer_periods,
            shared_latency_periods: profile.device.shared_latency_periods,
        })
    }

    pub fn info(&self) -> DeviceInfo {
        self.info.clone()
    }

    pub fn stats(&self) -> Stats {
        stats_from_core(self.timeline.snapshot(), &self.pro.region)
    }

    pub fn hardware_ready(&self) -> bool {
        self.hardware_ready.load(Ordering::Acquire)
    }

    pub fn hardware_ready_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.hardware_ready)
    }

    pub fn open_pro(&self) -> Option<(u64, SharedRegionInfo, [std::os::fd::RawFd; 4])> {
        let session_id = self.next_session_id();
        self.pro
            .try_open(session_id)
            .then(|| (session_id, self.pro.info(), self.pro.fds()))
    }

    pub fn open_shared(&self, port_id: &str) -> Result<Option<SharedOpen>, OpenSharedError> {
        let port = self
            .shared
            .iter()
            .find(|port| port.id.as_ref() == port_id)
            .ok_or_else(|| OpenSharedError::UnknownPort(port_id.to_owned()))?;
        let session_id = self.next_session_id();
        Ok(port.session.try_open(session_id).then(|| SharedOpen {
            session_id,
            direction: port.direction,
            shared: port.session.info(),
            fds: port.session.fds(),
        }))
    }

    pub fn start(&self, session_id: u64) -> bool {
        if self.pro.owner.load(Ordering::Acquire) == session_id {
            if self.pro.active.load(Ordering::Acquire) != 0 {
                return false;
            }
            return self.pro.start(session_id);
        }
        self.shared
            .iter()
            .any(|port| port.session.start(session_id))
    }

    pub fn stop(&self, session_id: u64) -> bool {
        if self.pro.stop(session_id) {
            return true;
        }
        self.shared.iter().any(|port| port.session.stop(session_id))
    }

    pub fn close(&self, session_id: u64) -> bool {
        if self.pro.close(session_id) {
            return true;
        }
        self.shared
            .iter()
            .any(|port| port.session.close(session_id))
    }

    pub fn owns(&self, session_id: u64) -> bool {
        self.pro.owner.load(Ordering::Acquire) == session_id
            || self
                .shared
                .iter()
                .any(|port| port.session.owner.load(Ordering::Acquire) == session_id)
    }

    pub fn bridges(&self) -> (DaemonCaptureBridge, DaemonPlaybackBridge) {
        let capture_shared = self
            .shared
            .iter()
            .filter(|port| port.direction == PortDirection::Capture)
            .map(|port| {
                SharedAudioPortBridge::new(
                    port,
                    self.period_frames,
                    self.playback_channels,
                    self.capture_channels,
                    self.shared_buffer_periods,
                    self.shared_latency_periods,
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let playback_shared = self
            .shared
            .iter()
            .filter(|port| port.direction == PortDirection::Playback)
            .map(|port| {
                SharedAudioPortBridge::new(
                    port,
                    self.period_frames,
                    self.playback_channels,
                    self.capture_channels,
                    self.shared_buffer_periods,
                    self.shared_latency_periods,
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let capture = DaemonCaptureBridge {
            pro_region: Arc::clone(&self.pro.region),
            pro_events: Arc::clone(&self.pro.events),
            pro_active: Arc::clone(&self.pro.active),
            pro_rt_activity: Arc::clone(&self.pro.rt_activity),
            pro_capture_index: 0,
            shared: capture_shared,
            timeline: Arc::clone(&self.timeline),
        };
        let playback = DaemonPlaybackBridge {
            pro_region: Arc::clone(&self.pro.region),
            pro_events: Arc::clone(&self.pro.events),
            pro_active: Arc::clone(&self.pro.active),
            pro_armed: Arc::clone(&self.pro.armed),
            pro_warmup_blocks: Arc::clone(&self.pro.warmup_blocks),
            pro_rt_activity: Arc::clone(&self.pro.rt_activity),
            shared: playback_shared,
            timeline: Arc::clone(&self.timeline),
        };
        (capture, playback)
    }

    #[cfg(test)]
    fn bridge(&self) -> TestBridge {
        let (capture, playback) = self.bridges();
        TestBridge { capture, playback }
    }

    fn next_session_id(&self) -> u64 {
        loop {
            let session_id = self.next_session.fetch_add(1, Ordering::Relaxed);
            if session_id != 0 && session_id != SESSION_CLOSING {
                return session_id;
            }
        }
    }
}

pub struct DaemonCaptureBridge {
    pro_region: Arc<SharedRegion>,
    pro_events: Arc<SharedEvents>,
    pro_active: Arc<AtomicU64>,
    pro_rt_activity: Arc<AtomicU32>,
    pro_capture_index: usize,
    shared: Box<[SharedAudioPortBridge]>,
    timeline: Arc<HardwareTimeline>,
}

pub struct DaemonPlaybackBridge {
    pro_region: Arc<SharedRegion>,
    pro_events: Arc<SharedEvents>,
    pro_active: Arc<AtomicU64>,
    pro_armed: Arc<AtomicU64>,
    pro_warmup_blocks: Arc<AtomicU64>,
    pro_rt_activity: Arc<AtomicU32>,
    shared: Box<[SharedAudioPortBridge]>,
    timeline: Arc<HardwareTimeline>,
}

#[cfg(test)]
struct TestBridge {
    capture: DaemonCaptureBridge,
    playback: DaemonPlaybackBridge,
}

#[cfg(test)]
impl TestBridge {
    fn process(&mut self, sequence: u64, capture: &[i32], playback: &mut [i32]) {
        self.capture.process_capture(sequence, capture);
        self.playback
            .process_playback(sequence, playback, Instant::now());
    }
}

struct SharedAudioPortBridge {
    region: Arc<SharedRegion>,
    events: Arc<SharedEvents>,
    active: Arc<AtomicU64>,
    rt_activity: Arc<AtomicU32>,
    armed: Arc<AtomicU64>,
    channels: Box<[usize]>,
    period_frames: usize,
    physical_channels: usize,
    capture_capacity_slots: usize,
    latency_periods: u64,
    index: usize,
    scratch: Box<[i32]>,
}

impl SharedAudioPortBridge {
    fn new(
        port: &SharedPortState,
        period_frames: usize,
        playback_channels: usize,
        capture_channels: usize,
        capture_capacity_slots: usize,
        latency_periods: u32,
    ) -> Self {
        Self {
            region: Arc::clone(&port.session.region),
            events: Arc::clone(&port.session.events),
            active: Arc::clone(&port.session.active),
            rt_activity: Arc::clone(&port.session.rt_activity),
            armed: Arc::clone(&port.session.armed),
            channels: port.channels.clone(),
            period_frames,
            physical_channels: match port.direction {
                PortDirection::Playback => playback_channels,
                PortDirection::Capture => capture_channels,
            },
            capture_capacity_slots,
            latency_periods: u64::from(latency_periods),
            index: 0,
            scratch: vec![0; port.logical_samples].into_boxed_slice(),
        }
    }

    fn process_playback(
        &mut self,
        sequence: u64,
        physical: &mut [i32],
        timeline: &HardwareTimeline,
    ) {
        let _activity = RtActivity::enter(&self.rt_activity);
        self.region.set_cycle_sequence(sequence);
        let session_id = self.active.load(Ordering::Acquire);
        if session_id == 0 {
            return;
        }
        if self.region.establish_activation(sequence) {
            self.events.notify_playback();
            return;
        }
        if self.region.client_state() == SHARED_CLIENT_STARTING {
            self.events.notify_playback();
            return;
        }
        if self.region.client_state() != SHARED_CLIENT_RUNNING {
            return;
        }
        let expected_sequence = sequence.wrapping_sub(self.latency_periods);
        let consumed = self
            .region
            .try_consume_playback(expected_sequence, &mut self.scratch);
        let session_is_current = self.active.load(Ordering::Acquire) == session_id;
        if consumed && session_is_current {
            if self.armed.load(Ordering::Acquire) != session_id {
                let _ =
                    self.armed
                        .compare_exchange(0, session_id, Ordering::AcqRel, Ordering::Acquire);
            }
            let logical_channels = self.channels.len();
            for frame in 0..self.period_frames {
                let physical_offset = frame * self.physical_channels;
                let logical_offset = frame * logical_channels;
                for (logical_channel, &physical_channel) in self.channels.iter().enumerate() {
                    let physical_index = physical_offset + physical_channel;
                    physical[physical_index] = physical[physical_index]
                        .saturating_add(self.scratch[logical_offset + logical_channel]);
                }
            }
        } else if !consumed
            && session_is_current
            && self.armed.load(Ordering::Acquire) == session_id
        {
            timeline.record_shared_underrun();
        }
        self.events.notify_playback();
    }

    fn process_capture(&mut self, sequence: u64, physical: &[i32], timeline: &HardwareTimeline) {
        let _activity = RtActivity::enter(&self.rt_activity);
        self.region.set_cycle_sequence(sequence);
        if self.active.load(Ordering::Acquire) == 0
            || self.region.client_state() == SHARED_CLIENT_IDLE
        {
            return;
        }
        if self.region.establish_activation(sequence) {
            self.region.set_client_state(SHARED_CLIENT_RUNNING);
            return;
        }
        let logical_channels = self.channels.len();
        for frame in 0..self.period_frames {
            let physical_offset = frame * self.physical_channels;
            let logical_offset = frame * logical_channels;
            for (logical_channel, &physical_channel) in self.channels.iter().enumerate() {
                self.scratch[logical_offset + logical_channel] =
                    physical[physical_offset + physical_channel];
            }
        }
        if self.region.ready_capture_slots() >= self.capture_capacity_slots
            || !self
                .region
                .try_publish_capture(&mut self.index, sequence, &self.scratch)
        {
            timeline.record_shared_overrun();
            self.region.record_capture_discontinuity();
        }
        self.events.notify_capture();
    }
}

impl ProCaptureSink for DaemonCaptureBridge {
    fn process_capture(&mut self, sequence: u64, capture: &[i32]) {
        let _activity = RtActivity::enter(&self.pro_rt_activity);
        self.pro_region.set_cycle_sequence(sequence);
        let session_id = self.pro_active.load(Ordering::Acquire);
        if session_id != 0
            && !self.pro_region.establish_activation(sequence)
            && self.pro_region.client_state() != SHARED_CLIENT_IDLE
        {
            if self
                .pro_region
                .try_publish_capture(&mut self.pro_capture_index, sequence, capture)
            {
                self.pro_events.notify_capture();
            } else {
                self.timeline.record_pro_capture_overrun();
            }
        }
        for port in &mut self.shared {
            port.process_capture(sequence, capture, &self.timeline);
        }
    }
}

impl ProPlaybackSource for DaemonPlaybackBridge {
    fn process_playback(&mut self, sequence: u64, playback: &mut [i32], deadline: Instant) {
        let wait_started = Instant::now();
        let _activity = RtActivity::enter(&self.pro_rt_activity);
        self.pro_region.set_playback_sequence(sequence);
        playback.fill(0);
        let session_id = self.pro_active.load(Ordering::Acquire);
        if session_id != 0
            && self.pro_region.activation_ready()
            && self.pro_region.client_state() != SHARED_CLIENT_IDLE
        {
            let mut wait_failed = false;
            let mut may_consume = true;
            let consumed = loop {
                self.pro_events.drain_playback_ready();
                if self.pro_active.load(Ordering::Acquire) != session_id {
                    break false;
                }
                if may_consume
                    && self.pro_region.client_state() == SHARED_CLIENT_RUNNING
                    && self.pro_region.try_consume_playback(sequence, playback)
                {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                match self.pro_events.wait_playback_ready_until(deadline) {
                    PlaybackWaitResult::Ready => may_consume = Instant::now() < deadline,
                    PlaybackWaitResult::TimedOut => break false,
                    PlaybackWaitResult::Failed => {
                        wait_failed = true;
                        break false;
                    }
                }
            };
            self.timeline.record_pro_ready_wait(
                u64::try_from(wait_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            );
            if consumed {
                self.timeline
                    .record_pro_playback_block(playback.iter().any(|sample| *sample != 0));
                if self.pro_armed.load(Ordering::Acquire) != session_id {
                    let blocks = self.pro_warmup_blocks.fetch_add(1, Ordering::AcqRel) + 1;
                    if blocks >= 2 {
                        let _ = self.pro_armed.compare_exchange(
                            0,
                            session_id,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                }
            } else if self.pro_region.client_state() == SHARED_CLIENT_RUNNING
                && self.pro_armed.load(Ordering::Acquire) == session_id
            {
                if wait_failed {
                    self.timeline.record_pro_core_deadline_miss();
                } else {
                    self.timeline.record_pro_deadline_miss();
                }
            } else if self.pro_region.client_state() == SHARED_CLIENT_STARTING {
                self.pro_warmup_blocks.store(0, Ordering::Release);
            }
            self.pro_events.notify_playback();
        }
        for port in &mut self.shared {
            port.process_playback(sequence, playback, &self.timeline);
        }
    }
}

fn device_info(profile: &Profile) -> DeviceInfo {
    DeviceInfo {
        name: profile.device.name.clone(),
        rate: profile.device.rate,
        period_size: profile.device.period_size,
        hardware_period_size: profile.device.effective_hardware_period_size(),
        buffer_size: profile.device.buffer_size,
        shared_buffer_size: profile.device.effective_shared_buffer_size(),
        pro_latency_periods: profile.device.pro_latency_periods,
        pro_realtime_priority: profile.device.effective_pro_realtime_priority(),
        shared_latency_periods: profile.device.shared_latency_periods,
        playback_channels: profile.device.playback.channels,
        capture_channels: profile.device.capture.channels,
        playback_ports: profile.ports.playback.iter().map(port_info).collect(),
        capture_ports: profile.ports.capture.iter().map(port_info).collect(),
    }
}

fn port_info(port: &PortConfig) -> PortInfo {
    PortInfo {
        id: port.id.clone(),
        name: port.name.clone(),
        channels: port.channels.clone(),
    }
}

fn stats_from_core(stats: HardwareStats, pro_region: &SharedRegion) -> Stats {
    Stats {
        generation: stats.generation,
        sample_position: stats.sample_position,
        playback_position: stats.playback_position,
        capture_position: stats.capture_position,
        hw_playback_xruns: stats.hw_playback_xruns,
        hw_capture_xruns: stats.hw_capture_xruns,
        playback_delay_frames: stats.playback_delay_frames,
        capture_delay_frames: stats.capture_delay_frames,
        playback_delay_min_frames: stats.playback_delay_min_frames,
        playback_delay_max_frames: stats.playback_delay_max_frames,
        playback_ring_delay_frames: stats.playback_ring_delay_frames,
        playback_ring_delay_min_frames: stats.playback_ring_delay_min_frames,
        playback_ring_delay_max_frames: stats.playback_ring_delay_max_frames,
        playback_driver_delay_frames: stats.playback_driver_delay_frames,
        playback_driver_delay_min_frames: stats.playback_driver_delay_min_frames,
        playback_driver_delay_max_frames: stats.playback_driver_delay_max_frames,
        capture_delay_min_frames: stats.capture_delay_min_frames,
        capture_delay_max_frames: stats.capture_delay_max_frames,
        playback_target_overshoot_max_frames: stats.playback_target_overshoot_max_frames,
        capture_clock_wait_max_nanos: stats.capture_clock_wait_max_nanos,
        pro_wait_budget_min_nanos: stats.pro_wait_budget_min_nanos,
        pro_wait_budget_max_nanos: stats.pro_wait_budget_max_nanos,
        pro_ready_wait_max_nanos: stats.pro_ready_wait_max_nanos,
        playback_write_max_nanos: stats.playback_write_max_nanos,
        capture_to_playback_write_nanos: stats.capture_to_playback_write_nanos,
        capture_to_playback_write_min_nanos: stats.capture_to_playback_write_min_nanos,
        capture_to_playback_write_max_nanos: stats.capture_to_playback_write_max_nanos,
        linked_phase_attempts: stats.linked_phase_attempts,
        linked_phase_rebases: stats.linked_phase_rebases,
        linked_phase_score_nanos: stats.linked_phase_score_nanos,
        linked_phase_target_met: stats.linked_phase_target_met,
        playback_low_watermarks: stats.playback_low_watermarks,
        pro_deadline_misses: stats.pro_deadline_misses,
        pro_client_deadline_misses: stats.pro_client_deadline_misses,
        pro_core_deadline_misses: stats.pro_core_deadline_misses,
        pro_capture_overruns: stats.pro_capture_overruns,
        pro_expired_capture_blocks: pro_region.client_expired_capture_blocks(),
        pro_playback_submit_failures: pro_region.client_playback_submit_failures(),
        pro_realtime_failures: pro_region.client_realtime_failures(),
        pro_callback_overruns: pro_region.client_callback_overruns(),
        pro_callback_max_nanos: pro_region.client_callback_max_nanos(),
        pro_playback_blocks: stats.pro_playback_blocks,
        pro_playback_nonzero_blocks: stats.pro_playback_nonzero_blocks,
        shared_underruns: stats.shared_underruns,
        shared_overruns: stats.shared_overruns,
        timeline_resets: stats.timeline_resets,
        periods_processed: stats.periods_processed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const PROFILE: &str = r#"
        [device]
        name = "Test"
        rate = 48000
        period_size = 4
        buffer_size = 8
        shared_latency_periods = 0

        [device.playback]
        device = "hw:Test,0"
        channels = 2
        format = "S32_LE"

        [device.capture]
        device = "hw:Test,0"
        channels = 2
        format = "S32_LE"

        [[ports.playback]]
        id = "line1"
        name = "Line 1"
        channels = [0, 1]

        [[ports.capture]]
        id = "mic1"
        name = "Mic 1"
        channels = [0]
    "#;

    fn deadline_after(duration: Duration) -> Instant {
        let now = Instant::now();
        now.checked_add(duration).unwrap_or(now)
    }

    fn activate_session(state: &DaemonState, session_id: u64, sequence: u64) {
        let session = if state.pro.owner.load(Ordering::Acquire) == session_id {
            &state.pro
        } else {
            &state
                .shared
                .iter()
                .find(|port| port.session.owner.load(Ordering::Acquire) == session_id)
                .expect("session should exist")
                .session
        };
        session.region.set_cycle_sequence(sequence);
        assert!(session.region.establish_activation(sequence));
        if session.region.info().playback_channels == 0 {
            session.region.set_client_state(SHARED_CLIENT_RUNNING);
        }
    }

    #[test]
    fn armed_pro_deadline_miss_keeps_hardware_running() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let session = state.open_pro().expect("PRO should open").0;
        assert!(state.start(session));
        activate_session(&state, session, 9);

        let (_, mut playback) = state.bridges();
        let mut producer_index = 0;
        let mut output = [0; 8];
        for sequence in [10, 11] {
            assert!(state.pro.region.try_client_publish_playback(
                &mut producer_index,
                sequence,
                &[sequence as i32; 8],
            ));
            playback.process_playback(sequence, &mut output, deadline_after(Duration::ZERO));
        }

        playback.process_playback(12, &mut output, deadline_after(Duration::ZERO));
        assert_eq!(timeline.snapshot().pro_deadline_misses, 1);
    }

    #[test]
    fn shared_start_publishes_activation_sequence() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let shared = state
            .open_shared("mic1")
            .expect("port should exist")
            .expect("shared port should open");
        state.shared[1].session.region.set_cycle_sequence(42);

        assert!(state.start(shared.session_id));
        let (mut capture, _) = state.bridges();
        capture.process_capture(43, &[0; 8]);
        assert_eq!(state.shared[1].session.region.start_sequence(), 43);
        assert_eq!(state.shared[1].session.region.ready_capture_slots(), 0);
    }

    #[test]
    fn stop_waits_for_inflight_rt_work_before_reset() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = Arc::new(
            DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
                .expect("state should create"),
        );
        let session = state.open_pro().expect("PRO should open").0;
        assert!(state.start(session));
        state.pro.rt_activity.fetch_add(1, Ordering::AcqRel);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let stop_state = Arc::clone(&state);
        let worker = std::thread::spawn(move || {
            started_tx.send(()).expect("start signal should send");
            done_tx
                .send(stop_state.stop(session))
                .expect("stop result should send");
        });

        started_rx.recv().expect("stop should begin");
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        state.pro.rt_activity.fetch_sub(1, Ordering::Release);
        assert!(
            done_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("stop should finish")
        );
        worker.join().expect("stop worker should not panic");
    }

    #[test]
    fn pro_and_shared_sessions_are_independent() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let (pro, _, _) = state.open_pro().expect("PRO should open");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");

        assert!(state.start(pro));
        assert!(state.start(shared.session_id));
        assert!(state.open_pro().is_none());
        assert!(
            state
                .open_shared("line1")
                .expect("port should exist")
                .is_none()
        );
        assert!(
            state
                .open_shared("mic1")
                .expect("port should exist")
                .is_some()
        );
        assert!(state.close(pro));
        assert!(state.close(shared.session_id));
    }

    #[test]
    fn bridge_maps_shared_playback_and_capture() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let (pro_session, _, _) = state.open_pro().expect("PRO should open");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        let capture_session = state
            .open_shared("mic1")
            .expect("port should exist")
            .expect("capture port should open");
        assert!(state.start(pro_session));
        assert!(state.start(shared.session_id));
        assert!(state.start(capture_session.session_id));
        activate_session(&state, pro_session, u64::MAX);
        activate_session(&state, shared.session_id, u64::MAX);
        activate_session(&state, capture_session.session_id, u64::MAX);

        let mut bridge = state.bridge();
        let mut pro_client_index = 0;
        let pro_playback = [100; 8];
        assert!(state.pro.region.try_client_publish_playback(
            &mut pro_client_index,
            0,
            &pro_playback,
        ));
        let mut playback_client_index = 0;
        let playback = [10, 20, 30, 40, 50, 60, 70, 80];
        assert!(state.shared[0].session.region.try_client_publish_playback(
            &mut playback_client_index,
            0,
            &playback,
        ));
        let capture = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut output = [0; 8];
        bridge.process(0, &capture, &mut output);
        assert_eq!(output, [110, 120, 130, 140, 150, 160, 170, 180]);

        let mut capture_client_index = 0;
        let mut logical_capture = [0; 4];
        assert_eq!(
            state.shared[1]
                .session
                .region
                .try_client_read_capture(&mut capture_client_index, &mut logical_capture),
            Some(0)
        );
        assert_eq!(logical_capture, [1, 3, 5, 7]);
        assert_eq!(timeline.snapshot().shared_underruns, 0);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 0);
        assert_eq!(timeline.snapshot().hw_playback_xruns, 0);
    }

    #[test]
    fn shared_playback_startup_notifies_before_first_block() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, timeline).expect("state should create");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(shared.session_id));
        assert_eq!(
            state.shared[0].session.region.client_state(),
            SHARED_CLIENT_STARTING
        );

        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(0, &[0; 8], &mut output);

        let mut notification = 0_u64;
        let bytes = unsafe {
            libc::read(
                state.shared[0].session.events.playback_fd(),
                (&mut notification as *mut u64).cast(),
                std::mem::size_of::<u64>(),
            )
        };
        assert_eq!(bytes, std::mem::size_of::<u64>() as isize);
        assert_eq!(state.shared[0].session.region.cycle_sequence(), 0);

        let mut producer_index = 0;
        assert!(state.shared[0].session.region.try_client_publish_playback(
            &mut producer_index,
            1,
            &[11; 8],
        ));
        bridge.process(1, &[0; 8], &mut output);
        assert_eq!(output, [11; 8]);
    }

    #[test]
    fn restarting_shared_session_discards_old_slots_and_events() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(shared.session_id));
        let first_generation = state.shared[0].session.region.lifecycle_generation();

        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(10, &[0; 8], &mut output);
        let mut producer_index = 0;
        assert!(state.shared[0].session.region.try_client_publish_playback(
            &mut producer_index,
            11,
            &[11; 8],
        ));

        assert!(state.stop(shared.session_id));
        assert!(
            !state.shared[0]
                .session
                .region
                .try_consume_playback(11, &mut output)
        );
        let mut notification = 0_u64;
        let bytes = unsafe {
            libc::read(
                state.shared[0].session.events.playback_fd(),
                (&mut notification as *mut u64).cast(),
                std::mem::size_of::<u64>(),
            )
        };
        assert_eq!(bytes, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN)
        );

        assert!(state.start(shared.session_id));
        let second_generation = state.shared[0].session.region.lifecycle_generation();
        assert!(second_generation > first_generation);
    }

    #[test]
    fn shared_playback_consumes_configured_lookahead() {
        let profile_text =
            PROFILE.replace("shared_latency_periods = 0", "shared_latency_periods = 2");
        let profile = Profile::from_toml(&profile_text).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(shared.session_id));
        activate_session(&state, shared.session_id, 0);

        let mut producer_index = 0;
        assert!(state.shared[0].session.region.try_client_publish_playback(
            &mut producer_index,
            0,
            &[11; 8],
        ));

        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(0, &[0; 8], &mut output);
        assert_eq!(output, [0; 8]);
        bridge.process(1, &[0; 8], &mut output);
        assert_eq!(output, [0; 8]);
        bridge.process(2, &[0; 8], &mut output);
        assert_eq!(output, [11; 8]);
        assert_eq!(timeline.snapshot().shared_underruns, 0);
    }

    #[test]
    fn shared_playback_lookahead_wraps_sequence() {
        let profile_text =
            PROFILE.replace("shared_latency_periods = 0", "shared_latency_periods = 2");
        let profile = Profile::from_toml(&profile_text).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, timeline).expect("state should create");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(shared.session_id));
        activate_session(&state, shared.session_id, 0);

        let mut producer_index = 0;
        assert!(state.shared[0].session.region.try_client_publish_playback(
            &mut producer_index,
            u64::MAX,
            &[7; 8],
        ));

        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(1, &[0; 8], &mut output);
        assert_eq!(output, [7; 8]);
    }

    #[test]
    fn shared_playback_recovers_after_one_missing_sequence() {
        let profile_text =
            PROFILE.replace("shared_latency_periods = 0", "shared_latency_periods = 3");
        let profile = Profile::from_toml(&profile_text).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(shared.session_id));
        activate_session(&state, shared.session_id, 104);

        let mut producer_index = 0;
        assert!(state.shared[0].session.region.try_client_publish_playback(
            &mut producer_index,
            102,
            &[102; 8],
        ));
        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(105, &[0; 8], &mut output);
        assert_eq!(output, [102; 8]);

        bridge.process(106, &[0; 8], &mut output);
        assert_eq!(output, [0; 8]);
        assert_eq!(timeline.snapshot().shared_underruns, 1);

        assert!(state.shared[0].session.region.try_client_publish_playback(
            &mut producer_index,
            104,
            &[104; 8],
        ));
        bridge.process(107, &[0; 8], &mut output);
        assert_eq!(output, [104; 8]);
        assert_eq!(timeline.snapshot().shared_underruns, 1);
    }

    #[test]
    fn shared_startup_gap_does_not_count_until_first_block_is_consumed() {
        let profile_text =
            PROFILE.replace("shared_latency_periods = 0", "shared_latency_periods = 2");
        let profile = Profile::from_toml(&profile_text).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(shared.session_id));

        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(10, &[0; 8], &mut output);
        let mut producer_index = 0;
        assert!(state.shared[0].session.region.try_client_publish_playback(
            &mut producer_index,
            11,
            &[11; 8],
        ));
        bridge.process(11, &[0; 8], &mut output);
        bridge.process(12, &[0; 8], &mut output);
        assert_eq!(timeline.snapshot().shared_underruns, 0);

        bridge.process(13, &[0; 8], &mut output);
        assert_eq!(output, [11; 8]);
        bridge.process(14, &[0; 8], &mut output);
        assert_eq!(timeline.snapshot().shared_underruns, 1);
    }

    #[test]
    fn startup_gap_does_not_count_until_pro_is_armed() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let pro_session = state.open_pro().expect("PRO should open").0;
        assert!(state.start(pro_session));

        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(10, &[0; 8], &mut output);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 0);

        let mut producer_index = 0;
        assert!(
            state
                .pro
                .region
                .try_client_publish_playback(&mut producer_index, 11, &[11; 8],)
        );
        bridge.process(11, &[0; 8], &mut output);
        assert_eq!(output, [11; 8]);

        bridge.process(12, &[0; 8], &mut output);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 0);

        assert!(
            state
                .pro
                .region
                .try_client_publish_playback(&mut producer_index, 13, &[13; 8],)
        );
        bridge.process(13, &[0; 8], &mut output);
        assert!(
            state
                .pro
                .region
                .try_client_publish_playback(&mut producer_index, 14, &[14; 8],)
        );
        bridge.process(14, &[0; 8], &mut output);

        bridge.process(15, &[0; 8], &mut output);
        let stats = timeline.snapshot();
        assert_eq!(stats.pro_deadline_misses, 1);
        assert_eq!(stats.pro_client_deadline_misses, 1);
        assert_eq!(stats.pro_core_deadline_misses, 0);
    }

    #[test]
    fn full_pro_capture_ring_is_not_a_core_deadline_miss() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let pro_session = state.open_pro().expect("PRO should open").0;
        assert!(state.start(pro_session));
        activate_session(&state, pro_session, u64::MAX);

        let (mut capture, _) = state.bridges();
        for sequence in 0..=u64::from(sidealsa_protocol::SHARED_SLOT_COUNT) {
            capture.process_capture(sequence, &[sequence as i32; 8]);
        }

        let stats = timeline.snapshot();
        assert_eq!(stats.pro_deadline_misses, 0);
        assert_eq!(stats.pro_client_deadline_misses, 0);
        assert_eq!(stats.pro_core_deadline_misses, 0);
        assert_eq!(stats.pro_capture_overruns, 1);
    }

    #[test]
    fn full_shared_capture_ring_keeps_timeline_and_notification_moving() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let shared = state
            .open_shared("mic1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(shared.session_id));
        activate_session(&state, shared.session_id, u64::MAX);

        let (mut capture, _) = state.bridges();
        let capacity = state.shared_buffer_periods as u64;
        for sequence in 0..capacity {
            capture.process_capture(sequence, &[sequence as i32; 8]);
        }
        state.shared[1].session.events.drain();
        let failed_sequence = capacity;
        capture.process_capture(failed_sequence, &[0; 8]);

        assert_eq!(
            state.shared[1].session.region.cycle_sequence(),
            failed_sequence
        );
        assert_eq!(state.shared[1].session.region.capture_discontinuities(), 1);
        assert_eq!(timeline.snapshot().shared_overruns, 1);
        let mut notification = 0_u64;
        assert_eq!(
            unsafe {
                libc::read(
                    state.shared[1].session.events.capture_fd(),
                    (&mut notification as *mut u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            },
            std::mem::size_of::<u64>() as isize
        );
        assert_eq!(notification, 1);
    }

    #[test]
    fn playback_barrier_consumes_block_published_before_deadline() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let pro_session = state.open_pro().expect("PRO should open").0;
        assert!(state.start(pro_session));
        activate_session(&state, pro_session, 9);

        let (_, mut playback) = state.bridges();
        let region = Arc::clone(&state.pro.region);
        let events = Arc::clone(&state.pro.events);
        let mut output = [0; 8];
        std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(1));
                let mut producer_index = 0;
                assert!(region.try_client_publish_playback(&mut producer_index, 10, &[10; 8],));
                let value = 1_u64;
                assert_eq!(
                    unsafe {
                        libc::write(
                            events.playback_ready_fd(),
                            (&value as *const u64).cast(),
                            std::mem::size_of::<u64>(),
                        )
                    },
                    std::mem::size_of::<u64>() as isize
                );
            });
            playback.process_playback(10, &mut output, deadline_after(Duration::from_millis(10)));
        });

        assert_eq!(output, [10; 8]);
        assert_eq!(state.pro.region.playback_sequence(), 10);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 0);
    }

    #[test]
    fn playback_barrier_ignores_future_wake_until_exact_sequence_arrives() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let pro_session = state.open_pro().expect("PRO should open").0;
        assert!(state.start(pro_session));
        activate_session(&state, pro_session, 9);

        let (_, mut playback) = state.bridges();
        let region = Arc::clone(&state.pro.region);
        let events = Arc::clone(&state.pro.events);
        let mut producer_index = 0;
        assert!(region.try_client_publish_playback(&mut producer_index, 11, &[11; 8]));
        let value = 1_u64;
        assert_eq!(
            unsafe {
                libc::write(
                    events.playback_ready_fd(),
                    (&value as *const u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            },
            std::mem::size_of::<u64>() as isize
        );

        let mut output = [0; 8];
        std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(1));
                assert!(region.try_client_publish_playback(&mut producer_index, 10, &[10; 8],));
                let value = 1_u64;
                assert_eq!(
                    unsafe {
                        libc::write(
                            events.playback_ready_fd(),
                            (&value as *const u64).cast(),
                            std::mem::size_of::<u64>(),
                        )
                    },
                    std::mem::size_of::<u64>() as isize
                );
            });
            playback.process_playback(10, &mut output, deadline_after(Duration::from_millis(10)));
        });

        assert_eq!(output, [10; 8]);
        playback.process_playback(11, &mut output, deadline_after(Duration::ZERO));
        assert_eq!(output, [11; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 0);
    }

    #[test]
    fn late_pro_block_does_not_poison_next_sequence() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let pro_session = state.open_pro().expect("PRO should open").0;
        assert!(state.start(pro_session));
        activate_session(&state, pro_session, 9);

        let (_, mut playback) = state.bridges();
        let mut producer_index = 0;
        let mut output = [0; 8];
        for sequence in [10, 11] {
            assert!(state.pro.region.try_client_publish_playback(
                &mut producer_index,
                sequence,
                &[sequence as i32; 8],
            ));
            playback.process_playback(sequence, &mut output, deadline_after(Duration::ZERO));
        }

        playback.process_playback(12, &mut output, deadline_after(Duration::ZERO));
        assert_eq!(output, [0; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 1);

        assert!(
            state
                .pro
                .region
                .try_client_publish_playback(&mut producer_index, 12, &[12; 8])
        );
        assert!(
            state
                .pro
                .region
                .try_client_publish_playback(&mut producer_index, 13, &[13; 8])
        );
        playback.process_playback(13, &mut output, deadline_after(Duration::ZERO));

        assert_eq!(output, [13; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 1);
    }

    #[test]
    fn inactive_pro_region_tracks_hardware_sequence() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, timeline).expect("state should create");
        let mut bridge = state.bridge();
        let mut output = [0; 8];

        bridge.process(27, &[0; 8], &mut output);

        assert_eq!(state.pro.region.cycle_sequence(), 27);
    }

    #[test]
    fn inactive_shared_regions_track_hardware_sequence() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, timeline).expect("state should create");
        let mut bridge = state.bridge();
        let mut output = [0; 8];

        bridge.process(27, &[0; 8], &mut output);

        assert_eq!(state.shared[0].session.region.cycle_sequence(), 27);
        assert_eq!(state.shared[1].session.region.cycle_sequence(), 27);
    }

    #[test]
    fn opening_new_pro_session_discards_old_slots() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, timeline).expect("state should create");
        let first = state.open_pro().expect("PRO should open").0;
        let mut producer_index = 0;
        assert!(
            state
                .pro
                .region
                .try_client_publish_playback(&mut producer_index, 9, &[9; 8],)
        );
        assert!(state.close(first));

        let second = state.open_pro().expect("PRO should reopen").0;
        assert_ne!(first, second);
        let mut output = [0; 8];
        assert!(!state.pro.region.try_consume_playback(9, &mut output));
    }

    #[test]
    fn missing_shared_output_does_not_count_as_pro_or_hardware_failure() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(shared.session_id));
        activate_session(&state, shared.session_id, u64::MAX);
        let mut bridge = state.bridge();
        let mut output = [0; 8];
        let mut producer_index = 0;
        assert!(state.shared[0].session.region.try_client_publish_playback(
            &mut producer_index,
            0,
            &[1; 8],
        ));
        bridge.process(0, &[0; 8], &mut output);
        bridge.process(1, &[0; 8], &mut output);

        let stats = timeline.snapshot();
        assert_eq!(stats.shared_underruns, 1);
        assert_eq!(stats.pro_deadline_misses, 0);
        assert_eq!(stats.hw_playback_xruns, 0);
        assert_eq!(stats.generation, 0);
    }
}
