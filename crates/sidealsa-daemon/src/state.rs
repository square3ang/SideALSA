use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use sidealsa_config::{PortConfig, Profile};
use sidealsa_core::{HardwareStats, HardwareTimeline, ProCallback};
use sidealsa_protocol::{
    DeviceInfo, PortDirection, PortInfo, SHARED_CLIENT_IDLE, SHARED_CLIENT_RUNNING,
    SHARED_CLIENT_STARTING, SharedRegionInfo, Stats,
};
use thiserror::Error;

use crate::shared::{SharedError, SharedEvents, SharedRegion};

struct SessionState {
    region: Arc<SharedRegion>,
    events: Arc<SharedEvents>,
    owner: Arc<AtomicU64>,
    active: Arc<AtomicU64>,
    lifecycle_generation: Arc<AtomicU64>,
    armed: Arc<AtomicU64>,
    warmup_blocks: Arc<AtomicU64>,
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
        if self
            .active
            .compare_exchange(0, session_id, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
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
            self.region.reset_slots();
            self.events.drain();
        }
        stopped
    }

    fn close(&self, session_id: u64) -> bool {
        if self.active.load(Ordering::Acquire) == session_id {
            self.armed.store(0, Ordering::Release);
            self.warmup_blocks.store(0, Ordering::Release);
        }
        let stopped =
            self.active
                .compare_exchange(session_id, 0, Ordering::AcqRel, Ordering::Acquire);
        let closed = self
            .owner
            .compare_exchange(session_id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if closed {
            let _ = stopped;
            self.region.reset_slots();
            self.events.drain();
        }
        closed
    }

    fn info(&self) -> SharedRegionInfo {
        self.region.info()
    }

    fn fds(&self) -> [std::os::fd::RawFd; 3] {
        [
            self.region.fd(),
            self.events.capture_fd(),
            self.events.playback_fd(),
        ]
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
    pro: SessionState,
    shared: Box<[SharedPortState]>,
    next_session: AtomicU64,
    period_frames: usize,
    playback_channels: usize,
    capture_channels: usize,
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
    pub fds: [std::os::fd::RawFd; 3],
}

impl DaemonState {
    pub fn new(profile: &Profile, timeline: Arc<HardwareTimeline>) -> Result<Self, SharedError> {
        let period_frames = usize::try_from(profile.device.period_size)
            .map_err(|_| SharedError::Protocol(sidealsa_protocol::ProtocolError::LayoutOverflow))?;
        let playback_channels = usize::try_from(profile.device.playback.channels)
            .map_err(|_| SharedError::Protocol(sidealsa_protocol::ProtocolError::LayoutOverflow))?;
        let capture_channels = usize::try_from(profile.device.capture.channels)
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
            pro,
            shared: shared.into_boxed_slice(),
            next_session: AtomicU64::new(1),
            period_frames,
            playback_channels,
            capture_channels,
            shared_latency_periods: profile.device.shared_latency_periods,
        })
    }

    pub fn info(&self) -> DeviceInfo {
        self.info.clone()
    }

    pub fn stats(&self) -> Stats {
        stats_from_core(self.timeline.snapshot())
    }

    pub fn open_pro(&self) -> Option<(u64, SharedRegionInfo, [std::os::fd::RawFd; 3])> {
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
        if self.pro.start(session_id) {
            return true;
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

    pub fn bridge(&self) -> DaemonAudioBridge {
        let shared = self
            .shared
            .iter()
            .map(|port| {
                SharedAudioPortBridge::new(
                    port,
                    self.period_frames,
                    self.playback_channels,
                    self.capture_channels,
                    self.shared_latency_periods,
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        DaemonAudioBridge {
            pro_region: Arc::clone(&self.pro.region),
            pro_events: Arc::clone(&self.pro.events),
            pro_active: Arc::clone(&self.pro.active),
            pro_armed: Arc::clone(&self.pro.armed),
            pro_warmup_blocks: Arc::clone(&self.pro.warmup_blocks),
            pro_capture_index: 0,
            shared,
            timeline: Arc::clone(&self.timeline),
        }
    }

    fn next_session_id(&self) -> u64 {
        loop {
            let session_id = self.next_session.fetch_add(1, Ordering::Relaxed);
            if session_id != 0 {
                return session_id;
            }
        }
    }
}

pub struct DaemonAudioBridge {
    pro_region: Arc<SharedRegion>,
    pro_events: Arc<SharedEvents>,
    pro_active: Arc<AtomicU64>,
    pro_armed: Arc<AtomicU64>,
    pro_warmup_blocks: Arc<AtomicU64>,
    pro_capture_index: usize,
    shared: Box<[SharedAudioPortBridge]>,
    timeline: Arc<HardwareTimeline>,
}

struct SharedAudioPortBridge {
    region: Arc<SharedRegion>,
    events: Arc<SharedEvents>,
    active: Arc<AtomicU64>,
    direction: PortDirection,
    channels: Box<[usize]>,
    period_frames: usize,
    physical_channels: usize,
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
        latency_periods: u32,
    ) -> Self {
        Self {
            region: Arc::clone(&port.session.region),
            events: Arc::clone(&port.session.events),
            active: Arc::clone(&port.session.active),
            direction: port.direction,
            channels: port.channels.clone(),
            period_frames,
            physical_channels: match port.direction {
                PortDirection::Playback => playback_channels,
                PortDirection::Capture => capture_channels,
            },
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
        if self.active.load(Ordering::Acquire) == 0 {
            self.region.set_cycle_sequence(sequence);
            return;
        }
        if self.region.client_state() == SHARED_CLIENT_STARTING {
            self.region.set_cycle_sequence(sequence);
            self.events.notify_playback();
            return;
        }
        if self.region.client_state() != SHARED_CLIENT_RUNNING {
            return;
        }
        let expected_sequence = sequence.checked_sub(self.latency_periods);
        let consumed = expected_sequence.is_some_and(|expected| {
            self.region
                .try_consume_playback(expected, &mut self.scratch)
        });
        if consumed {
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
        } else if expected_sequence.is_some() {
            timeline.record_shared_underrun();
        }
        self.region.set_cycle_sequence(sequence);
        self.events.notify_playback();
    }

    fn process_capture(&mut self, sequence: u64, physical: &[i32], timeline: &HardwareTimeline) {
        if self.active.load(Ordering::Acquire) == 0 {
            self.region.set_cycle_sequence(sequence);
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
        if self
            .region
            .try_publish_capture(&mut self.index, sequence, &self.scratch)
        {
            self.region.set_cycle_sequence(sequence);
            self.events.notify_capture();
        } else {
            timeline.record_shared_overrun();
        }
    }
}

impl ProCallback for DaemonAudioBridge {
    fn process(&mut self, sequence: u64, capture: &[i32], playback: &mut [i32]) {
        playback.fill(0);
        self.pro_region.set_cycle_sequence(sequence);
        let session_id = self.pro_active.load(Ordering::Acquire);
        if session_id != 0 && self.pro_region.client_state() != SHARED_CLIENT_IDLE {
            if self
                .pro_region
                .try_publish_capture(&mut self.pro_capture_index, sequence, capture)
            {
                self.pro_events.notify_capture();
            }
            if self.pro_region.client_state() == SHARED_CLIENT_RUNNING
                && self.pro_region.try_consume_playback(sequence, playback)
            {
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
                self.timeline.record_pro_deadline_miss();
            } else if self.pro_region.client_state() == SHARED_CLIENT_STARTING {
                self.pro_warmup_blocks.store(0, Ordering::Release);
            }
            self.pro_events.notify_playback();
        }
        for port in &mut self.shared {
            match port.direction {
                PortDirection::Playback => {
                    port.process_playback(sequence, playback, &self.timeline);
                }
                PortDirection::Capture => {
                    port.process_capture(sequence, capture, &self.timeline);
                }
            }
        }
    }
}

fn device_info(profile: &Profile) -> DeviceInfo {
    DeviceInfo {
        name: profile.device.name.clone(),
        rate: profile.device.rate,
        period_size: profile.device.period_size,
        buffer_size: profile.device.buffer_size,
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

fn stats_from_core(stats: HardwareStats) -> Stats {
    Stats {
        generation: stats.generation,
        sample_position: stats.sample_position,
        playback_position: stats.playback_position,
        capture_position: stats.capture_position,
        hw_playback_xruns: stats.hw_playback_xruns,
        hw_capture_xruns: stats.hw_capture_xruns,
        pro_deadline_misses: stats.pro_deadline_misses,
        pro_client_deadline_misses: stats.pro_client_deadline_misses,
        pro_core_deadline_misses: stats.pro_core_deadline_misses,
        shared_underruns: stats.shared_underruns,
        shared_overruns: stats.shared_overruns,
        timeline_resets: stats.timeline_resets,
        periods_processed: stats.periods_processed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        state.shared[0]
            .session
            .region
            .set_client_state(SHARED_CLIENT_STARTING);

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
        state.shared[0]
            .session
            .region
            .set_client_state(SHARED_CLIENT_STARTING);

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
        state.shared[0]
            .session
            .region
            .set_client_state(SHARED_CLIENT_RUNNING);

        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(0, &[0; 8], &mut output);

        let stats = timeline.snapshot();
        assert_eq!(stats.shared_underruns, 1);
        assert_eq!(stats.pro_deadline_misses, 0);
        assert_eq!(stats.hw_playback_xruns, 0);
        assert_eq!(stats.generation, 0);
    }
}
