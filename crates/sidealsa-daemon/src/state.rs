use sidealsa_client::PlaybackConsume;
use sidealsa_config::{PortConfig, Profile};
use sidealsa_core::{HardwareStats, HardwareTimeline, ProCaptureSink, ProPlaybackSource};
use sidealsa_protocol::{
    DeviceInfo, PortDirection, PortInfo, SHARED_CLIENT_IDLE, SHARED_CLIENT_RUNNING,
    SHARED_CLIENT_STARTING, SharedPlaybackPortStats, SharedRegionInfo, Stats,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering},
};
use std::time::Instant;
use thiserror::Error;

use crate::diagnostics::{ProDiagnostics, ProDiagnosticsSnapshot, ProMiss};
use crate::shared::{PlaybackReadyWait, SharedError, SharedEvents, SharedRegion};

const SESSION_CLOSING: u64 = u64::MAX;

struct SessionState {
    endpoint: Arc<EndpointSlot>,
    owner: Arc<AtomicU64>,
    active: Arc<AtomicU64>,
    lifecycle_generation: Arc<AtomicU64>,
    lifecycle_hardware_generation: Arc<AtomicU64>,
    armed: Arc<AtomicU64>,
    outage: Arc<AtomicU64>,
    warmup_blocks: Arc<AtomicU64>,
    playback_epoch: Option<Arc<AtomicU64>>,
    playback_commits: Option<Arc<PlaybackCommitBarrier>>,
    period_frames: u32,
    playback_channels: u32,
    capture_channels: u32,
    slot_count: u32,
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
        slot_count: u32,
    ) -> Result<Box<Self>, SharedError> {
        Ok(Box::new(Self {
            session_id,
            region: SharedRegion::create_with_slot_count(
                period_frames,
                playback_channels,
                capture_channels,
                slot_count,
            )?,
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
    reader_epoch: AtomicU64,
    reader_activity: [AtomicU32; 2],
    retired_client_playback: Mutex<ClientPlaybackDiagnostics>,
    playback_underruns: AtomicU64,
    last_playback_underrun_sequence: AtomicU64,
    last_playback_underrun_nanos: AtomicU64,
    last_playback_sequence_lag_periods: AtomicU64,
    max_playback_sequence_lag_periods: AtomicU64,
}

#[derive(Clone, Copy, Default)]
struct ClientPlaybackDiagnostics {
    expired_playback_periods: u64,
    playback_submit_failures: u64,
    playback_xruns: u64,
}

impl EndpointSlot {
    fn new(endpoint: Box<SessionEndpoint>) -> Self {
        Self {
            current: AtomicPtr::new(Box::into_raw(endpoint)),
            reader_epoch: AtomicU64::new(0),
            reader_activity: [AtomicU32::new(0), AtomicU32::new(0)],
            retired_client_playback: Mutex::new(ClientPlaybackDiagnostics::default()),
            playback_underruns: AtomicU64::new(0),
            last_playback_underrun_sequence: AtomicU64::new(0),
            last_playback_underrun_nanos: AtomicU64::new(0),
            last_playback_sequence_lag_periods: AtomicU64::new(0),
            max_playback_sequence_lag_periods: AtomicU64::new(0),
        }
    }

    fn load(&self) -> EndpointGuard<'_> {
        loop {
            let epoch = self.reader_epoch.load(Ordering::SeqCst);
            let activity = RtActivity::enter(&self.reader_activity[(epoch & 1) as usize]);
            if self.reader_epoch.load(Ordering::SeqCst) != epoch {
                drop(activity);
                continue;
            }
            let current = self.current.load(Ordering::SeqCst);
            debug_assert!(!current.is_null());
            // SAFETY: `current` comes from `Box::into_raw`. A replacement swaps the pointer,
            // advances the reader epoch, and drains this epoch before freeing the old endpoint.
            let endpoint = unsafe { &*current };
            return EndpointGuard {
                endpoint,
                _activity: activity,
            };
        }
    }

    fn replace(&self, endpoint: Box<SessionEndpoint>) {
        let mut retired_diagnostics = self
            .retired_client_playback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let replacement = Box::into_raw(endpoint);
        let retired = self.current.swap(replacement, Ordering::SeqCst);
        self.wait_for_idle();
        debug_assert!(!retired.is_null());
        // SAFETY: session ownership serializes replacements, and the activity barrier proves
        // that no reader can still dereference the retired `Box::into_raw` pointer.
        let retired = unsafe { Box::from_raw(retired) };
        retired_diagnostics.expired_playback_periods = retired_diagnostics
            .expired_playback_periods
            .wrapping_add(retired.region.client_expired_playback_periods());
        retired_diagnostics.playback_submit_failures = retired_diagnostics
            .playback_submit_failures
            .wrapping_add(retired.region.client_playback_submit_failures());
        retired_diagnostics.playback_xruns = retired_diagnostics
            .playback_xruns
            .wrapping_add(retired.region.client_playback_xruns());
    }

    fn wait_for_idle(&self) {
        let retired_epoch = self.reader_epoch.fetch_add(1, Ordering::SeqCst);
        let retired_activity = &self.reader_activity[(retired_epoch & 1) as usize];
        while retired_activity.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }
    }

    fn record_playback_miss(&self, sequence: u64, sequence_lag_periods: u64, starts_outage: bool) {
        self.last_playback_underrun_sequence
            .store(sequence, Ordering::Relaxed);
        self.last_playback_underrun_nanos
            .store(monotonic_nanos(), Ordering::Relaxed);
        self.last_playback_sequence_lag_periods
            .store(sequence_lag_periods, Ordering::Relaxed);
        self.max_playback_sequence_lag_periods
            .fetch_max(sequence_lag_periods, Ordering::Relaxed);
        if starts_outage {
            self.playback_underruns.fetch_add(1, Ordering::Release);
        }
    }

    fn playback_diagnostics(&self) -> SharedPlaybackDiagnostics {
        let retired = self
            .retired_client_playback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = self.load();
        SharedPlaybackDiagnostics {
            underruns: self.playback_underruns.load(Ordering::Acquire),
            last_underrun_sequence: self.last_playback_underrun_sequence.load(Ordering::Relaxed),
            last_underrun_nanos: self.last_playback_underrun_nanos.load(Ordering::Relaxed),
            last_sequence_lag_periods: self
                .last_playback_sequence_lag_periods
                .load(Ordering::Relaxed),
            max_sequence_lag_periods: self
                .max_playback_sequence_lag_periods
                .load(Ordering::Relaxed),
            expired_playback_periods: retired
                .expired_playback_periods
                .wrapping_add(current.region.client_expired_playback_periods()),
            playback_submit_failures: retired
                .playback_submit_failures
                .wrapping_add(current.region.client_playback_submit_failures()),
            playback_xruns: retired
                .playback_xruns
                .wrapping_add(current.region.client_playback_xruns()),
        }
    }
}

struct SharedPlaybackDiagnostics {
    underruns: u64,
    last_underrun_sequence: u64,
    last_underrun_nanos: u64,
    last_sequence_lag_periods: u64,
    max_sequence_lag_periods: u64,
    expired_playback_periods: u64,
    playback_submit_failures: u64,
    playback_xruns: u64,
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

#[derive(Default)]
struct PlaybackCommitBarrier {
    started: AtomicU64,
    completed: AtomicU64,
}

impl PlaybackCommitBarrier {
    fn begin(&self) {
        self.started.fetch_add(1, Ordering::SeqCst);
    }

    fn end(&self) {
        self.completed.fetch_add(1, Ordering::SeqCst);
    }

    fn wait_for_preexisting(&self) {
        self.wait_for(self.started.load(Ordering::SeqCst));
    }

    fn wait_for(&self, target: u64) {
        while sequence_before(self.completed.load(Ordering::SeqCst), target) {
            std::thread::yield_now();
        }
    }
}

impl SessionState {
    fn new(
        period_frames: u32,
        playback_channels: u32,
        capture_channels: u32,
        slot_count: u32,
        playback_epoch: Option<Arc<AtomicU64>>,
        playback_commits: Option<Arc<PlaybackCommitBarrier>>,
    ) -> Result<Self, SharedError> {
        Ok(Self {
            endpoint: Arc::new(EndpointSlot::new(SessionEndpoint::create(
                0,
                period_frames,
                playback_channels,
                capture_channels,
                slot_count,
            )?)),
            owner: Arc::new(AtomicU64::new(0)),
            active: Arc::new(AtomicU64::new(0)),
            lifecycle_generation: Arc::new(AtomicU64::new(0)),
            lifecycle_hardware_generation: Arc::new(AtomicU64::new(0)),
            armed: Arc::new(AtomicU64::new(0)),
            outage: Arc::new(AtomicU64::new(0)),
            warmup_blocks: Arc::new(AtomicU64::new(0)),
            playback_epoch,
            playback_commits,
            period_frames,
            playback_channels,
            capture_channels,
            slot_count,
        })
    }

    fn try_open(
        &self,
        session_id: u64,
        hardware_generation: u64,
    ) -> Result<Option<(SharedRegionInfo, [std::os::fd::RawFd; 4])>, SharedError> {
        self.try_open_layout(
            session_id,
            hardware_generation,
            self.playback_channels,
            self.capture_channels,
        )
    }

    fn try_open_layout(
        &self,
        session_id: u64,
        hardware_generation: u64,
        playback_channels: u32,
        capture_channels: u32,
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
            playback_channels,
            capture_channels,
            self.slot_count,
        ) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.owner.store(0, Ordering::Release);
                return Err(error);
            }
        };
        endpoint.region.set_hardware_generation(hardware_generation);
        let info = endpoint.info();
        let fds = endpoint.fds();
        self.endpoint.replace(endpoint);
        Ok(Some((info, fds)))
    }

    fn start(&self, session_id: u64, hardware_generation: u64) -> bool {
        if self.owner.load(Ordering::Acquire) != session_id
            || self.active.load(Ordering::SeqCst) != 0
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
        self.lifecycle_hardware_generation
            .store(hardware_generation, Ordering::Release);
        endpoint.region.set_hardware_generation(hardware_generation);
        endpoint.region.reset_activation();
        endpoint.region.set_client_state(SHARED_CLIENT_STARTING);
        if self
            .active
            .compare_exchange(0, session_id, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            endpoint.region.set_client_state(SHARED_CLIENT_IDLE);
            return false;
        }
        self.armed.store(0, Ordering::Release);
        self.outage.store(0, Ordering::Release);
        self.warmup_blocks.store(0, Ordering::Release);
        true
    }

    fn stop(&self, session_id: u64) -> bool {
        if self.active.load(Ordering::SeqCst) != session_id {
            return false;
        }
        self.armed.store(0, Ordering::Release);
        self.outage.store(0, Ordering::Release);
        self.warmup_blocks.store(0, Ordering::Release);
        let stopped = self
            .active
            .compare_exchange(session_id, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if stopped {
            self.bump_playback_epoch();
            let endpoint = self.endpoint.load();
            endpoint.events.notify_playback_ready();
            drop(endpoint);
            self.wait_for_rt_idle();
            self.armed.store(0, Ordering::Release);
            self.outage.store(0, Ordering::Release);
            self.warmup_blocks.store(0, Ordering::Release);
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
        let was_active = self.active.load(Ordering::SeqCst) == session_id;
        if was_active {
            self.armed.store(0, Ordering::Release);
            self.outage.store(0, Ordering::Release);
            self.warmup_blocks.store(0, Ordering::Release);
        }
        let stopped = self
            .active
            .compare_exchange(session_id, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if stopped {
            self.bump_playback_epoch();
        }
        let endpoint = self.endpoint.load();
        endpoint.events.notify_playback_ready();
        drop(endpoint);
        self.wait_for_rt_idle();
        self.armed.store(0, Ordering::Release);
        self.outage.store(0, Ordering::Release);
        self.warmup_blocks.store(0, Ordering::Release);
        let endpoint = self.endpoint.load();
        endpoint.region.reset_slots();
        endpoint.events.drain();
        self.owner.store(0, Ordering::Release);
        true
    }

    fn wait_for_rt_idle(&self) {
        self.endpoint.wait_for_idle();
        if let Some(commits) = &self.playback_commits {
            // The fixed target lets the hardware begin later cycles without extending teardown.
            // SeqCst lifecycle transitions ensure those later cycles observe the stop or epoch.
            commits.wait_for_preexisting();
        }
    }

    fn bump_playback_epoch(&self) {
        if let Some(epoch) = &self.playback_epoch {
            epoch.fetch_add(1, Ordering::SeqCst);
        }
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
        slot_count: u32,
        playback_epoch: Option<Arc<AtomicU64>>,
        playback_commits: Option<Arc<PlaybackCommitBarrier>>,
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
            session: SessionState::new(
                period_frames,
                playback_channels,
                capture_channels,
                slot_count,
                playback_epoch,
                playback_commits,
            )?,
        })
    }
}

