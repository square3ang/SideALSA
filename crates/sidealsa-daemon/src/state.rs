use sidealsa_client::PlaybackConsume;
use sidealsa_config::{PortConfig, Profile};
use sidealsa_core::{HardwareStats, HardwareTimeline, ProCaptureSink, ProPlaybackSource};
use sidealsa_protocol::{
    DeviceInfo, PortDirection, PortInfo, SHARED_CLIENT_IDLE, SHARED_CLIENT_RUNNING,
    SHARED_CLIENT_STARTING, SharedRegionInfo, Stats,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering},
};
use std::time::Instant;
use thiserror::Error;

use crate::shared::{SharedError, SharedEvents, SharedRegion};

const SESSION_CLOSING: u64 = u64::MAX;

struct SessionState {
    endpoint: Arc<EndpointSlot>,
    owner: Arc<AtomicU64>,
    active: Arc<AtomicU64>,
    lifecycle_generation: Arc<AtomicU64>,
    armed: Arc<AtomicU64>,
    warmup_blocks: Arc<AtomicU64>,
    period_frames: u32,
    playback_channels: u32,
    capture_channels: u32,
}

struct SessionEndpoint {
    session_id: u64,
    region: SharedRegion,
    events: SharedEvents,
}

impl SessionEndpoint {
    fn create(
        session_id: u64,
        period_frames: u32,
        playback_channels: u32,
        capture_channels: u32,
    ) -> Result<Box<Self>, SharedError> {
        Ok(Box::new(Self {
            session_id,
            region: SharedRegion::create(period_frames, playback_channels, capture_channels)?,
            events: SharedEvents::new()?,
        }))
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
}

struct EndpointSlot {
    current: AtomicPtr<SessionEndpoint>,
    rt_activity: AtomicU32,
}

impl EndpointSlot {
    fn new(endpoint: Box<SessionEndpoint>) -> Self {
        Self {
            current: AtomicPtr::new(Box::into_raw(endpoint)),
            rt_activity: AtomicU32::new(0),
        }
    }

    fn load(&self) -> EndpointGuard<'_> {
        // The guard increment and pointer load must stay in this SC order. A replacement that
        // observes zero readers can then prove that no reader still holds the old pointer.
        let activity = RtActivity::enter(&self.rt_activity);
        let current = self.current.load(Ordering::SeqCst);
        debug_assert!(!current.is_null());
        // SAFETY: `current` always comes from `Box::into_raw`. The activity guard was entered
        // before this load and prevents a replacing control thread from freeing this endpoint.
        let endpoint = unsafe { &*current };
        EndpointGuard {
            endpoint,
            _activity: activity,
        }
    }

    fn replace(&self, endpoint: Box<SessionEndpoint>) {
        let replacement = Box::into_raw(endpoint);
        let retired = self.current.swap(replacement, Ordering::SeqCst);
        self.wait_for_idle();
        debug_assert!(!retired.is_null());
        // SAFETY: session ownership serializes replacements, and the activity barrier proves
        // that no reader can still dereference the retired `Box::into_raw` pointer.
        unsafe {
            drop(Box::from_raw(retired));
        }
    }

    fn wait_for_idle(&self) {
        while self.rt_activity.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }
    }
}

impl Drop for EndpointSlot {
    fn drop(&mut self) {
        let current = *self.current.get_mut();
        debug_assert!(!current.is_null());
        // SAFETY: dropping the last `EndpointSlot` owner requires exclusive access, so no guard
        // can remain, and `current` is the one outstanding `Box::into_raw` pointer.
        unsafe {
            drop(Box::from_raw(current));
        }
    }
}

struct EndpointGuard<'a> {
    endpoint: &'a SessionEndpoint,
    _activity: RtActivity<'a>,
}

impl std::ops::Deref for EndpointGuard<'_> {
    type Target = SessionEndpoint;

    fn deref(&self) -> &Self::Target {
        self.endpoint
    }
}

struct RtActivity<'a>(&'a AtomicU32);

impl<'a> RtActivity<'a> {
    fn enter(counter: &'a AtomicU32) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for RtActivity<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl SessionState {
    fn new(
        period_frames: u32,
        playback_channels: u32,
        capture_channels: u32,
    ) -> Result<Self, SharedError> {
        Ok(Self {
            endpoint: Arc::new(EndpointSlot::new(SessionEndpoint::create(
                0,
                period_frames,
                playback_channels,
                capture_channels,
            )?)),
            owner: Arc::new(AtomicU64::new(0)),
            active: Arc::new(AtomicU64::new(0)),
            lifecycle_generation: Arc::new(AtomicU64::new(0)),
            armed: Arc::new(AtomicU64::new(0)),
            warmup_blocks: Arc::new(AtomicU64::new(0)),
            period_frames,
            playback_channels,
            capture_channels,
        })
    }

    fn try_open(
        &self,
        session_id: u64,
    ) -> Result<Option<(SharedRegionInfo, [std::os::fd::RawFd; 4])>, SharedError> {
        if self
            .owner
            .compare_exchange(0, session_id, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(None);
        }
        let endpoint = match SessionEndpoint::create(
            session_id,
            self.period_frames,
            self.playback_channels,
            self.capture_channels,
        ) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.owner.store(0, Ordering::Release);
                return Err(error);
            }
        };
        let info = endpoint.info();
        let fds = endpoint.fds();
        self.endpoint.replace(endpoint);
        Ok(Some((info, fds)))
    }

    fn start(&self, session_id: u64) -> bool {
        if self.owner.load(Ordering::Acquire) != session_id
            || self.active.load(Ordering::Acquire) != 0
        {
            return false;
        }
        let endpoint = self.endpoint.load();
        if endpoint.session_id != session_id {
            return false;
        }
        endpoint.region.reset_slots();
        endpoint.events.drain();
        let generation = self.lifecycle_generation.fetch_add(1, Ordering::AcqRel) + 1;
        endpoint.region.set_lifecycle_generation(generation);
        endpoint.region.reset_activation();
        endpoint.region.set_client_state(SHARED_CLIENT_STARTING);
        if self
            .active
            .compare_exchange(0, session_id, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            endpoint.region.set_client_state(SHARED_CLIENT_IDLE);
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
            let endpoint = self.endpoint.load();
            endpoint.region.reset_slots();
            endpoint.events.drain();
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
        let endpoint = self.endpoint.load();
        endpoint.region.reset_slots();
        endpoint.events.drain();
        self.owner.store(0, Ordering::Release);
        true
    }

    fn wait_for_rt_idle(&self) {
        self.endpoint.wait_for_idle();
    }

    #[cfg(test)]
    fn current(&self) -> EndpointGuard<'_> {
        self.endpoint.load()
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
    #[error("could not create shared session resources: {0}")]
    Resources(#[from] SharedError),
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
        let endpoint = self.pro.endpoint.load();
        stats_from_core(self.timeline.snapshot(), &endpoint.region)
    }

    pub fn hardware_ready(&self) -> bool {
        self.hardware_ready.load(Ordering::Acquire)
    }

    pub fn hardware_ready_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.hardware_ready)
    }

    pub fn open_pro(
        &self,
    ) -> Result<Option<(u64, SharedRegionInfo, [std::os::fd::RawFd; 4])>, SharedError> {
        let session_id = self.next_session_id();
        Ok(self
            .pro
            .try_open(session_id)?
            .map(|(shared, fds)| (session_id, shared, fds)))
    }

    pub fn open_shared(&self, port_id: &str) -> Result<Option<SharedOpen>, OpenSharedError> {
        let port = self
            .shared
            .iter()
            .find(|port| port.id.as_ref() == port_id)
            .ok_or_else(|| OpenSharedError::UnknownPort(port_id.to_owned()))?;
        let session_id = self.next_session_id();
        Ok(port
            .session
            .try_open(session_id)?
            .map(|(shared, fds)| SharedOpen {
                session_id,
                direction: port.direction,
                shared,
                fds,
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
            pro_endpoint: Arc::clone(&self.pro.endpoint),
            pro_active: Arc::clone(&self.pro.active),
            pro_capture_index: 0,
            shared: capture_shared,
            timeline: Arc::clone(&self.timeline),
        };
        let playback = DaemonPlaybackBridge {
            pro_endpoint: Arc::clone(&self.pro.endpoint),
            pro_active: Arc::clone(&self.pro.active),
            pro_armed: Arc::clone(&self.pro.armed),
            pro_warmup_blocks: Arc::clone(&self.pro.warmup_blocks),
            shared: playback_shared,
            prepared_shared_sequence: None,
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
    pro_endpoint: Arc<EndpointSlot>,
    pro_active: Arc<AtomicU64>,
    pro_capture_index: usize,
    shared: Box<[SharedAudioPortBridge]>,
    timeline: Arc<HardwareTimeline>,
}

pub struct DaemonPlaybackBridge {
    pro_endpoint: Arc<EndpointSlot>,
    pro_active: Arc<AtomicU64>,
    pro_armed: Arc<AtomicU64>,
    pro_warmup_blocks: Arc<AtomicU64>,
    shared: Box<[SharedAudioPortBridge]>,
    prepared_shared_sequence: Option<u64>,
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
        self.playback.process_playback(sequence, playback);
        self.playback.commit_playback(sequence, playback);
    }
}

struct SharedAudioPortBridge {
    endpoint: Arc<EndpointSlot>,
    active: Arc<AtomicU64>,
    lifecycle_generation: Arc<AtomicU64>,
    armed: Arc<AtomicU64>,
    channels: Box<[usize]>,
    period_frames: usize,
    physical_channels: usize,
    capture_capacity_slots: usize,
    latency_periods: u64,
    index: usize,
    scratch: Box<[i32]>,
    prepared: Option<(u64, u64, u64)>,
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
            endpoint: Arc::clone(&port.session.endpoint),
            active: Arc::clone(&port.session.active),
            lifecycle_generation: Arc::clone(&port.session.lifecycle_generation),
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
            prepared: None,
        }
    }

    fn prepare_playback(&mut self, sequence: u64, timeline: &HardwareTimeline) {
        self.prepared = None;
        let endpoint = self.endpoint.load();
        endpoint
            .region
            .set_hardware_generation(timeline.generation());
        endpoint.region.set_cycle_sequence(sequence);
        let session_id = self.active.load(Ordering::Acquire);
        if session_id == 0 || endpoint.session_id != session_id {
            return;
        }
        if endpoint.region.establish_activation(sequence) {
            endpoint.events.notify_playback();
            return;
        }
        if endpoint.region.client_state() == SHARED_CLIENT_STARTING {
            endpoint.events.notify_playback();
            return;
        }
        if endpoint.region.client_state() != SHARED_CLIENT_RUNNING {
            return;
        }
        let generation = self.lifecycle_generation.load(Ordering::Acquire);
        let expected_sequence = sequence.wrapping_sub(self.latency_periods);
        let consumed = endpoint
            .region
            .try_consume_playback(expected_sequence, &mut self.scratch);
        let session_is_current = self.active.load(Ordering::Acquire) == session_id
            && self.lifecycle_generation.load(Ordering::Acquire) == generation
            && endpoint.session_id == session_id;
        if consumed && session_is_current {
            if self.armed.load(Ordering::Acquire) != session_id {
                let _ =
                    self.armed
                        .compare_exchange(0, session_id, Ordering::AcqRel, Ordering::Acquire);
            }
            self.prepared = Some((sequence, session_id, generation));
        } else if !consumed
            && session_is_current
            && self.armed.load(Ordering::Acquire) == session_id
        {
            timeline.record_shared_underrun();
            let _ = self
                .armed
                .compare_exchange(session_id, 0, Ordering::AcqRel, Ordering::Acquire);
        }
        endpoint.events.notify_playback();
    }

    fn commit_prepared(&mut self, sequence: u64, physical: &mut [i32]) {
        let Some((prepared_sequence, session_id, generation)) = self.prepared.take() else {
            return;
        };
        let endpoint = self.endpoint.load();
        if prepared_sequence != sequence
            || endpoint.session_id != session_id
            || self.active.load(Ordering::Acquire) != session_id
            || self.lifecycle_generation.load(Ordering::Acquire) != generation
            || endpoint.region.client_state() != SHARED_CLIENT_RUNNING
        {
            return;
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
    }

    fn process_playback(
        &mut self,
        sequence: u64,
        physical: &mut [i32],
        timeline: &HardwareTimeline,
    ) {
        self.prepare_playback(sequence, timeline);
        self.commit_prepared(sequence, physical);
    }

    fn process_capture(&mut self, sequence: u64, physical: &[i32], timeline: &HardwareTimeline) {
        let endpoint = self.endpoint.load();
        endpoint
            .region
            .set_hardware_generation(timeline.generation());
        endpoint.region.set_cycle_sequence(sequence);
        let session_id = self.active.load(Ordering::Acquire);
        if session_id == 0
            || endpoint.session_id != session_id
            || endpoint.region.client_state() == SHARED_CLIENT_IDLE
        {
            return;
        }
        if endpoint.region.establish_activation(sequence) {
            endpoint.region.set_client_state(SHARED_CLIENT_RUNNING);
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
        if endpoint.region.ready_capture_slots() >= self.capture_capacity_slots
            || !endpoint
                .region
                .try_publish_capture(&mut self.index, sequence, &self.scratch)
        {
            timeline.record_shared_overrun();
            endpoint.region.record_capture_discontinuity();
        }
        endpoint.events.notify_capture();
    }
}

impl DaemonCaptureBridge {
    fn publish_pro_capture(&mut self, playback_sequence: u64, capture: &[i32]) {
        let endpoint = self.pro_endpoint.load();
        endpoint
            .region
            .set_hardware_generation(self.timeline.generation());
        endpoint.region.set_cycle_sequence(playback_sequence);
        let session_id = self.pro_active.load(Ordering::Acquire);
        if session_id != 0
            && endpoint.session_id == session_id
            && !endpoint.region.establish_activation(playback_sequence)
            && endpoint.region.client_state() != SHARED_CLIENT_IDLE
        {
            if endpoint.region.try_publish_capture(
                &mut self.pro_capture_index,
                playback_sequence,
                capture,
            ) {
                endpoint.events.notify_capture();
            } else {
                self.timeline.record_pro_capture_overrun();
            }
        }
    }

    fn publish_shared_capture(&mut self, hardware_sequence: u64, capture: &[i32]) {
        for port in &mut self.shared {
            port.process_capture(hardware_sequence, capture, &self.timeline);
        }
    }
}

impl ProCaptureSink for DaemonCaptureBridge {
    fn process_capture(&mut self, sequence: u64, capture: &[i32]) {
        self.publish_pro_capture(sequence, capture);
        self.publish_shared_capture(sequence, capture);
    }

    fn process_capture_for_playback(
        &mut self,
        hardware_sequence: u64,
        playback_sequence: u64,
        capture: &[i32],
    ) {
        let _ = hardware_sequence;
        self.publish_pro_capture(playback_sequence, capture);
    }

    fn process_deferred_capture(&mut self, hardware_sequence: u64, capture: &[i32]) {
        self.publish_shared_capture(hardware_sequence, capture);
    }
}

impl ProPlaybackSource for DaemonPlaybackBridge {
    fn prepare_playback(&mut self, sequence: u64) {
        let endpoint = self.pro_endpoint.load();
        endpoint.region.set_playback_sequence(sequence);
    }

    fn prepare_playback_mix(&mut self, sequence: u64) {
        for port in &mut self.shared {
            port.prepare_playback(sequence, &self.timeline);
        }
        self.prepared_shared_sequence = Some(sequence);
    }

    fn process_playback(&mut self, sequence: u64, playback: &mut [i32]) {
        self.process_pro_playback(sequence, None, playback);
    }

    fn process_playback_before(&mut self, sequence: u64, cutoff_nanos: u64, playback: &mut [i32]) {
        self.process_pro_playback(sequence, Some(cutoff_nanos), playback);
    }

    fn commit_playback(&mut self, sequence: u64, playback: &mut [i32]) {
        if self.prepared_shared_sequence == Some(sequence) {
            self.prepared_shared_sequence = None;
            return;
        }
        for port in &mut self.shared {
            port.process_playback(sequence, playback, &self.timeline);
        }
    }
}

impl DaemonPlaybackBridge {
    fn process_pro_playback(
        &mut self,
        sequence: u64,
        cutoff_nanos: Option<u64>,
        playback: &mut [i32],
    ) {
        let wait_started = Instant::now();
        let endpoint = self.pro_endpoint.load();
        endpoint
            .region
            .set_hardware_generation(self.timeline.generation());
        endpoint.region.set_playback_sequence(sequence);
        playback.fill(0);
        let session_id = self.pro_active.load(Ordering::Acquire);
        if session_id != 0
            && endpoint.session_id == session_id
            && endpoint.region.activation_ready()
            && endpoint.region.client_state() != SHARED_CLIENT_IDLE
        {
            endpoint.events.drain_playback_ready();
            let consumed = if self.pro_active.load(Ordering::Acquire) == session_id
                && endpoint.region.client_state() == SHARED_CLIENT_RUNNING
            {
                match cutoff_nanos {
                    Some(cutoff) => {
                        endpoint
                            .region
                            .try_consume_playback_before(sequence, cutoff, playback)
                            == PlaybackConsume::Ready
                    }
                    None => endpoint.region.try_consume_playback(sequence, playback),
                }
            } else {
                false
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
            } else if endpoint.region.client_state() == SHARED_CLIENT_RUNNING
                && self.pro_armed.load(Ordering::Acquire) == session_id
            {
                self.timeline.record_pro_deadline_miss();
            } else if endpoint.region.client_state() == SHARED_CLIENT_STARTING {
                self.pro_warmup_blocks.store(0, Ordering::Release);
            }
            endpoint.events.notify_playback();
        }
        endpoint
            .region
            .set_playback_sequence(sequence.wrapping_add(1));
        if self.prepared_shared_sequence == Some(sequence) {
            for port in &mut self.shared {
                port.commit_prepared(sequence, playback);
            }
        }
    }
}

fn device_info(profile: &Profile) -> DeviceInfo {
    DeviceInfo {
        name: profile.device.name.clone(),
        profile_fingerprint: profile.fingerprint(),
        rate: profile.device.rate,
        period_size: profile.device.period_size,
        hardware_period_size: profile.device.effective_hardware_period_size(),
        buffer_size: profile.device.buffer_size,
        shared_buffer_size: profile.device.effective_shared_buffer_size(),
        pro_latency_periods: profile.device.pro_latency_periods,
        pro_output_latency_frames: profile.device.effective_pro_output_latency_frames(),
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
    use std::{
        io,
        mem::size_of,
        os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
        time::Duration,
    };

    const PROFILE: &str = r#"
        [device]
        name = "Test"
        rate = 48000
        period_size = 4
        buffer_size = 8
        pro_handoff_us = 10
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
        let endpoint = session.current();
        endpoint.region.set_cycle_sequence(sequence);
        assert!(endpoint.region.establish_activation(sequence));
        if endpoint.region.info().playback_channels == 0 {
            endpoint.region.set_client_state(SHARED_CLIENT_RUNNING);
        }
    }

    fn open_pro(state: &DaemonState) -> (u64, SharedRegionInfo, [std::os::fd::RawFd; 4]) {
        state
            .open_pro()
            .expect("PRO resources should create")
            .expect("PRO should open")
    }

    fn duplicate_fd(fd: RawFd) -> OwnedFd {
        let duplicate = unsafe { libc::dup(fd) };
        assert!(duplicate >= 0, "file descriptor should duplicate");
        unsafe { OwnedFd::from_raw_fd(duplicate) }
    }

    fn notify_fd(fd: RawFd) {
        let value = 1_u64;
        assert_eq!(
            unsafe { libc::write(fd, (&value as *const u64).cast(), size_of::<u64>()) },
            size_of::<u64>() as isize
        );
    }

    fn read_event(fd: RawFd) -> Result<u64, io::Error> {
        let mut value = 0_u64;
        let result = unsafe { libc::read(fd, (&mut value as *mut u64).cast(), size_of::<u64>()) };
        if result == size_of::<u64>() as isize {
            Ok(value)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[test]
    fn armed_pro_deadline_miss_keeps_hardware_running() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let session = open_pro(&state).0;
        assert!(state.start(session));
        activate_session(&state, session, 9);

        let (_, mut playback) = state.bridges();
        let mut producer_index = 0;
        let mut output = [0; 8];
        for sequence in [10, 11] {
            assert!(state.pro.current().region.try_client_publish_playback(
                &mut producer_index,
                sequence,
                &[sequence as i32; 8],
            ));
            playback.process_playback(sequence, &mut output);
        }

        playback.process_playback(12, &mut output);
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
        state.shared[1]
            .session
            .current()
            .region
            .set_cycle_sequence(42);

        assert!(state.start(shared.session_id));
        let (mut capture, _) = state.bridges();
        capture.process_capture(43, &[0; 8]);
        assert_eq!(
            state.shared[1].session.current().region.start_sequence(),
            43
        );
        assert_eq!(
            state.shared[1]
                .session
                .current()
                .region
                .ready_capture_slots(),
            0
        );
    }

    #[test]
    fn stop_waits_for_inflight_rt_work_before_reset() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = Arc::new(
            DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
                .expect("state should create"),
        );
        let session = open_pro(&state).0;
        assert!(state.start(session));
        state
            .pro
            .endpoint
            .rt_activity
            .fetch_add(1, Ordering::SeqCst);
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
        state
            .pro
            .endpoint
            .rt_activity
            .fetch_sub(1, Ordering::SeqCst);
        assert!(
            done_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("stop should finish")
        );
        worker.join().expect("stop worker should not panic");
    }

    #[test]
    fn endpoint_replacement_waits_for_pre_swap_reader() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = Arc::new(
            DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
                .expect("state should create"),
        );
        let first_session = open_pro(&state).0;
        assert!(state.close(first_session));
        let old_endpoint = state.pro.current();
        assert_eq!(old_endpoint.session_id, first_session);

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let open_state = Arc::clone(&state);
        let worker = std::thread::spawn(move || {
            done_tx
                .send(open_pro(&open_state).0)
                .expect("open result should send");
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if state.pro.current().session_id != first_session {
                break;
            }
            assert!(Instant::now() < deadline, "replacement should publish");
            std::thread::yield_now();
        }
        assert_eq!(old_endpoint.region.info().period_frames, 4);
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));

        drop(old_endpoint);
        let second_session = done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("open should finish after reader exits");
        worker.join().expect("open worker should not panic");
        assert!(state.close(second_session));
    }

    #[test]
    fn pro_and_shared_sessions_are_independent() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let (pro, _, _) = open_pro(&state);
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");

        assert!(state.start(pro));
        assert!(state.start(shared.session_id));
        assert!(
            state
                .open_pro()
                .expect("busy PRO open should not allocate")
                .is_none()
        );
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
        let (pro_session, _, _) = open_pro(&state);
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
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut pro_client_index,
            0,
            &pro_playback,
        ));
        let mut playback_client_index = 0;
        let playback = [10, 20, 30, 40, 50, 60, 70, 80];
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut playback_client_index, 0, &playback,)
        );
        let capture = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut output = [0; 8];
        bridge.process(0, &capture, &mut output);
        assert_eq!(output, [110, 120, 130, 140, 150, 160, 170, 180]);

        let mut capture_client_index = 0;
        let mut logical_capture = [0; 4];
        assert_eq!(
            state.shared[1]
                .session
                .current()
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
    fn prepared_shared_mix_is_applied_once() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(shared.session_id));
        activate_session(&state, shared.session_id, u64::MAX);

        let mut producer_index = 0;
        let contribution = [10, 20, 30, 40, 50, 60, 70, 80];
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 0, &contribution,)
        );

        let (_, mut playback) = state.bridges();
        let mut output = [0; 8];
        playback.prepare_playback_mix(0);
        playback.process_playback(0, &mut output);
        playback.commit_playback(0, &mut output);

        assert_eq!(output, contribution);
    }

    #[test]
    fn prepared_shared_mix_is_discarded_after_stop() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(shared.session_id));
        activate_session(&state, shared.session_id, u64::MAX);

        let mut producer_index = 0;
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(
                    &mut producer_index,
                    0,
                    &[10, 20, 30, 40, 50, 60, 70, 80],
                )
        );
        let (_, mut playback) = state.bridges();
        playback.prepare_playback_mix(0);
        assert!(state.stop(shared.session_id));

        let mut output = [0; 8];
        playback.process_playback(0, &mut output);
        playback.commit_playback(0, &mut output);
        assert_eq!(output, [0; 8]);
    }

    #[test]
    fn process_ahead_keeps_shared_capture_on_hardware_sequence() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let pro_session = open_pro(&state).0;
        let shared = state
            .open_shared("mic1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(pro_session));
        assert!(state.start(shared.session_id));
        activate_session(&state, pro_session, 9);
        activate_session(&state, shared.session_id, 9);

        let (mut capture, _) = state.bridges();
        capture.process_capture_for_playback(10, 11, &[1, 2, 3, 4, 5, 6, 7, 8]);

        let mut pro_index = 0;
        let mut pro_samples = [0; 8];
        assert_eq!(
            state
                .pro
                .current()
                .region
                .try_client_read_capture(&mut pro_index, &mut pro_samples),
            Some(11)
        );
        let mut shared_index = 0;
        let mut shared_samples = [0; 4];
        assert_eq!(
            state.shared[1]
                .session
                .current()
                .region
                .try_client_read_capture(&mut shared_index, &mut shared_samples),
            None
        );
        capture.process_deferred_capture(10, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            state.shared[1]
                .session
                .current()
                .region
                .try_client_read_capture(&mut shared_index, &mut shared_samples),
            Some(10)
        );
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
            state.shared[0].session.current().region.client_state(),
            SHARED_CLIENT_STARTING
        );

        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(0, &[0; 8], &mut output);

        let mut notification = 0_u64;
        let bytes = unsafe {
            libc::read(
                state.shared[0].session.current().events.playback_fd(),
                (&mut notification as *mut u64).cast(),
                std::mem::size_of::<u64>(),
            )
        };
        assert_eq!(bytes, std::mem::size_of::<u64>() as isize);
        assert_eq!(state.shared[0].session.current().region.cycle_sequence(), 0);

        let mut producer_index = 0;
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 1, &[11; 8],)
        );
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
        let first_generation = state.shared[0]
            .session
            .current()
            .region
            .lifecycle_generation();

        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(10, &[0; 8], &mut output);
        let mut producer_index = 0;
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 11, &[11; 8],)
        );

        assert!(state.stop(shared.session_id));
        assert!(
            !state.shared[0]
                .session
                .current()
                .region
                .try_consume_playback(11, &mut output)
        );
        let mut notification = 0_u64;
        let bytes = unsafe {
            libc::read(
                state.shared[0].session.current().events.playback_fd(),
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
        let second_generation = state.shared[0]
            .session
            .current()
            .region
            .lifecycle_generation();
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
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 0, &[11; 8],)
        );

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
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, u64::MAX, &[7; 8],)
        );

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
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 102, &[102; 8],)
        );
        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(105, &[0; 8], &mut output);
        assert_eq!(output, [102; 8]);

        bridge.process(106, &[0; 8], &mut output);
        assert_eq!(output, [0; 8]);
        assert_eq!(timeline.snapshot().shared_underruns, 1);

        bridge.process(107, &[0; 8], &mut output);
        assert_eq!(timeline.snapshot().shared_underruns, 1);

        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 105, &[105; 8],)
        );
        bridge.process(108, &[0; 8], &mut output);
        assert_eq!(output, [105; 8]);
        assert_eq!(timeline.snapshot().shared_underruns, 1);

        bridge.process(109, &[0; 8], &mut output);
        assert_eq!(timeline.snapshot().shared_underruns, 2);
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
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 11, &[11; 8],)
        );
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
        let pro_session = open_pro(&state).0;
        assert!(state.start(pro_session));

        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(10, &[0; 8], &mut output);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 0);

        let mut producer_index = 0;
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            11,
            &[11; 8],
        ));
        bridge.process(11, &[0; 8], &mut output);
        assert_eq!(output, [11; 8]);

        bridge.process(12, &[0; 8], &mut output);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 0);

        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            13,
            &[13; 8],
        ));
        bridge.process(13, &[0; 8], &mut output);
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            14,
            &[14; 8],
        ));
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
        let pro_session = open_pro(&state).0;
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
        state.shared[1].session.current().events.drain();
        let failed_sequence = capacity;
        capture.process_capture(failed_sequence, &[0; 8]);

        assert_eq!(
            state.shared[1].session.current().region.cycle_sequence(),
            failed_sequence
        );
        assert_eq!(
            state.shared[1]
                .session
                .current()
                .region
                .capture_discontinuities(),
            1
        );
        assert_eq!(timeline.snapshot().shared_overruns, 1);
        let mut notification = 0_u64;
        assert_eq!(
            unsafe {
                libc::read(
                    state.shared[1].session.current().events.capture_fd(),
                    (&mut notification as *mut u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            },
            std::mem::size_of::<u64>() as isize
        );
        assert_eq!(notification, 1);
    }

    #[test]
    fn playback_cutoff_keeps_future_sequence() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let pro_session = open_pro(&state).0;
        assert!(state.start(pro_session));
        activate_session(&state, pro_session, 9);

        let (_, mut playback) = state.bridges();
        let mut producer_index = 0;
        let mut output = [0; 8];
        for sequence in [10, 11] {
            assert!(state.pro.current().region.try_client_publish_playback(
                &mut producer_index,
                sequence,
                &[sequence as i32; 8],
            ));
            playback.process_playback(sequence, &mut output);
        }
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            13,
            &[13; 8],
        ));

        playback.process_playback(12, &mut output);
        assert_eq!(output, [0; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 1);
        assert_eq!(state.pro.current().region.playback_sequence(), 13);

        playback.process_playback(13, &mut output);
        assert_eq!(output, [13; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 1);
        assert_eq!(state.pro.current().region.playback_sequence(), 14);
    }

    #[test]
    fn late_pro_block_does_not_poison_next_sequence() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let pro_session = open_pro(&state).0;
        assert!(state.start(pro_session));
        activate_session(&state, pro_session, 9);

        let (_, mut playback) = state.bridges();
        let mut producer_index = 0;
        let mut output = [0; 8];
        for sequence in [10, 11] {
            assert!(state.pro.current().region.try_client_publish_playback(
                &mut producer_index,
                sequence,
                &[sequence as i32; 8],
            ));
            playback.process_playback(sequence, &mut output);
        }

        playback.process_playback(12, &mut output);
        assert_eq!(output, [0; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 1);

        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            12,
            &[12; 8]
        ));
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            13,
            &[13; 8]
        ));
        playback.process_playback(13, &mut output);

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

        assert_eq!(state.pro.current().region.cycle_sequence(), 27);
    }

    #[test]
    fn inactive_shared_regions_track_hardware_sequence() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, timeline).expect("state should create");
        let mut bridge = state.bridge();
        let mut output = [0; 8];

        bridge.process(27, &[0; 8], &mut output);

        assert_eq!(
            state.shared[0].session.current().region.cycle_sequence(),
            27
        );
        assert_eq!(
            state.shared[1].session.current().region.cycle_sequence(),
            27
        );
    }

    #[test]
    fn opening_new_pro_session_discards_old_slots() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, timeline).expect("state should create");
        let first = open_pro(&state).0;
        let mut producer_index = 0;
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            9,
            &[9; 8],
        ));
        assert!(state.close(first));

        let second = open_pro(&state).0;
        assert_ne!(first, second);
        let mut output = [0; 8];
        assert!(
            !state
                .pro
                .current()
                .region
                .try_consume_playback(9, &mut output)
        );
    }

    #[test]
    fn reopened_pro_isolated_from_stale_mapping_and_events() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let mut bridge = state.bridge();

        let (first_session, first_info, first_fds) = open_pro(&state);
        let old_region = SharedRegion::map_fd(duplicate_fd(first_fds[0]).into_raw_fd(), first_info)
            .expect("old client region should map");
        let old_events = first_fds[1..]
            .iter()
            .map(|fd| duplicate_fd(*fd))
            .collect::<Vec<_>>();
        assert!(state.close(first_session));

        let (second_session, second_info, second_fds) = open_pro(&state);
        let new_region =
            SharedRegion::map_fd(duplicate_fd(second_fds[0]).into_raw_fd(), second_info)
                .expect("new client region should map");
        let new_events = second_fds[1..]
            .iter()
            .map(|fd| duplicate_fd(*fd))
            .collect::<Vec<_>>();

        for (old, new) in old_events.iter().zip(&new_events) {
            notify_fd(old.as_raw_fd());
            let error = read_event(new.as_raw_fd()).expect_err("new event must remain empty");
            assert_eq!(error.raw_os_error(), Some(libc::EAGAIN));
            notify_fd(new.as_raw_fd());
            assert_eq!(
                read_event(new.as_raw_fd()).expect("new event should work"),
                1
            );
        }

        assert!(state.start(second_session));
        let mut output = [0; 8];
        bridge.process(10, &[0; 8], &mut output);

        let mut old_index = 0;
        assert!(old_region.try_client_publish_playback(&mut old_index, 11, &[99; 8]));
        notify_fd(old_events[2].as_raw_fd());
        bridge.process(11, &[0; 8], &mut output);
        assert_eq!(output, [0; 8]);

        let mut new_index = 0;
        assert!(new_region.try_client_publish_playback(&mut new_index, 12, &[12; 8]));
        notify_fd(new_events[2].as_raw_fd());
        bridge.process(12, &[0; 8], &mut output);
        assert_eq!(output, [12; 8]);
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
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 0, &[1; 8],)
        );
        bridge.process(0, &[0; 8], &mut output);
        bridge.process(1, &[0; 8], &mut output);

        let stats = timeline.snapshot();
        assert_eq!(stats.shared_underruns, 1);
        assert_eq!(stats.pro_deadline_misses, 0);
        assert_eq!(stats.hw_playback_xruns, 0);
        assert_eq!(stats.generation, 0);
    }
}