enum ProOwnership {
    None,
    Classic(u64),
    Split {
        peer_pid: u32,
        peer_uid: u32,
        token: [u64; 2],
        playback: Option<u64>,
        capture: Option<u64>,
    },
}

pub struct DaemonState {
    // Aligned-start requests awaiting RT activation: bit 0 playback, bit 1 capture.
    pro_pair_start: Arc<AtomicU64>,
    pro_diagnostics: Arc<ProDiagnostics>,
    info: DeviceInfo,
    timeline: Arc<HardwareTimeline>,
    hardware_ready: Arc<AtomicBool>,
    pro: SessionState,
    pro_capture: SessionState,
    pro_ownership: Mutex<ProOwnership>,
    shared: Box<[SharedPortState]>,
    next_session: AtomicU64,
    period_frames: usize,
    playback_channels: usize,
    capture_channels: usize,
    shared_buffer_periods: usize,
    shared_latency_periods: u32,
    shared_playback_repeat_on_underrun: bool,
    playback_epoch: Arc<AtomicU64>,
    playback_commits: Arc<PlaybackCommitBarrier>,
    bridges_created: AtomicBool,
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
        let playback_epoch = Arc::new(AtomicU64::new(0));
        let playback_commits = Arc::new(PlaybackCommitBarrier::default());
        let pro = SessionState::new(
            profile.device.period_size,
            profile.device.playback.channels,
            profile.device.capture.channels,
            sidealsa_protocol::SHARED_SLOT_COUNT,
            Some(Arc::clone(&playback_epoch)),
            Some(Arc::clone(&playback_commits)),
        )?;
        let mut shared =
            Vec::with_capacity(profile.ports.playback.len() + profile.ports.capture.len());
        for port in &profile.ports.playback {
            shared.push(SharedPortState::new(
                port,
                PortDirection::Playback,
                profile.device.period_size,
                sidealsa_protocol::SHARED_SLOT_COUNT,
                Some(Arc::clone(&playback_epoch)),
                Some(Arc::clone(&playback_commits)),
            )?);
        }
        for port in &profile.ports.capture {
            // shared_buffer_size remains the base transport geometry. Capture gets
            // automatic storage reserve, not a larger steady-state latency target.
            let slot_count = (shared_buffer_periods.saturating_mul(2)).clamp(4, 16) as u32;
            shared.push(SharedPortState::new(
                port,
                PortDirection::Capture,
                profile.device.period_size,
                slot_count,
                None,
                None,
            )?);
        }
        Ok(Self {
            pro_pair_start: Arc::new(AtomicU64::new(0)),
            info: device_info(profile),
            pro_diagnostics: Arc::new(ProDiagnostics::default()),
            timeline,
            hardware_ready: Arc::new(AtomicBool::new(false)),
            pro,
            pro_capture: SessionState::new(
                profile.device.period_size,
                0,
                profile.device.capture.channels,
                sidealsa_protocol::SHARED_SLOT_COUNT,
                None,
                None,
            )?,
            pro_ownership: Mutex::new(ProOwnership::None),
            shared: shared.into_boxed_slice(),
            next_session: AtomicU64::new(1),
            period_frames,
            playback_channels,
            capture_channels,
            shared_buffer_periods,
            shared_latency_periods: profile.device.shared_latency_periods,
            shared_playback_repeat_on_underrun: profile.device.shared_playback_repeat_on_underrun,
            playback_epoch,
            playback_commits,
            bridges_created: AtomicBool::new(false),
        })
    }

    pub fn info(&self) -> DeviceInfo {
        self.info.clone()
    }

    /// Enable before starting the direct-duplex hardware loop.
    pub fn enable_pro_diagnostics(&self) {
        self.pro_diagnostics.enabled.store(true, Ordering::Relaxed);
    }

    pub fn pro_diagnostics(&self) -> Option<ProDiagnosticsSnapshot> {
        self.pro_diagnostics.snapshot()
    }

    pub fn stats(&self) -> Stats {
        // Diagnostics are non-RT; hold group membership stable while combining
        // the independently owned input/output client counters.
        let ownership = self
            .pro_ownership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let endpoint = self.pro.endpoint.load();
        let input = self.pro_capture.endpoint.load();
        let (capture_owned, capture_only) = match &*ownership {
            ProOwnership::Split {
                playback,
                capture: Some(id),
                ..
            } if *id == input.session_id => (true, playback.is_none()),
            _ => (false, false),
        };
        let shared_playback_ports = self
            .shared
            .iter()
            .filter(|port| port.direction == PortDirection::Playback)
            .map(|port| {
                let diagnostics = port.session.endpoint.playback_diagnostics();
                SharedPlaybackPortStats {
                    port_id: port.id.to_string(),
                    underruns: diagnostics.underruns,
                    last_underrun_sequence: diagnostics.last_underrun_sequence,
                    last_underrun_nanos: diagnostics.last_underrun_nanos,
                    last_sequence_lag_periods: diagnostics.last_sequence_lag_periods,
                    max_sequence_lag_periods: diagnostics.max_sequence_lag_periods,
                    expired_playback_periods: diagnostics.expired_playback_periods,
                    playback_submit_failures: diagnostics.playback_submit_failures,
                    playback_xruns: diagnostics.playback_xruns,
                }
            })
            .collect();
        let mut stats = stats_from_core(
            self.timeline.snapshot(),
            if capture_only {
                &input.region
            } else {
                &endpoint.region
            },
            shared_playback_ports,
        );
        if capture_owned && !capture_only {
            stats.pro_expired_capture_blocks = stats
                .pro_expired_capture_blocks
                .saturating_add(input.region.client_expired_capture_blocks());
            stats.pro_realtime_failures = stats
                .pro_realtime_failures
                .saturating_add(input.region.client_realtime_failures());
            stats.pro_callback_overruns = stats
                .pro_callback_overruns
                .saturating_add(input.region.client_callback_overruns());
            stats.pro_callback_max_nanos = stats
                .pro_callback_max_nanos
                .max(input.region.client_callback_max_nanos());
        }
        stats
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
        let mut ownership = self
            .pro_ownership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(*ownership, ProOwnership::None) {
            return Ok(None);
        }
        let session_id = self.next_session_id();
        let opened = self.pro.try_open(session_id, self.timeline.generation())?;
        if opened.is_some() {
            *ownership = ProOwnership::Classic(session_id);
        }
        Ok(opened.map(|(shared, fds)| (session_id, shared, fds)))
    }

    /// Credentials must come from SO_PEERCRED on the owning control connection.
    pub fn open_pro_direction(
        &self,
        peer_pid: u32,
        peer_uid: u32,
        token: [u64; 2],
        direction: PortDirection,
    ) -> Result<Option<(u64, SharedRegionInfo, [std::os::fd::RawFd; 4])>, SharedError> {
        let mut ownership = self
            .pro_ownership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if token == [0; 2] || peer_pid == 0 {
            return Ok(None);
        }
        match &*ownership {
            ProOwnership::None => {}
            ProOwnership::Split {
                peer_pid: pid,
                peer_uid: uid,
                token: capability,
                playback,
                capture,
            } if *pid == peer_pid
                && *uid == peer_uid
                && *capability == token
                && match direction {
                    PortDirection::Playback => playback.is_none(),
                    PortDirection::Capture => capture.is_none(),
                } => {}
            _ => return Ok(None),
        }
        let session_id = self.next_session_id();
        let opened = match direction {
            PortDirection::Playback => self.pro.try_open_layout(
                session_id,
                self.timeline.generation(),
                self.pro.playback_channels,
                0,
            )?,
            PortDirection::Capture => self
                .pro_capture
                .try_open(session_id, self.timeline.generation())?,
        };
        if opened.is_some() {
            if matches!(*ownership, ProOwnership::None) {
                *ownership = ProOwnership::Split {
                    peer_pid,
                    peer_uid,
                    token,
                    playback: None,
                    capture: None,
                };
            }
            if let ProOwnership::Split {
                playback, capture, ..
            } = &mut *ownership
            {
                *match direction {
                    PortDirection::Playback => playback,
                    PortDirection::Capture => capture,
                } = Some(session_id);
            }
        }
        Ok(opened.map(|(shared, fds)| (session_id, shared, fds)))
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
            .try_open(session_id, self.timeline.generation())?
            .map(|(shared, fds)| SharedOpen {
                session_id,
                direction: port.direction,
                shared,
                fds,
            }))
    }

    pub fn start(&self, session_id: u64) -> bool {
        if session_id == 0 || session_id == SESSION_CLOSING {
            return false;
        }
        let _ownership = self
            .pro_ownership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let hardware_generation = self.timeline.generation();
        if self.pro_capture.owner.load(Ordering::Acquire) == session_id {
            return self.pro_capture.start(session_id, hardware_generation);
        }
        if self.pro.owner.load(Ordering::Acquire) == session_id {
            if self.pro.active.load(Ordering::SeqCst) != 0 {
                return false;
            }
            return self.pro.start(session_id, hardware_generation);
        }
        self.shared
            .iter()
            .any(|port| port.session.start(session_id, hardware_generation))
    }

    pub fn start_pro_aligned(&self, session: u64) -> bool {
        let ownership = self
            .pro_ownership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ProOwnership::Split {
            playback: p,
            capture: c,
            ..
        } = *ownership
        else {
            return false;
        };
        let (bit, state) = if p == Some(session) {
            (1, &self.pro)
        } else if c == Some(session) {
            (2, &self.pro_capture)
        } else {
            return false;
        };
        if state.active.load(Ordering::SeqCst) != 0 {
            return false;
        }
        self.pro_pair_start.fetch_or(bit, Ordering::Release);
        if !state.start(session, self.timeline.generation()) {
            self.pro_pair_start.fetch_and(!bit, Ordering::Release);
            return false;
        }
        true
    }

    pub fn stop(&self, session_id: u64) -> bool {
        if session_id == 0 || session_id == SESSION_CLOSING {
            return false;
        }
        let _ownership = self
            .pro_ownership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.pro_capture.stop(session_id) {
            self.pro_pair_start.fetch_and(!2, Ordering::Release);
            return true;
        }
        if self.pro.stop(session_id) {
            self.pro_pair_start.fetch_and(!1, Ordering::Release);
            return true;
        }
        self.shared.iter().any(|port| port.session.stop(session_id))
    }

    pub fn close(&self, session_id: u64) -> bool {
        if session_id == 0 || session_id == SESSION_CLOSING {
            return false;
        }
        let mut ownership = self
            .pro_ownership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.pro.close(session_id) || self.pro_capture.close(session_id) {
            match &mut *ownership {
                ProOwnership::Classic(id) if *id == session_id => *ownership = ProOwnership::None,
                ProOwnership::Split {
                    playback, capture, ..
                } => {
                    if *playback == Some(session_id) {
                        self.pro_pair_start.fetch_and(!1, Ordering::Release);
                        *playback = None;
                    }
                    if *capture == Some(session_id) {
                        self.pro_pair_start.fetch_and(!2, Ordering::Release);
                        *capture = None;
                    }
                    if playback.is_none() && capture.is_none() {
                        *ownership = ProOwnership::None;
                    }
                }
                _ => {}
            }
            return true;
        }
        self.shared
            .iter()
            .any(|port| port.session.close(session_id))
    }

    pub fn owns(&self, session_id: u64) -> bool {
        if session_id == 0 || session_id == SESSION_CLOSING {
            return false;
        }
        self.pro.owner.load(Ordering::Acquire) == session_id
            || self.pro_capture.owner.load(Ordering::Acquire) == session_id
            || self
                .shared
                .iter()
                .any(|port| port.session.owner.load(Ordering::Acquire) == session_id)
    }

    pub fn bridges(&self) -> (DaemonCaptureBridge, DaemonPlaybackBridge) {
        assert!(
            self.bridges_created
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "hardware bridges may only be created once"
        );
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
                    port.session.slot_count as usize,
                    self.shared_latency_periods,
                    self.shared_playback_repeat_on_underrun,
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
                    self.shared_playback_repeat_on_underrun,
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let capture = DaemonCaptureBridge {
            pair_start: Arc::clone(&self.pro_pair_start),
            pair_owners: [
                Arc::clone(&self.pro.owner),
                Arc::clone(&self.pro_capture.owner),
            ],
            pair_wait: [None, None],
            directional_endpoint: Arc::clone(&self.pro_capture.endpoint),
            directional_active: Arc::clone(&self.pro_capture.active),
            directional_hardware_generation: Arc::clone(
                &self.pro_capture.lifecycle_hardware_generation,
            ),
            directional_capture_index: 0,
            pro_hardware_generation: Arc::clone(&self.pro.lifecycle_hardware_generation),
            pro_endpoint: Arc::clone(&self.pro.endpoint),
            pro_active: Arc::clone(&self.pro.active),
            pro_capture_index: 0,
            shared: capture_shared,
            timeline: Arc::clone(&self.timeline),
        };
        let playback = DaemonPlaybackBridge {
            pro_diagnostics: Arc::clone(&self.pro_diagnostics),
            pro_wait_timing: None,
            pro_endpoint: Arc::clone(&self.pro.endpoint),
            pro_active: Arc::clone(&self.pro.active),
            pro_gate: ProPlaybackGate {
                lifecycle_generation: Arc::clone(&self.pro.lifecycle_generation),
                lifecycle_hardware_generation: Arc::clone(&self.pro.lifecycle_hardware_generation),
                armed: Arc::clone(&self.pro.armed),
                warmup_blocks: Arc::clone(&self.pro.warmup_blocks),
            },
            playback_epoch: Arc::clone(&self.playback_epoch),
            playback_commits: Arc::clone(&self.playback_commits),
            shared: playback_shared,
            prepared_shared_sequence: None,
            last_valid_pro: vec![0; self.period_frames * self.playback_channels].into_boxed_slice(),
            last_valid_pro_identity: None,
            core_miss_pro_sequence: None,
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
    pair_start: Arc<AtomicU64>,
    pair_owners: [Arc<AtomicU64>; 2],
    pair_wait: [Option<(u64, u64, u64)>; 2],
    directional_endpoint: Arc<EndpointSlot>,
    directional_active: Arc<AtomicU64>,
    directional_hardware_generation: Arc<AtomicU64>,
    directional_capture_index: usize,
    pro_hardware_generation: Arc<AtomicU64>,
    pro_endpoint: Arc<EndpointSlot>,
    pro_active: Arc<AtomicU64>,
    pro_capture_index: usize,
    shared: Box<[SharedAudioPortBridge]>,
    timeline: Arc<HardwareTimeline>,
}

pub struct DaemonPlaybackBridge {
    pro_diagnostics: Arc<ProDiagnostics>,
    // Identity, sequence, cutoff, budget at entry to the direct client wait.
    pro_wait_timing: Option<(ProPlaybackIdentity, u64, u64, u64)>,
    pro_endpoint: Arc<EndpointSlot>,
    pro_active: Arc<AtomicU64>,
    pro_gate: ProPlaybackGate,
    playback_epoch: Arc<AtomicU64>,
    playback_commits: Arc<PlaybackCommitBarrier>,
    shared: Box<[SharedAudioPortBridge]>,
    prepared_shared_sequence: Option<u64>,
    last_valid_pro: Box<[i32]>,
    last_valid_pro_identity: Option<ProPlaybackIdentity>,
    core_miss_pro_sequence: Option<u64>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SharedPlaybackIdentity {
    session_id: u64,
    lifecycle_generation: u64,
    hardware_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProPlaybackIdentity {
    session_id: u64,
    lifecycle_generation: u64,
    hardware_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreparedSharedPlayback {
    sequence: u64,
    identity: SharedPlaybackIdentity,
    fresh: bool,
}

struct ProPlaybackGate {
    lifecycle_generation: Arc<AtomicU64>,
    lifecycle_hardware_generation: Arc<AtomicU64>,
    armed: Arc<AtomicU64>,
    warmup_blocks: Arc<AtomicU64>,
}

#[derive(Clone, Copy)]
struct ProPlaybackGateState {
    hardware_generation: u64,
    blocked: bool,
}

impl ProPlaybackGate {
    fn synchronize(
        &self,
        endpoint: &SessionEndpoint,
        active: &AtomicU64,
        timeline: &HardwareTimeline,
        sequence: u64,
    ) -> ProPlaybackGateState {
        let hardware_generation = timeline.generation();
        endpoint.region.set_hardware_generation(hardware_generation);
        endpoint.region.set_playback_sequence(sequence);

        let session_id = active.load(Ordering::SeqCst);
        let blocked = session_id != 0
            && endpoint.session_id == session_id
            && self.lifecycle_hardware_generation.load(Ordering::Acquire) != hardware_generation;
        if blocked {
            let _ = self
                .armed
                .compare_exchange(session_id, 0, Ordering::AcqRel, Ordering::Acquire);
            self.warmup_blocks.store(0, Ordering::Release);
            endpoint.events.notify_playback();
        }
        ProPlaybackGateState {
            hardware_generation,
            blocked,
        }
    }
}

struct SharedAudioPortBridge {
    endpoint: Arc<EndpointSlot>,
    active: Arc<AtomicU64>,
    lifecycle_generation: Arc<AtomicU64>,
    lifecycle_hardware_generation: Arc<AtomicU64>,
    armed: Arc<AtomicU64>,
    outage: Arc<AtomicU64>,
    channels: Box<[usize]>,
    period_frames: usize,
    physical_channels: usize,
    capture_capacity_slots: usize,
    latency_periods: u64,
    repeat_on_underrun: bool,
    index: usize,
    scratch: Box<[i32]>,
    last_valid_playback: Option<SharedPlaybackIdentity>,
    observed_hardware_generation: Option<u64>,
    prepared: Option<PreparedSharedPlayback>,
}

impl SharedAudioPortBridge {
    fn new(
        port: &SharedPortState,
        period_frames: usize,
        playback_channels: usize,
        capture_channels: usize,
        capture_capacity_slots: usize,
        latency_periods: u32,
        repeat_on_underrun: bool,
    ) -> Self {
        Self {
            endpoint: Arc::clone(&port.session.endpoint),
            active: Arc::clone(&port.session.active),
            lifecycle_generation: Arc::clone(&port.session.lifecycle_generation),
            lifecycle_hardware_generation: Arc::clone(&port.session.lifecycle_hardware_generation),
            armed: Arc::clone(&port.session.armed),
            outage: Arc::clone(&port.session.outage),
            channels: port.channels.clone(),
            period_frames,
            physical_channels: match port.direction {
                PortDirection::Playback => playback_channels,
                PortDirection::Capture => capture_channels,
            },
            capture_capacity_slots,
            latency_periods: u64::from(latency_periods),
            repeat_on_underrun,
            index: 0,
            scratch: vec![0; port.logical_samples].into_boxed_slice(),
            last_valid_playback: None,
            observed_hardware_generation: None,
            prepared: None,
        }
    }

    fn prepare_playback(&mut self, sequence: u64, timeline: &HardwareTimeline) {
        if self.prepared.take().is_some_and(|prepared| prepared.fresh) {
            self.last_valid_playback = None;
        }
        let endpoint = self.endpoint.load();
        let hardware_generation = timeline.generation();
        let hardware_generation_changed = self
            .observed_hardware_generation
            .replace(hardware_generation)
            .is_some_and(|previous| previous != hardware_generation);
        if hardware_generation_changed {
            self.last_valid_playback = None;
        }
        endpoint.region.set_hardware_generation(hardware_generation);
        endpoint.region.set_cycle_sequence(sequence);
        let session_id = self.active.load(Ordering::SeqCst);
        if session_id == 0 || endpoint.session_id != session_id {
            self.last_valid_playback = None;
            return;
        }
        let generation = self.lifecycle_generation.load(Ordering::Acquire);
        if self.lifecycle_hardware_generation.load(Ordering::Acquire) != hardware_generation {
            self.last_valid_playback = None;
            let _ = self
                .armed
                .compare_exchange(session_id, 0, Ordering::AcqRel, Ordering::Acquire);
            let _ =
                self.outage
                    .compare_exchange(session_id, 0, Ordering::AcqRel, Ordering::Acquire);
            endpoint.events.notify_playback();
            return;
        }
        if endpoint.region.establish_activation(sequence) {
            self.last_valid_playback = None;
            endpoint.events.notify_playback();
            return;
        }
        if endpoint.region.client_state() == SHARED_CLIENT_STARTING {
            self.last_valid_playback = None;
            endpoint.events.notify_playback();
            return;
        }
        if endpoint.region.client_state() != SHARED_CLIENT_RUNNING {
            self.last_valid_playback = None;
            return;
        }
        let identity = SharedPlaybackIdentity {
            session_id,
            lifecycle_generation: generation,
            hardware_generation,
        };
        let expected_sequence = sequence.wrapping_sub(self.latency_periods);
        let consumed = endpoint
            .region
            .try_consume_playback(expected_sequence, &mut self.scratch);
        let session_is_current = self.active.load(Ordering::SeqCst) == session_id
            && self.lifecycle_generation.load(Ordering::Acquire) == generation
            && endpoint.session_id == session_id
            && timeline.generation() == hardware_generation;
        if consumed && session_is_current {
            self.prepared = Some(PreparedSharedPlayback {
                sequence,
                identity,
                fresh: true,
            });
        } else if !consumed && session_is_current {
            let armed = self.armed.load(Ordering::Acquire) == session_id;
            let outage = self.outage.load(Ordering::Acquire) == session_id;
            if !armed && !outage {
                endpoint.events.notify_playback();
                return;
            }
            let sequence_lag = playback_sequence_lag(
                expected_sequence,
                endpoint.region.client_playback_sequence(),
            );
            self.endpoint
                .record_playback_miss(expected_sequence, sequence_lag, armed);
            if armed {
                timeline.record_shared_underrun();
                let _ =
                    self.armed
                        .compare_exchange(session_id, 0, Ordering::AcqRel, Ordering::Acquire);
                self.outage.store(session_id, Ordering::Release);
            }
            if self.repeat_on_underrun && self.last_valid_playback == Some(identity) {
                self.prepared = Some(PreparedSharedPlayback {
                    sequence,
                    identity,
                    fresh: false,
                });
            } else if self.last_valid_playback != Some(identity) {
                self.last_valid_playback = None;
            }
        } else {
            self.last_valid_playback = None;
        }
        endpoint.events.notify_playback();
    }

    fn commit_prepared(
        &mut self,
        sequence: u64,
        physical: &mut [i32],
        timeline: &HardwareTimeline,
    ) {
        let Some(prepared) = self.prepared.take() else {
            return;
        };
        let endpoint = self.endpoint.load();
        if prepared.sequence != sequence
            || endpoint.session_id != prepared.identity.session_id
            || self.active.load(Ordering::SeqCst) != prepared.identity.session_id
            || self.lifecycle_generation.load(Ordering::Acquire)
                != prepared.identity.lifecycle_generation
            || timeline.generation() != prepared.identity.hardware_generation
            || endpoint.region.client_state() != SHARED_CLIENT_RUNNING
        {
            self.last_valid_playback = None;
            return;
        }
        if prepared.fresh {
            let _ = self.outage.compare_exchange(
                prepared.identity.session_id,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            if self.armed.load(Ordering::Acquire) != prepared.identity.session_id {
                let _ = self.armed.compare_exchange(
                    0,
                    prepared.identity.session_id,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            self.last_valid_playback = Some(prepared.identity);
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
        self.commit_prepared(sequence, physical, timeline);
    }

    fn process_capture(&mut self, sequence: u64, physical: &[i32], timeline: &HardwareTimeline) {
        let endpoint = self.endpoint.load();
        endpoint
            .region
            .set_hardware_generation(timeline.generation());
        endpoint.region.set_cycle_sequence(sequence);
        let session_id = self.active.load(Ordering::SeqCst);
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
    fn activate_pair(&mut self, capture_sequence: u64, playback_sequence: u64) -> u64 {
        if self.pair_start.load(Ordering::Acquire) == 0 {
            return 0;
        }
        // Keep both endpoint guards through clearing the pending bits. Stop/close
        // drain these guards before resetting or reopening a lifecycle, so this
        // activation cannot clear a request belonging to a replacement session.
        let p = self.pro_endpoint.load();
        let c = self.directional_endpoint.load();
        let generation = self.timeline.generation();
        let active = [
            self.pro_active.load(Ordering::SeqCst),
            self.directional_active.load(Ordering::SeqCst),
        ];
        let lifecycle_hardware = [
            self.pro_hardware_generation.load(Ordering::Acquire),
            self.directional_hardware_generation.load(Ordering::Acquire),
        ];
        let mask = self.pair_start.load(Ordering::Acquire);
        let endpoints = [&*p, &*c];
        let eligible = std::array::from_fn::<_, 2, _>(|i| {
            mask & (1 << i) != 0
                && active[i] != 0
                && active[i] == endpoints[i].session_id
                && lifecycle_hardware[i] == generation
        });
        let mut activated = 0;
        for i in 0..2 {
            let bit = 1 << i;
            if mask & bit == 0 {
                self.pair_wait[i] = None;
                continue;
            }
            let endpoint = endpoints[i];
            endpoint.region.set_hardware_generation(generation);
            if !eligible[i] {
                if i == 0 {
                    endpoint.events.notify_playback();
                } else {
                    endpoint.events.notify_capture();
                }
                continue;
            }
            let peer_owner = self.pair_owners[i ^ 1].load(Ordering::Acquire);
            if !eligible[i ^ 1]
                && active[i ^ 1] == 0
                && peer_owner != 0
                && peer_owner != SESSION_CLOSING
            {
                let life = endpoint.region.lifecycle_generation();
                match self.pair_wait[i] {
                    Some((id, previous_life, first))
                        if id == active[i]
                            && previous_life == life
                            && first != capture_sequence => {}
                    _ => {
                        self.pair_wait[i] = Some((active[i], life, capture_sequence));
                        continue;
                    }
                }
            }
            let sequence = if i == 0 {
                playback_sequence
            } else {
                capture_sequence
            };
            endpoint.region.set_cycle_sequence(sequence);
            endpoint.region.establish_activation(sequence);
            if i == 1 {
                endpoint.region.set_client_state(SHARED_CLIENT_RUNNING);
            }
            self.pair_wait[i] = None;
            activated |= bit;
        }
        self.pair_start.fetch_and(!activated, Ordering::Release);
        if activated & 1 != 0 {
            p.events.notify_playback();
        }
        if activated & 2 != 0 {
            c.events.notify_capture();
        }
        activated
    }

    fn publish_pro_capture(&mut self, playback_sequence: u64, capture: &[i32]) {
        let endpoint = self.pro_endpoint.load();
        endpoint
            .region
            .set_hardware_generation(self.timeline.generation());
        endpoint.region.set_cycle_sequence(playback_sequence);
        let session_id = self.pro_active.load(Ordering::SeqCst);
        // Check after active: seeing a newly started session must also observe
        // the preceding pair-start barrier, even if this cycle entered earlier.
        if self.pair_start.load(Ordering::Acquire) & 1 != 0 {
            return;
        }
        if endpoint.info().capture_channels == 0 {
            if session_id != 0 && endpoint.session_id == session_id {
                if self.pro_hardware_generation.load(Ordering::Acquire)
                    == self.timeline.generation()
                    && endpoint.region.client_state() != SHARED_CLIENT_IDLE
                {
                    endpoint.region.establish_activation(playback_sequence);
                }
                // Playback-only clients wake at capture-ready, before the hardware waits.
                endpoint.events.notify_playback();
            }
            return;
        }
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

    fn publish_directional_capture(&mut self, sequence: u64, capture: &[i32]) {
        let endpoint = self.directional_endpoint.load();
        let hardware_generation = self.timeline.generation();
        endpoint.region.set_hardware_generation(hardware_generation);
        endpoint.region.set_cycle_sequence(sequence);
        let session_id = self.directional_active.load(Ordering::SeqCst);
        if self.pair_start.load(Ordering::Acquire) & 2 != 0 {
            return;
        }
        if session_id == 0
            || endpoint.session_id != session_id
            || endpoint.region.client_state() == SHARED_CLIENT_IDLE
        {
            return;
        }
        if self.directional_hardware_generation.load(Ordering::Acquire) != hardware_generation {
            endpoint.events.notify_capture();
            return;
        }
        if endpoint.region.establish_activation(sequence) {
            endpoint.region.set_client_state(SHARED_CLIENT_RUNNING);
        } else if !endpoint.region.try_publish_capture(
            &mut self.directional_capture_index,
            sequence,
            capture,
        ) {
            self.timeline.record_pro_capture_overrun();
            endpoint.region.record_capture_discontinuity();
        }
        endpoint.events.notify_capture();
    }
}

impl ProCaptureSink for DaemonCaptureBridge {
    fn process_capture(&mut self, sequence: u64, capture: &[i32]) {
        let activated = self.activate_pair(sequence, sequence);
        if activated & 1 == 0 {
            self.publish_pro_capture(sequence, capture);
        }
        if activated & 2 == 0 {
            self.publish_directional_capture(sequence, capture);
        }
        self.publish_shared_capture(sequence, capture);
    }

    fn process_capture_for_playback(
        &mut self,
        hardware_sequence: u64,
        playback_sequence: u64,
        capture: &[i32],
    ) {
        let activated = self.activate_pair(hardware_sequence, playback_sequence);
        if activated & 2 == 0 {
            self.publish_directional_capture(hardware_sequence, capture);
        }
        if activated & 1 == 0 {
            self.publish_pro_capture(playback_sequence, capture);
        }
    }

    fn process_deferred_capture(&mut self, hardware_sequence: u64, capture: &[i32]) {
        self.publish_shared_capture(hardware_sequence, capture);
    }
}

impl ProPlaybackSource for DaemonPlaybackBridge {
    fn playback_epoch(&self) -> u64 {
        self.playback_epoch.load(Ordering::SeqCst)
    }

    fn prepare_playback(&mut self, sequence: u64) {
        let endpoint = self.pro_endpoint.load();
        endpoint.events.drain_playback_ready();
        self.pro_gate
            .synchronize(&endpoint, &self.pro_active, &self.timeline, sequence);
    }

    fn prepare_playback_mix(&mut self, sequence: u64) {
        for port in &mut self.shared {
            port.prepare_playback(sequence, &self.timeline);
        }
        self.prepared_shared_sequence = Some(sequence);
    }

    fn wait_for_playback_before(&mut self, sequence: u64, cutoff_nanos: u64) {
        let wait_budget = self
            .pro_diagnostics
            .enabled
            .load(Ordering::Relaxed)
            .then(|| cutoff_nanos.saturating_sub(monotonic_nanos()));
        let wait_started = Instant::now();
        let endpoint = self.pro_endpoint.load();
        let session_id = self.pro_active.load(Ordering::SeqCst);
        let hardware_generation = self.timeline.generation();
        self.pro_wait_timing = wait_budget.map(|budget| {
            (
                ProPlaybackIdentity {
                    session_id,
                    lifecycle_generation: self
                        .pro_gate
                        .lifecycle_generation
                        .load(Ordering::Acquire),
                    hardware_generation,
                },
                sequence,
                cutoff_nanos,
                budget,
            )
        });
        if session_id == 0
            || endpoint.session_id != session_id
            || !endpoint.region.activation_ready()
            || endpoint.region.client_state() == SHARED_CLIENT_IDLE
            || (endpoint.region.client_state() == SHARED_CLIENT_STARTING
                && endpoint.region.start_sequence() == sequence)
            || self
                .pro_gate
                .lifecycle_hardware_generation
                .load(Ordering::Acquire)
                != hardware_generation
        {
            return;
        }

        loop {
            if self.pro_active.load(Ordering::SeqCst) != session_id
                || endpoint.region.client_state() == SHARED_CLIENT_IDLE
                || (endpoint.region.client_state() == SHARED_CLIENT_STARTING
                    && endpoint.region.start_sequence() == sequence)
                || self.timeline.generation() != hardware_generation
                || self
                    .pro_gate
                    .lifecycle_hardware_generation
                    .load(Ordering::Acquire)
                    != hardware_generation
                || endpoint.region.has_ready_playback(sequence)
            {
                break;
            }
            endpoint.events.drain_playback_ready();
            if self.pro_active.load(Ordering::SeqCst) != session_id
                || endpoint.region.client_state() == SHARED_CLIENT_IDLE
                || (endpoint.region.client_state() == SHARED_CLIENT_STARTING
                    && endpoint.region.start_sequence() == sequence)
                || self.timeline.generation() != hardware_generation
                || self
                    .pro_gate
                    .lifecycle_hardware_generation
                    .load(Ordering::Acquire)
                    != hardware_generation
                || endpoint.region.has_ready_playback(sequence)
            {
                break;
            }
            let remaining = cutoff_nanos.saturating_sub(monotonic_nanos());
            if remaining == 0 {
                break;
            }
            match endpoint.events.wait_playback_ready_before(cutoff_nanos) {
                PlaybackReadyWait::Ready | PlaybackReadyWait::Interrupted => {}
                PlaybackReadyWait::Timeout => break,
                PlaybackReadyWait::Failed => {
                    self.core_miss_pro_sequence = Some(sequence);
                    break;
                }
            }
        }
        self.timeline.record_pro_ready_wait(
            u64::try_from(wait_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
        );
    }

    fn mark_playback_budget_exhausted(&mut self, sequence: u64) {
        self.core_miss_pro_sequence = Some(sequence);
    }

    fn begin_playback_commit(&mut self) {
        self.playback_commits.begin();
    }

    fn end_playback_commit(&mut self) {
        self.playback_commits.end();
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
            for port in &mut self.shared {
                port.commit_prepared(sequence, playback, &self.timeline);
            }
        } else {
            for port in &mut self.shared {
                port.process_playback(sequence, playback, &self.timeline);
            }
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
        let diagnostics_enabled = self.pro_diagnostics.enabled.load(Ordering::Relaxed);
        let selection_started_nanos = if diagnostics_enabled {
            monotonic_nanos()
        } else {
            0
        };
        let wait_timing = self.pro_wait_timing.take();
        let endpoint = self.pro_endpoint.load();
        let gate = self
            .pro_gate
            .synchronize(&endpoint, &self.pro_active, &self.timeline, sequence);
        playback.fill(0);
        let core_deadline_miss = self.core_miss_pro_sequence.take() == Some(sequence);
        let session_id = self.pro_active.load(Ordering::SeqCst);
        let identity = ProPlaybackIdentity {
            session_id,
            lifecycle_generation: self.pro_gate.lifecycle_generation.load(Ordering::Acquire),
            hardware_generation: gate.hardware_generation,
        };
        if self.last_valid_pro_identity != Some(identity) {
            self.last_valid_pro_identity = None;
        }
        if !gate.blocked
            && session_id != 0
            && endpoint.session_id == session_id
            && endpoint.region.activation_ready()
            && endpoint.region.client_state() != SHARED_CLIENT_IDLE
        {
            endpoint.events.drain_playback_ready();
            let (outcome, published_nanos) = if self.pro_active.load(Ordering::SeqCst) == session_id
                && endpoint.region.client_state() == SHARED_CLIENT_RUNNING
            {
                match cutoff_nanos {
                    Some(cutoff) => endpoint
                        .region
                        .try_consume_playback_before_with_timestamp(sequence, cutoff, playback),
                    None => (
                        if endpoint.region.try_consume_playback(sequence, playback) {
                            PlaybackConsume::Ready
                        } else {
                            PlaybackConsume::Missing
                        },
                        None,
                    ),
                }
            } else {
                (PlaybackConsume::Missing, None)
            };
            let consumed = outcome == PlaybackConsume::Ready;
            let capture_to_publish_nanos = if diagnostics_enabled {
                published_nanos.and_then(|published| {
                    self.timeline.pro_capture_elapsed_nanos(sequence, published)
                })
            } else {
                None
            };
            let identity_is_current = self.pro_active.load(Ordering::SeqCst) == session_id
                && self.pro_gate.lifecycle_generation.load(Ordering::Acquire)
                    == identity.lifecycle_generation
                && self.timeline.generation() == gate.hardware_generation;
            if consumed && identity_is_current {
                if let Some(elapsed) = capture_to_publish_nanos {
                    self.pro_diagnostics.record_accepted(elapsed);
                }
                self.timeline
                    .record_pro_playback_block(playback.iter().any(|sample| *sample != 0));
                self.last_valid_pro.copy_from_slice(playback);
                self.last_valid_pro_identity = Some(identity);
                if self.pro_gate.armed.load(Ordering::Acquire) != session_id {
                    self.pro_gate.warmup_blocks.store(1, Ordering::Release);
                    let _ = self.pro_gate.armed.compare_exchange(
                        0,
                        session_id,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
            } else if consumed {
                playback.fill(0);
                self.last_valid_pro_identity = None;
            } else if identity_is_current
                && endpoint.region.client_state() == SHARED_CLIENT_RUNNING
                && self.pro_gate.armed.load(Ordering::Acquire) == session_id
            {
                if core_deadline_miss {
                    self.timeline.record_pro_core_deadline_miss();
                } else {
                    self.timeline.record_pro_deadline_miss();
                }
                if diagnostics_enabled {
                    self.pro_diagnostics.record_miss(ProMiss {
                        generation: gate.hardware_generation,
                        session_id,
                        sequence,
                        wait_entry_budget_nanos: wait_timing
                            .filter(|(wait_identity, wait_sequence, cutoff, _)| {
                                *wait_identity == identity
                                    && *wait_sequence == sequence
                                    && Some(*cutoff) == cutoff_nanos
                            })
                            .map(|(_, _, _, budget)| budget),
                        cutoff_nanos,
                        selection_started_nanos,
                        published_nanos,
                        capture_to_publish_nanos,
                        core: core_deadline_miss,
                        late: outcome == PlaybackConsume::Late,
                    });
                }
                if self.last_valid_pro_identity == Some(identity) {
                    playback.copy_from_slice(&self.last_valid_pro);
                }
            } else if endpoint.region.client_state() == SHARED_CLIENT_STARTING {
                self.pro_gate.warmup_blocks.store(0, Ordering::Release);
                self.last_valid_pro_identity = None;
            }
            endpoint.events.notify_playback();
        } else {
            self.last_valid_pro_identity = None;
        }
        endpoint
            .region
            .set_playback_sequence(sequence.wrapping_add(1));
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

fn stats_from_core(
    stats: HardwareStats,
    pro_region: &SharedRegion,
    shared_playback_ports: Vec<SharedPlaybackPortStats>,
) -> Stats {
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
        duplex_pointer_phase_nanos: stats.duplex_pointer_phase_nanos,
        duplex_pointer_phase_min_nanos: stats.duplex_pointer_phase_min_nanos,
        duplex_pointer_phase_max_nanos: stats.duplex_pointer_phase_max_nanos,
        duplex_pointer_phase_samples: stats.duplex_pointer_phase_samples,
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
        shared_playback_ports,
    }
}

fn playback_sequence_lag(expected: u64, last_published: u64) -> u64 {
    let distance = expected.wrapping_sub(last_published);
    if distance < (1_u64 << 63) {
        distance
    } else {
        0
    }
}

fn sequence_before(sequence: u64, target: u64) -> bool {
    sequence != target && target.wrapping_sub(sequence) < (1_u64 << 63)
}

fn monotonic_nanos() -> u64 {
    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timestamp) } != 0 {
        return 0;
    }
    u64::try_from(timestamp.tv_sec)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(timestamp.tv_nsec).unwrap_or(0))
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

    #[test]
    fn aligned_pro_start_coalesces_both_orders_without_losing_capture() {
        for first_capture in [false, true] {
            let state = DaemonState::new(
                &Profile::from_toml(PROFILE).unwrap(),
                Arc::new(HardwareTimeline::default()),
            )
            .unwrap();
            let p = state
                .open_pro_direction(10, 20, [1, 2], PortDirection::Playback)
                .unwrap()
                .unwrap()
                .0;
            let c = state
                .open_pro_direction(10, 20, [1, 2], PortDirection::Capture)
                .unwrap()
                .unwrap()
                .0;
            let mut bridge = state.bridge();
            assert!(state.start_pro_aligned(if first_capture { c } else { p }));
            bridge.process(100, &[1; 8], &mut [0; 8]);
            assert!(!state.pro.current().region.activation_ready());
            assert!(!state.pro_capture.current().region.activation_ready());
            assert!(state.start_pro_aligned(if first_capture { p } else { c }));
            bridge.process(101, &[2; 8], &mut [0; 8]);
            assert_eq!(state.pro.current().region.start_sequence(), 101);
            assert_eq!(state.pro_capture.current().region.start_sequence(), 101);
            assert_eq!(state.pro_capture.current().region.ready_capture_slots(), 0);
            bridge.process(102, &[3; 8], &mut [0; 8]);
            let mut samples = [0; 8];
            assert_eq!(
                state
                    .pro_capture
                    .current()
                    .region
                    .try_client_read_capture(&mut 0, &mut samples),
                Some(102)
            );
            assert_eq!(samples, [3; 8]);
            assert!(!state.start_pro_aligned(p));
            assert!(state.stop(p));
            bridge.process(103, &[4; 8], &mut [0; 8]);
            assert_eq!(state.pro_capture.current().region.ready_capture_slots(), 1);
        }
    }

    #[test]
    fn aligned_capture_starts_alone_after_one_cycle_and_peer_cannot_reset_it() {
        let state = DaemonState::new(
            &Profile::from_toml(PROFILE).unwrap(),
            Arc::new(HardwareTimeline::default()),
        )
        .unwrap();
        let p = state
            .open_pro_direction(10, 20, [1, 2], PortDirection::Playback)
            .unwrap()
            .unwrap()
            .0;
        let c = state
            .open_pro_direction(10, 20, [1, 2], PortDirection::Capture)
            .unwrap()
            .unwrap()
            .0;
        assert!(!state.start_pro_aligned(999));
        let mut bridge = state.bridge();
        assert!(state.start_pro_aligned(c));
        bridge.process(10, &[1; 8], &mut [0; 8]);
        bridge.process(11, &[2; 8], &mut [0; 8]);
        assert_eq!(state.pro_capture.current().region.start_sequence(), 11);
        bridge.process(12, &[3; 8], &mut [0; 8]);
        assert!(state.start_pro_aligned(p));
        bridge.process(13, &[4; 8], &mut [0; 8]);
        assert_eq!(state.pro_capture.current().region.start_sequence(), 11);
        assert_eq!(state.pro.current().region.start_sequence(), 13);
        assert_eq!(state.pro_capture.current().region.ready_capture_slots(), 2);
        assert!(state.stop(c));
        assert!(state.stop(p));
        assert!(state.start_pro_aligned(c));
        bridge.process(20, &[0; 8], &mut [0; 8]);
        assert!(!state.pro_capture.current().region.activation_ready());
        bridge.process(21, &[0; 8], &mut [0; 8]);
        assert_eq!(state.pro_capture.current().region.start_sequence(), 21);
    }

    #[test]
    fn pending_aligned_start_cancels_on_stop_close_and_requires_current_hardware() {
        let state = DaemonState::new(
            &Profile::from_toml(PROFILE).unwrap(),
            Arc::new(HardwareTimeline::default()),
        )
        .unwrap();
        let p = state
            .open_pro_direction(10, 20, [1, 2], PortDirection::Playback)
            .unwrap()
            .unwrap()
            .0;
        let c = state
            .open_pro_direction(10, 20, [1, 2], PortDirection::Capture)
            .unwrap()
            .unwrap()
            .0;
        let mut bridge = state.bridge();
        assert!(state.start_pro_aligned(p));
        bridge.process(100, &[0; 8], &mut [0; 8]);
        assert!(!state.pro.current().region.activation_ready());
        assert!(state.stop(p));
        bridge.process(101, &[0; 8], &mut [0; 8]);
        assert!(!state.pro.current().region.activation_ready());
        // A normal start after cancellation must not inherit the pairing wait.
        assert!(state.start(p));
        bridge.process(102, &[0; 8], &mut [0; 8]);
        assert_eq!(state.pro.current().region.start_sequence(), 102);
        assert!(state.stop(p));

        assert!(state.start_pro_aligned(c));
        bridge.process(103, &[0; 8], &mut [0; 8]);
        assert!(!state.pro_capture.current().region.activation_ready());
        assert!(state.close(c));
        let new_c = state
            .open_pro_direction(10, 20, [1, 2], PortDirection::Capture)
            .unwrap()
            .unwrap()
            .0;
        bridge.process(104, &[0; 8], &mut [0; 8]);
        assert!(!state.pro_capture.current().region.activation_ready());
        assert!(!state.start_pro_aligned(c));
        assert!(state.start_pro_aligned(new_c));
        state
            .pro_capture
            .lifecycle_hardware_generation
            .store(u64::MAX, Ordering::Release);
        for sequence in [105, 106] {
            bridge.process(sequence, &[0; 8], &mut [0; 8]);
            assert!(!state.pro_capture.current().region.activation_ready());
            assert_eq!(state.pro_capture.current().region.ready_capture_slots(), 0);
        }
        assert!(state.stop(new_c));
        assert!(state.start_pro_aligned(new_c));
        bridge.process(107, &[0; 8], &mut [0; 8]);
        assert!(!state.pro_capture.current().region.activation_ready());
        bridge.process(108, &[0; 8], &mut [0; 8]);
        assert_eq!(state.pro_capture.current().region.start_sequence(), 108);
        bridge.process(109, &[42; 8], &mut [0; 8]);
        let mut samples = [0; 8];
        assert_eq!(
            state
                .pro_capture
                .current()
                .region
                .try_client_read_capture(&mut 0, &mut samples),
            Some(109)
        );
        assert_eq!(samples, [42; 8]);
        let stats = state.timeline.snapshot();
        assert_eq!(stats.generation, 0);
        assert_eq!(stats.pro_deadline_misses, 0);
        assert_eq!(stats.hw_playback_xruns, 0);
        assert_eq!(stats.hw_capture_xruns, 0);
    }

    #[test]
    fn directional_groups_require_pid_uid_capability_and_release_only_on_last_close() {
        for first in [PortDirection::Playback, PortDirection::Capture] {
            let state = DaemonState::new(
                &Profile::from_toml(PROFILE).unwrap(),
                Arc::new(HardwareTimeline::default()),
            )
            .unwrap();
            let second = if first == PortDirection::Playback {
                PortDirection::Capture
            } else {
                PortDirection::Playback
            };
            assert!(
                state
                    .open_pro_direction(10, 20, [0, 0], first)
                    .unwrap()
                    .is_none()
            );
            let a = state
                .open_pro_direction(10, 20, [1, 2], first)
                .unwrap()
                .unwrap();
            assert_eq!(
                (a.1.playback_channels, a.1.capture_channels),
                if first == PortDirection::Playback {
                    (2, 0)
                } else {
                    (0, 2)
                }
            );
            assert!(state.open_pro().unwrap().is_none());
            for (pid, uid, token, direction) in [
                (10, 20, [1, 2], first),
                (11, 20, [1, 2], second),
                (10, 21, [1, 2], second),
                (10, 20, [1, 3], second),
            ] {
                assert!(
                    state
                        .open_pro_direction(pid, uid, token, direction)
                        .unwrap()
                        .is_none()
                );
            }
            let b = state
                .open_pro_direction(10, 20, [1, 2], second)
                .unwrap()
                .unwrap();
            assert_ne!(a.0, b.0);
            assert_ne!(a.2, b.2);
            assert!(state.close(a.0));
            assert!(state.owns(b.0));
            assert!(state.open_pro().unwrap().is_none());
            assert!(state.close(b.0));
            let classic = open_pro(&state);
            assert_eq!(
                (classic.1.playback_channels, classic.1.capture_channels),
                (2, 2)
            );
            assert!(
                state
                    .open_pro_direction(10, 20, [1, 2], first)
                    .unwrap()
                    .is_none()
            );
            assert!(state.close(classic.0));
        }
    }

    #[test]
    fn directional_lifecycles_capture_overflow_and_playback_close_are_independent() {
        let timeline = Arc::new(HardwareTimeline::default());
        let state =
            DaemonState::new(&Profile::from_toml(PROFILE).unwrap(), Arc::clone(&timeline)).unwrap();
        let p = state
            .open_pro_direction(10, 20, [1, 2], PortDirection::Playback)
            .unwrap()
            .unwrap();
        let c = state
            .open_pro_direction(10, 20, [1, 2], PortDirection::Capture)
            .unwrap()
            .unwrap();
        assert!(state.start(c.0));
        assert!(state.start(p.0));
        let mut bridge = state.bridge();
        bridge.capture.process_capture_for_playback(10, 10, &[3; 8]);
        assert!(read_event(p.2[2]).is_ok());
        assert!(read_event(p.2[1]).is_err());
        assert!(read_event(c.2[1]).is_ok());
        assert_eq!(
            state.pro_capture.current().region.client_state(),
            SHARED_CLIENT_RUNNING
        );
        assert!(state.stop(p.0));
        assert!(state.start(p.0));
        bridge.capture.process_capture_for_playback(11, 11, &[4; 8]);
        assert_eq!(state.pro_capture.current().region.ready_capture_slots(), 1);
        let c_generation = state
            .pro_capture
            .lifecycle_generation
            .load(Ordering::Acquire);
        assert!(state.close(p.0));
        let mut output = [99; 8];
        for sequence in 12..24 {
            bridge.process(sequence, &[5; 8], &mut output);
            assert_eq!(output, [0; 8]);
        }
        assert_eq!(
            state
                .pro_capture
                .lifecycle_generation
                .load(Ordering::Acquire),
            c_generation
        );
        assert_eq!(timeline.snapshot().pro_deadline_misses, 0);
        assert!(timeline.snapshot().pro_capture_overruns > 0);
        assert!(state.pro_capture.current().region.capture_discontinuities() > 0);
        read_event(c.2[1]).unwrap();
        bridge.capture.process_capture(24, &[6; 8]);
        assert!(read_event(c.2[1]).is_ok(), "overflow must wake capture");
        let epoch = state.playback_epoch.load(Ordering::Acquire);
        assert!(state.stop(c.0));
        assert!(state.start(c.0));
        assert_eq!(state.playback_epoch.load(Ordering::Acquire), epoch);
        assert_eq!(state.pro_capture.current().region.ready_capture_slots(), 0);
        bridge.capture.process_capture(25, &[6; 8]);
        bridge.capture.process_capture(26, &[6; 8]);
        assert_eq!(state.pro_capture.current().region.ready_capture_slots(), 1);
        assert_eq!(timeline.generation(), 0);
    }

    #[test]
    fn directional_capture_close_does_not_reset_playback_and_stale_maps_are_isolated() {
        let state = DaemonState::new(
            &Profile::from_toml(PROFILE).unwrap(),
            Arc::new(HardwareTimeline::default()),
        )
        .unwrap();
        let p = state
            .open_pro_direction(10, 20, [1, 2], PortDirection::Playback)
            .unwrap()
            .unwrap();
        let c = state
            .open_pro_direction(10, 20, [1, 2], PortDirection::Capture)
            .unwrap()
            .unwrap();
        let stale_p = SharedRegion::map_fd(duplicate_fd(p.2[0]).into_raw_fd(), p.1).unwrap();
        let stale_c = SharedRegion::map_fd(duplicate_fd(c.2[0]).into_raw_fd(), c.1).unwrap();
        assert!(state.start(p.0));
        assert!(state.start(c.0));
        let mut bridge = state.bridge();
        bridge.capture.process_capture(10, &[0; 8]);
        let mut index = 0;
        let mut output = [0; 8];
        for sequence in [11, 12] {
            assert!(stale_p.try_client_publish_playback(&mut index, sequence, &[7; 8]));
            bridge.playback.process_playback(sequence, &mut output);
            assert_eq!(output, [7; 8]);
        }
        let epoch = state.playback_epoch.load(Ordering::Acquire);
        assert!(state.stop(c.0));
        assert!(state.start(c.0));
        assert!(state.close(c.0));
        assert_eq!(state.playback_epoch.load(Ordering::Acquire), epoch);
        assert!(stale_p.try_client_publish_playback(&mut index, 13, &[8; 8]));
        bridge.playback.process_playback(13, &mut output);
        assert_eq!(output, [8; 8]);
        assert!(state.close(p.0));
        let new_p = state
            .open_pro_direction(10, 20, [3, 4], PortDirection::Playback)
            .unwrap()
            .unwrap();
        let new_c = state
            .open_pro_direction(10, 20, [3, 4], PortDirection::Capture)
            .unwrap()
            .unwrap();
        assert!(state.start(new_p.0));
        assert!(state.start(new_c.0));
        stale_c.set_client_state(SHARED_CLIENT_IDLE);
        stale_p.set_client_state(SHARED_CLIENT_RUNNING);
        assert!(stale_p.try_client_publish_playback(&mut index, 15, &[99; 8]));
        bridge.process(14, &[1; 8], &mut output);
        bridge.process(15, &[2; 8], &mut output);
        assert_eq!(output, [0; 8]);
        assert_eq!(state.pro_capture.current().region.ready_capture_slots(), 1);
        assert!(!state.stop(p.0));
        assert!(!state.close(c.0));
    }

    #[test]
    fn directional_hardware_generation_mismatch_requires_independent_restart() {
        let state = DaemonState::new(
            &Profile::from_toml(PROFILE).unwrap(),
            Arc::new(HardwareTimeline::default()),
        )
        .unwrap();
        let p = state
            .open_pro_direction(10, 20, [1, 2], PortDirection::Playback)
            .unwrap()
            .unwrap();
        let c = state
            .open_pro_direction(10, 20, [1, 2], PortDirection::Capture)
            .unwrap()
            .unwrap();
        assert!(state.start(p.0));
        assert!(state.start(c.0));
        let mut bridge = state.bridge();
        bridge.capture.process_capture(10, &[1; 8]);
        state
            .pro
            .lifecycle_hardware_generation
            .store(u64::MAX, Ordering::Release);
        state
            .pro_capture
            .lifecycle_hardware_generation
            .store(u64::MAX, Ordering::Release);
        let mut output = [99; 8];
        bridge.process(11, &[2; 8], &mut output);
        assert_eq!(output, [0; 8]);
        assert_eq!(state.pro_capture.current().region.ready_capture_slots(), 0);
        assert!(read_event(c.2[1]).is_ok());
        assert!(state.stop(c.0));
        assert!(state.start(c.0));
        bridge.process(12, &[3; 8], &mut output);
        bridge.process(13, &[4; 8], &mut output);
        assert_eq!(state.pro_capture.current().region.ready_capture_slots(), 1);
        assert_eq!(
            state
                .pro
                .lifecycle_hardware_generation
                .load(Ordering::Acquire),
            u64::MAX
        );
        assert!(state.stop(p.0));
        assert!(state.start(p.0));
        bridge.capture.process_capture(14, &[5; 8]);
        let mut index = 0;
        assert!(
            state
                .pro
                .current()
                .region
                .try_client_publish_playback(&mut index, 15, &[8; 8])
        );
        bridge.playback.process_playback(15, &mut output);
        assert_eq!(output, [8; 8]);
        assert_eq!(state.timeline.snapshot().pro_deadline_misses, 0);
    }

    #[test]
    fn only_shared_capture_reserves_slots_and_reopen_preserves_geometry() {
        for (base_periods, capture_slots) in [(2, 4), (4, 8), (8, 16)] {
            let mut profile = Profile::from_toml(PROFILE).unwrap();
            profile.device.shared_buffer_size = Some(profile.device.period_size * base_periods);
            let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default())).unwrap();
            assert_eq!(state.pro.current().info().slot_count, 8);
            assert_eq!(state.shared[0].session.current().info().slot_count, 8);
            assert_eq!(
                state.shared[1].session.current().info().slot_count,
                capture_slots
            );
            let (capture, _) = state.bridges();
            assert_eq!(
                capture.shared[0].capture_capacity_slots,
                capture_slots as usize
            );
            for _ in 0..2 {
                let (pro, info, _) = open_pro(&state);
                assert_eq!(info.slot_count, 8);
                assert!(state.close(pro));
                for (port, slots) in [("line1", 8), ("mic1", capture_slots)] {
                    let opened = state.open_shared(port).unwrap().unwrap();
                    assert_eq!(opened.shared.slot_count, slots);
                    assert!(state.close(opened.session_id));
                }
            }
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
    fn playback_lag_uses_wrapping_sequence_order() {
        assert_eq!(playback_sequence_lag(103, 102), 1);
        assert_eq!(playback_sequence_lag(0, u64::MAX), 1);
        assert_eq!(playback_sequence_lag(u64::MAX, 0), 0);
    }

    #[test]
    fn opening_session_seeds_current_hardware_generation() {
        let session =
            SessionState::new(4, 2, 2, 8, None, None).expect("session state should create");

        session
            .try_open(7, 23)
            .expect("session resources should create")
            .expect("session should open");

        assert_eq!(session.current().region.hardware_generation(), 23);
    }

    #[test]
    fn playback_epoch_changes_only_when_active_output_is_removed() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let initial = state.playback_epoch.load(Ordering::Acquire);

        let pro = open_pro(&state).0;
        assert!(state.start(pro));
        assert_eq!(state.playback_epoch.load(Ordering::Acquire), initial);
        assert!(state.stop(pro));
        assert_eq!(
            state.playback_epoch.load(Ordering::Acquire),
            initial.wrapping_add(1)
        );
        assert!(state.close(pro));
        assert_eq!(
            state.playback_epoch.load(Ordering::Acquire),
            initial.wrapping_add(1)
        );

        let active_close = open_pro(&state).0;
        assert!(state.start(active_close));
        assert!(state.close(active_close));
        assert_eq!(
            state.playback_epoch.load(Ordering::Acquire),
            initial.wrapping_add(2)
        );

        let capture = state
            .open_shared("mic1")
            .expect("port should exist")
            .expect("capture should open");
        assert!(state.start(capture.session_id));
        assert!(state.stop(capture.session_id));
        assert_eq!(
            state.playback_epoch.load(Ordering::Acquire),
            initial.wrapping_add(2)
        );

        let playback = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("playback should open");
        assert!(state.start(playback.session_id));
        assert!(state.stop(playback.session_id));
        assert_eq!(
            state.playback_epoch.load(Ordering::Acquire),
            initial.wrapping_add(3)
        );
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
        assert_eq!(output, [11; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 1);
        assert_eq!(state.pro_diagnostics().unwrap().misses_recorded, 0);
    }

    #[test]
    fn pro_diagnostics_distinguish_late_missing_and_unknown_wait_budget() {
        let profile = Profile::from_toml(PROFILE).unwrap();
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).unwrap();
        state.enable_pro_diagnostics();
        let session = open_pro(&state).0;
        assert!(state.start(session));
        activate_session(&state, session, 9);
        let (_, mut playback) = state.bridges();
        let mut producer_index = 0;
        let mut output = [0; 8];
        playback.prepare_playback(10);
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            10,
            &[10; 8]
        ));
        playback.process_playback_before(10, u64::MAX, &mut output);
        assert!(state.pro_diagnostics().unwrap().last_miss.is_none());

        playback.wait_for_playback_before(11, 0);
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            11,
            &[11; 8]
        ));
        playback.mark_playback_budget_exhausted(11);
        playback.process_playback_before(11, 0, &mut output);
        let late = state.pro_diagnostics().unwrap().last_miss.unwrap();
        assert_eq!(late.sequence, 11);
        assert_eq!(late.session_id, session);
        assert_eq!(late.wait_entry_budget_nanos, Some(0));
        assert!(late.late && late.core);
        assert!(late.published_nanos.is_some());
        assert_eq!(late.capture_to_publish_nanos, None);
        assert_eq!(output, [10; 8]);

        let cutoff = monotonic_nanos().saturating_add(10_000);
        playback.wait_for_playback_before(12, cutoff);
        playback.process_playback_before(12, cutoff, &mut output);
        let missing = state.pro_diagnostics().unwrap().last_miss.unwrap();
        assert_eq!(missing.sequence, 12);
        assert!(missing.wait_entry_budget_nanos.is_some());
        assert!(!missing.late && !missing.core);
        assert_eq!(missing.published_nanos, None);

        playback.process_playback_before(13, cutoff, &mut output);
        let snapshot = state.pro_diagnostics().unwrap();
        assert_eq!(snapshot.misses_recorded, 3);
        assert_eq!(snapshot.last_miss.unwrap().wait_entry_budget_nanos, None);
        let stats = timeline.snapshot();
        assert_eq!(stats.pro_core_deadline_misses, 1);
        assert_eq!(stats.pro_client_deadline_misses, 2);
        assert_eq!(stats.hw_playback_xruns, 0);
        assert_eq!(stats.generation, 0);

        for mismatch in 0..5 {
            let sequence = 14 + mismatch;
            let mut identity = playback.last_valid_pro_identity.unwrap();
            match mismatch {
                0 => identity.session_id += 1,
                1 => identity.lifecycle_generation += 1,
                2 => identity.hardware_generation += 1,
                _ => {}
            }
            let wait_sequence = sequence + u64::from(mismatch == 3);
            let wait_cutoff = cutoff + u64::from(mismatch == 4);
            playback.pro_wait_timing = Some((identity, wait_sequence, wait_cutoff, 123));
            playback.process_playback_before(sequence, cutoff, &mut output);
            let miss = state.pro_diagnostics().unwrap().last_miss.unwrap();
            assert_eq!(miss.wait_entry_budget_nanos, None);
            assert_eq!(miss.sequence, sequence);
        }
    }

    #[test]
    fn exhausted_hardware_budget_is_not_charged_to_client() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let session = open_pro(&state).0;
        assert!(state.start(session));
        activate_session(&state, session, 9);

        let (_, mut playback) = state.bridges();
        let mut producer_index = 0;
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            10,
            &[10; 8],
        ));
        let mut output = [0; 8];
        playback.process_playback(10, &mut output);

        playback.mark_playback_budget_exhausted(11);
        playback.process_playback(11, &mut output);

        assert_eq!(output, [10; 8]);
        let stats = timeline.snapshot();
        assert_eq!(stats.pro_deadline_misses, 1);
        assert_eq!(stats.pro_client_deadline_misses, 0);
        assert_eq!(stats.pro_core_deadline_misses, 1);
    }

    #[test]
    fn playback_ready_event_releases_direct_pro_wait() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let (session, _, fds) = open_pro(&state);
        assert!(state.start(session));
        activate_session(&state, session, 9);

        let (_, mut playback) = state.bridges();
        playback.prepare_playback(10);
        let started = Instant::now();
        let (published_tx, published_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(10));
                let mut producer_index = 0;
                assert!(state.pro.current().region.try_client_publish_playback(
                    &mut producer_index,
                    10,
                    &[10; 8],
                ));
                published_tx.send(()).expect("publish signal should send");
                notify_fd(fds[3]);
            });
            playback.wait_for_playback_before(10, monotonic_nanos().saturating_add(1_000_000_000));
            published_rx
                .try_recv()
                .expect("wait should not return before first playback publication");
        });

        let mut output = [0; 8];
        playback.process_playback(10, &mut output);
        assert_eq!(output, [10; 8]);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn starting_pro_does_not_wait_for_the_unpublishable_activation_block() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let session = open_pro(&state).0;
        assert!(state.start(session));
        activate_session(&state, session, 9);

        let (_, mut playback) = state.bridges();
        let started = Instant::now();
        playback.wait_for_playback_before(9, monotonic_nanos().saturating_add(1_000_000_000));

        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn stopping_pro_wakes_direct_wait() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let session = open_pro(&state).0;
        assert!(state.start(session));
        activate_session(&state, session, 9);
        state
            .pro
            .current()
            .region
            .set_client_state(SHARED_CLIENT_RUNNING);

        let (_, mut playback) = state.bridges();
        playback.prepare_playback(10);
        let started = Instant::now();
        std::thread::scope(|scope| {
            let stop = scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(10));
                state.stop(session)
            });
            playback.wait_for_playback_before(10, monotonic_nanos().saturating_add(1_000_000_000));
            assert!(started.elapsed() >= Duration::from_millis(5));
            assert!(stop.join().expect("stop thread should not panic"));
        });

        assert!(started.elapsed() < Duration::from_millis(500));
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
        let inflight = state.pro.endpoint.load();
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
        drop(inflight);
        assert!(
            done_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("stop should finish")
        );
        worker.join().expect("stop worker should not panic");
    }

    #[test]
    fn stop_waits_for_inflight_hardware_playback_commit() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = Arc::new(
            DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
                .expect("state should create"),
        );
        let session = open_pro(&state).0;
        assert!(state.start(session));
        let (_, mut playback) = state.bridges();
        playback.begin_playback_commit();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let stop_state = Arc::clone(&state);
        let worker = std::thread::spawn(move || {
            done_tx
                .send(stop_state.stop(session))
                .expect("stop result should send");
        });

        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        playback.end_playback_commit();
        assert!(
            done_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("stop should finish")
        );
        worker.join().expect("stop worker should not panic");
    }

    #[test]
    fn playback_commit_barrier_does_not_wait_for_later_cycles() {
        let barrier = PlaybackCommitBarrier::default();
        barrier.begin();
        let target = barrier.started.load(Ordering::SeqCst);
        barrier.end();
        barrier.begin();
        barrier.end();
        barrier.begin();

        barrier.wait_for(target);
        assert_eq!(barrier.started.load(Ordering::SeqCst), 3);
        assert_eq!(barrier.completed.load(Ordering::SeqCst), 2);
        barrier.end();
    }

    #[test]
    fn endpoint_drain_does_not_wait_for_later_readers() {
        let endpoint = Arc::new(EndpointSlot::new(
            SessionEndpoint::create(0, 4, 2, 2, 8).expect("endpoint should create"),
        ));
        let earlier = endpoint.load();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiting = Arc::clone(&endpoint);
        let worker = std::thread::spawn(move || {
            waiting.wait_for_idle();
            done_tx.send(()).expect("completion should send");
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while endpoint.reader_epoch.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "reader epoch should advance");
            std::thread::yield_now();
        }
        let later = endpoint.load();

        drop(earlier);
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("later reader must not extend the drain");
        drop(later);
        worker.join().expect("drain worker should not panic");
    }

    #[test]
    fn hardware_bridges_have_one_playback_owner() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let _ = state.bridges();

        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.bridges())).is_err()
        );
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
    fn shared_playback_client_diagnostics_survive_endpoint_replacement() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let first = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        {
            let endpoint = state.shared[0].session.current();
            endpoint.region.record_client_expired_playback_periods(3);
            endpoint.region.record_client_playback_xrun();
            let mut producer_index = 0;
            for sequence in 0..u64::from(sidealsa_protocol::SHARED_SLOT_COUNT) {
                assert!(endpoint.region.try_client_publish_playback(
                    &mut producer_index,
                    sequence,
                    &[sequence as i32; 8],
                ));
            }
            assert!(!endpoint.region.try_client_publish_playback(
                &mut producer_index,
                99,
                &[99; 8],
            ));
        }

        assert!(state.close(first.session_id));
        let second = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should reopen");
        let stats = state.stats();
        let port = &stats.shared_playback_ports[0];
        assert_eq!(port.port_id, "line1");
        assert_eq!(port.expired_playback_periods, 3);
        assert_eq!(port.playback_submit_failures, 1);
        assert_eq!(port.playback_xruns, 1);
        assert!(state.close(second.session_id));
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
    fn missing_pro_repeats_only_pro_then_mixes_current_shared() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let pro_session = open_pro(&state).0;
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(pro_session));
        assert!(state.start(shared.session_id));
        activate_session(&state, pro_session, 9);
        activate_session(&state, shared.session_id, 9);

        let (_, mut playback) = state.bridges();
        let mut pro_index = 0;
        let mut shared_index = 0;
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut pro_index,
            10,
            &[100; 8],
        ));
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut shared_index, 10, &[10; 8])
        );
        let mut output = [0; 8];
        playback.process_playback(10, &mut output);
        playback.prepare_playback_mix(10);
        playback.commit_playback(10, &mut output);
        assert_eq!(output, [110; 8]);

        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut shared_index, 11, &[20; 8])
        );
        playback.process_playback(11, &mut output);
        playback.prepare_playback_mix(11);
        playback.commit_playback(11, &mut output);

        assert_eq!(output, [120; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 1);
        assert_eq!(timeline.snapshot().shared_underruns, 0);
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
        assert_eq!(state.shared[0].session.armed.load(Ordering::Acquire), 0);
        assert_eq!(state.shared[0].session.outage.load(Ordering::Acquire), 0);
    }

    #[test]
    fn prepared_shared_path_repeats_cached_period_when_enabled() {
        let profile_text = PROFILE.replace(
            "shared_latency_periods = 0",
            "shared_latency_periods = 0\n        shared_playback_repeat_on_underrun = true",
        );
        let profile = Profile::from_toml(&profile_text).expect("profile should parse");
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
                .try_client_publish_playback(&mut producer_index, 0, &[12; 8])
        );
        let (_, mut playback) = state.bridges();
        let mut output = [0; 8];
        playback.prepare_playback_mix(0);
        playback.process_playback(0, &mut output);
        playback.commit_playback(0, &mut output);
        assert_eq!(output, [12; 8]);

        playback.prepare_playback_mix(1);
        playback.process_playback(1, &mut output);
        playback.commit_playback(1, &mut output);
        assert_eq!(output, [12; 8]);
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
    fn shared_lifecycle_can_cut_a_waveform_without_an_underrun() {
        // Reproduce a sample discontinuity, not an acoustic hardware result.
        // Use the reference Q64 / five-period lookahead with a synthetic PCM.
        let profile = Profile::from_toml(
            &PROFILE
                .replace("period_size = 4", "period_size = 64")
                .replace("buffer_size = 8", "buffer_size = 256")
                .replace("shared_latency_periods = 0", "shared_latency_periods = 5"),
        )
        .unwrap();
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).unwrap();
        let shared = state.open_shared("line1").unwrap().unwrap();
        let mut bridge = state.bridge();
        let mut output = [0; 128];
        bridge.process(0, &[0; 128], &mut output);
        assert_eq!(output, [0; 128]);
        assert!(state.start(shared.session_id));
        activate_session(&state, shared.session_id, 1);

        // Continuous 1 kHz cosine at -12 dBFS, both channels identical.
        let block = |offset: usize| -> [i32; 128] {
            std::array::from_fn(|sample| {
                let phase = (offset + sample / 2) as f64 * std::f64::consts::TAU / 48.0;
                (phase.cos() * (i32::MAX as f64 / 4.0)) as i32
            })
        };
        let first = block(0);
        let second = block(64);
        let mut index = 0;
        for (sequence, samples) in [(1, &first), (2, &second)] {
            assert!(
                state.shared[0]
                    .session
                    .current()
                    .region
                    .try_client_publish_playback(&mut index, sequence, samples,)
            );
        }
        for sequence in 1..6 {
            bridge.process(sequence, &[0; 128], &mut output);
            assert_eq!(output, [0; 128]);
        }
        bridge.process(6, &[0; 128], &mut output);
        assert_eq!(output, first); // No fade-in at the silence-to-signal boundary.
        let last_sample = output[126];
        let natural_step = (i64::from(second[0]) - i64::from(last_sample)).abs();

        assert!(state.stop(shared.session_id));
        bridge.process(7, &[0; 128], &mut output);
        assert_eq!(output, [0; 128]); // Queued continuation was discarded.
        assert!(i64::from(last_sample).abs() > 2 * natural_step);
        let stats = timeline.snapshot();
        assert_eq!(stats.shared_underruns, 0);
        assert_eq!(stats.hw_playback_xruns, 0);
        assert_eq!(stats.hw_capture_xruns, 0);
        assert_eq!(stats.timeline_resets, 0);
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
        let stats = state.stats();
        let port = &stats.shared_playback_ports[0];
        assert_eq!(port.port_id, "line1");
        assert_eq!(port.underruns, 1);
        assert_eq!(port.last_underrun_sequence, 103);
        assert!(port.last_underrun_nanos > 0);
        assert_eq!(port.last_sequence_lag_periods, 1);
        assert_eq!(port.max_sequence_lag_periods, 1);

        bridge.process(107, &[0; 8], &mut output);
        assert_eq!(timeline.snapshot().shared_underruns, 1);
        let stats = state.stats();
        let port = &stats.shared_playback_ports[0];
        assert_eq!(port.underruns, 1);
        assert_eq!(port.last_underrun_sequence, 104);
        assert_eq!(port.last_sequence_lag_periods, 2);
        assert_eq!(port.max_sequence_lag_periods, 2);

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
        let stats = state.stats();
        let port = &stats.shared_playback_ports[0];
        assert_eq!(port.underruns, 2);
        assert_eq!(port.last_underrun_sequence, 106);
        assert_eq!(port.last_sequence_lag_periods, 1);
        assert_eq!(port.max_sequence_lag_periods, 2);
    }

    #[test]
    fn shared_playback_repeats_last_period_until_recovery_when_enabled() {
        let profile_text = PROFILE.replace(
            "shared_latency_periods = 0",
            "shared_latency_periods = 0\n        shared_playback_repeat_on_underrun = true",
        );
        let profile = Profile::from_toml(&profile_text).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
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
                .try_client_publish_playback(&mut producer_index, 0, &[10; 8])
        );
        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(0, &[0; 8], &mut output);
        assert_eq!(output, [10; 8]);

        bridge.process(1, &[0; 8], &mut output);
        assert_eq!(output, [10; 8]);
        bridge.process(2, &[0; 8], &mut output);
        assert_eq!(output, [10; 8]);
        assert_eq!(timeline.snapshot().shared_underruns, 1);
        assert_eq!(state.stats().shared_playback_ports[0].underruns, 1);

        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 3, &[20; 8])
        );
        bridge.process(3, &[0; 8], &mut output);
        assert_eq!(output, [20; 8]);
        bridge.process(4, &[0; 8], &mut output);
        assert_eq!(output, [20; 8]);
        assert_eq!(timeline.snapshot().shared_underruns, 2);

        assert!(state.stop(shared.session_id));
        assert!(state.start(shared.session_id));
        bridge.process(5, &[0; 8], &mut output);
        assert_eq!(output, [0; 8]);
    }

    #[test]
    fn hardware_generation_change_blocks_cache_until_new_lifecycle() {
        let profile_text = PROFILE.replace(
            "shared_latency_periods = 0",
            "shared_latency_periods = 0\n        shared_playback_repeat_on_underrun = true",
        );
        let profile = Profile::from_toml(&profile_text).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
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
                .try_client_publish_playback(&mut producer_index, 0, &[10; 8])
        );
        let (_, mut playback) = state.bridges();
        let mut output = [0; 8];
        playback.process_playback(0, &mut output);
        playback.commit_playback(0, &mut output);
        assert_eq!(output, [10; 8]);

        playback.shared[0].observed_hardware_generation = Some(u64::MAX);
        state.shared[0]
            .session
            .lifecycle_hardware_generation
            .store(u64::MAX, Ordering::Release);
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 1, &[99; 8])
        );
        playback.process_playback(1, &mut output);
        playback.commit_playback(1, &mut output);
        assert_eq!(output, [0; 8]);
        playback.process_playback(2, &mut output);
        playback.commit_playback(2, &mut output);
        assert_eq!(output, [0; 8]);
        assert_eq!(timeline.snapshot().shared_underruns, 0);

        assert!(state.stop(shared.session_id));
        assert!(state.start(shared.session_id));
        activate_session(&state, shared.session_id, 2);
        producer_index = 0;
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 3, &[20; 8])
        );
        playback.process_playback(3, &mut output);
        playback.commit_playback(3, &mut output);
        assert_eq!(output, [20; 8]);
    }

    #[test]
    fn shared_restart_before_generation_detection_is_not_blocked() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(shared.session_id));
        activate_session(&state, shared.session_id, u64::MAX);

        let (_, mut playback) = state.bridges();
        let mut producer_index = 0;
        let mut output = [0; 8];
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 0, &[10; 8])
        );
        playback.process_playback(0, &mut output);
        playback.commit_playback(0, &mut output);
        assert_eq!(output, [10; 8]);

        playback.shared[0].observed_hardware_generation = Some(u64::MAX);
        state.shared[0]
            .session
            .lifecycle_hardware_generation
            .store(u64::MAX, Ordering::Release);
        assert!(state.stop(shared.session_id));
        assert!(state.start(shared.session_id));
        activate_session(&state, shared.session_id, 0);
        producer_index = 0;
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 1, &[20; 8])
        );
        playback.process_playback(1, &mut output);
        playback.commit_playback(1, &mut output);

        assert_eq!(output, [20; 8]);
        assert_eq!(timeline.snapshot().shared_underruns, 0);
    }

    #[test]
    fn hardware_generation_change_blocks_pro_until_new_lifecycle() {
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

        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            12,
            &[99; 8],
        ));
        state
            .pro
            .lifecycle_hardware_generation
            .store(u64::MAX, Ordering::Release);
        playback.process_playback(12, &mut output);
        assert_eq!(output, [0; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 0);
        assert_eq!(state.pro.armed.load(Ordering::Acquire), 0);

        let mut stale = [0; 8];
        assert!(
            state
                .pro
                .current()
                .region
                .try_consume_playback(12, &mut stale)
        );
        assert_eq!(stale, [99; 8]);
        playback.process_playback(13, &mut output);
        assert_eq!(output, [0; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 0);

        assert!(state.stop(session));
        assert!(state.start(session));
        activate_session(&state, session, 13);
        producer_index = 0;
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            14,
            &[14; 8],
        ));
        playback.process_playback(14, &mut output);
        assert_eq!(output, [14; 8]);
    }

    #[test]
    fn pro_restart_before_generation_detection_is_not_blocked() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let session = open_pro(&state).0;
        assert!(state.start(session));
        activate_session(&state, session, 9);

        let (_, mut playback) = state.bridges();
        let mut producer_index = 0;
        let mut output = [0; 8];
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            10,
            &[10; 8],
        ));
        playback.process_playback(10, &mut output);
        state
            .pro
            .lifecycle_hardware_generation
            .store(u64::MAX, Ordering::Release);

        assert!(state.stop(session));
        assert!(state.start(session));
        activate_session(&state, session, 10);
        producer_index = 0;
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut producer_index,
            11,
            &[11; 8],
        ));
        playback.process_playback(11, &mut output);

        assert_eq!(output, [11; 8]);
        assert_eq!(
            state
                .pro
                .lifecycle_hardware_generation
                .load(Ordering::Acquire),
            timeline.generation()
        );
        assert_eq!(timeline.snapshot().pro_deadline_misses, 0);
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
        let stats = state.stats();
        assert_eq!(stats.shared_playback_ports[0].underruns, 0);
        assert_eq!(stats.shared_playback_ports[0].max_sequence_lag_periods, 0);
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
    fn first_valid_pro_block_arms_repeat_fallback() {
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
        assert_eq!(output, [11; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 1);

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
        assert_eq!(output, [14; 8]);
        assert_eq!(stats.pro_deadline_misses, 2);
        assert_eq!(stats.pro_client_deadline_misses, 2);
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
        let capacity = u64::from(shared.shared.slot_count);
        for sequence in 0..capacity {
            capture.process_capture(sequence, &[sequence as i32; 8]);
        }
        assert_eq!(timeline.snapshot().shared_overruns, 0);
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
        assert_eq!(output, [11; 8]);
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
        assert_eq!(output, [11; 8]);
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
