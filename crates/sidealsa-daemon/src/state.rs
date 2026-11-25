use sidealsa_client::PlaybackConsume;
use sidealsa_config::{PortConfig, Profile};
use sidealsa_core::{HardwareStats, HardwareTimeline, ProCaptureSink, ProPlaybackSource};
use sidealsa_protocol::{
    DeviceInfo, PortDirection, PortInfo, SHARED_CLIENT_IDLE, SHARED_CLIENT_RUNNING,
    SHARED_CLIENT_STARTING, SharedPlaybackPortStats, SharedRegionInfo, Stats, sequence_is_before,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering, fence},
};
use thiserror::Error;

use crate::diagnostics::{ProDiagnostics, ProDiagnosticsSnapshot, ProMiss};
use crate::shared::{PlaybackReadyWait, SharedError, SharedEvents, SharedRegion};

const SESSION_CLOSING: u64 = u64::MAX;
// A stalled-but-connected client must not turn a repeated block into a sustained tone.
const MAX_REPEATED_PLAYBACK_PERIODS: u32 = 4;
const RT_READER_INACTIVE: u64 = u64::MAX;
const RT_SLOT_CAPTURE: usize = 0;
const RT_SLOT_PLAYBACK: usize = 1;

#[repr(align(64))]
struct CachePadded<T>(T);

struct RtReadDomain {
    epoch: CachePadded<AtomicU64>,
    readers: [CachePadded<AtomicU64>; 2],
}

impl Default for RtReadDomain {
    fn default() -> Self {
        Self {
            epoch: CachePadded(AtomicU64::new(0)),
            readers: [
                CachePadded(AtomicU64::new(RT_READER_INACTIVE)),
                CachePadded(AtomicU64::new(RT_READER_INACTIVE)),
            ],
        }
    }
}

impl RtReadDomain {
    #[inline]
    fn begin_unflushed(&self, slot: usize) {
        // Acquire pairs with wait_for_idle()'s epoch.fetch_add(SeqCst) so observing
        // epoch > retired guarantees visibility of the preceding swap or stop.
        let epoch = self.epoch.0.load(Ordering::Acquire);
        // Release ensures any prior critical section on this slot happens-before a
        // concurrent wait_for_idle() that observes this new epoch.
        self.readers[slot].0.store(epoch, Ordering::Release);
    }

    #[inline]
    fn enter(&self, slot: usize) -> RtPin<'_> {
        self.begin_unflushed(slot);
        // Pairs with SeqCst swap/stop + epoch.fetch_add + readers.load(SeqCst) in
        // wait_for_idle(): either wait_for_idle waits on our active epoch, or our
        // subsequent Acquire loads observe the swap/stop that preceded wait_for_idle.
        fence(Ordering::SeqCst);
        RtPin { domain: self, slot }
    }

    #[inline]
    fn exit(&self, slot: usize) {
        self.readers[slot]
            .0
            .store(RT_READER_INACTIVE, Ordering::Release);
    }

    fn wait_for_idle(&self) {
        let retired = self.epoch.0.fetch_add(1, Ordering::SeqCst);
        for reader in &self.readers {
            while {
                let observed = reader.0.load(Ordering::SeqCst);
                observed != RT_READER_INACTIVE && observed <= retired
            } {
                std::thread::yield_now();
            }
        }
    }
}

struct RtPin<'a> {
    domain: &'a RtReadDomain,
    slot: usize,
}

impl Drop for RtPin<'_> {
    #[inline]
    fn drop(&mut self) {
        self.domain.exit(self.slot);
    }
}

struct CapturePairGuard<'a> {
    bridge: &'a mut DaemonCaptureBridge,
}

impl<'a> CapturePairGuard<'a> {
    #[inline]
    fn enter(bridge: &'a mut DaemonCaptureBridge) -> Self {
        bridge.pro_domain.begin_unflushed(RT_SLOT_CAPTURE);
        bridge.pro_capture_domain.begin_unflushed(RT_SLOT_CAPTURE);
        fence(Ordering::SeqCst);
        Self { bridge }
    }
}

impl Drop for CapturePairGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.bridge.pro_domain.exit(RT_SLOT_CAPTURE);
        self.bridge.pro_capture_domain.exit(RT_SLOT_CAPTURE);
    }
}

struct SessionState {
    endpoint: Arc<EndpointSlot>,
    playback_epoch: Option<Arc<AtomicU64>>,
    playback_commits: Option<Arc<PlaybackCommitBarrier>>,
    period_frames: u32,
    playback_channels: u32,
    capture_channels: u32,
    slot_count: u32,
}

struct SessionEndpoint {
    session_id: u64,
    has_capture: bool,
    slot_count: usize,
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
        let region = SharedRegion::create_with_slot_count(
            period_frames,
            playback_channels,
            capture_channels,
            slot_count,
        )?;
        let region_slots = region.info().slot_count as usize;
        Ok(Box::new(Self {
            session_id,
            has_capture: capture_channels != 0,
            slot_count: region_slots,
            region,
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

// Fits all hot RT-read session atomics in one 64-byte cache line, isolated from
// control-plane / GET_STATS writes in the rest of EndpointSlot.
#[repr(align(64))]
struct SessionHot {
    current: AtomicPtr<SessionEndpoint>,
    active: AtomicU64,
    lifecycle_generation: AtomicU64,
    lifecycle_hardware_generation: AtomicU64,
    armed: AtomicU64,
    outage: AtomicU64,
    warmup_blocks: AtomicU64,
}

#[repr(C)]
struct EndpointSlot {
    hot: SessionHot,
    reader_epoch: AtomicU64,
    reader_activity: [AtomicU32; 2],
    owner: AtomicU64,
    rt_domain: Option<Arc<RtReadDomain>>,
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
    #[cfg(test)]
    fn new(endpoint: Box<SessionEndpoint>) -> Self {
        Self::create(endpoint, None)
    }

    fn with_rt_domain(endpoint: Box<SessionEndpoint>, rt_domain: Arc<RtReadDomain>) -> Self {
        Self::create(endpoint, Some(rt_domain))
    }

    fn create(endpoint: Box<SessionEndpoint>, rt_domain: Option<Arc<RtReadDomain>>) -> Self {
        Self {
            hot: SessionHot {
                current: AtomicPtr::new(Box::into_raw(endpoint)),
                active: AtomicU64::new(0),
                lifecycle_generation: AtomicU64::new(0),
                lifecycle_hardware_generation: AtomicU64::new(0),
                armed: AtomicU64::new(0),
                outage: AtomicU64::new(0),
                warmup_blocks: AtomicU64::new(0),
            },
            reader_epoch: AtomicU64::new(0),
            reader_activity: [AtomicU32::new(0), AtomicU32::new(0)],
            owner: AtomicU64::new(0),
            rt_domain,
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
            let current = self.hot.current.load(Ordering::SeqCst);
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

    /// # Safety
    /// Caller must hold an active pin on `domain` (which must match `self.rt_domain`)
    /// for at least the returned reference lifetime.
    #[inline]
    unsafe fn load_pinned<'a>(&'a self, domain: &Arc<RtReadDomain>) -> &'a SessionEndpoint {
        debug_assert!(
            self.rt_domain
                .as_ref()
                .is_some_and(|slot_domain| Arc::ptr_eq(slot_domain, domain))
        );
        let current = self.hot.current.load(Ordering::Acquire);
        debug_assert!(!current.is_null());
        // SAFETY: `current` comes from `Box::into_raw`; `replace()` drains `rt_domain`
        // after swapping `current` and before freeing the retired `SessionEndpoint`.
        unsafe { &*current }
    }

    fn replace(&self, endpoint: Box<SessionEndpoint>) {
        let mut retired_diagnostics = self
            .retired_client_playback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let replacement = Box::into_raw(endpoint);
        let retired = self.hot.current.swap(replacement, Ordering::SeqCst);
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
        if let Some(rt_domain) = &self.rt_domain {
            rt_domain.wait_for_idle();
        }
    }

    // Single writer: the daemon's sole playback bridge.
    fn record_playback_miss(&self, sequence: u64, sequence_lag_periods: u64, starts_outage: bool) {
        self.last_playback_underrun_sequence
            .store(sequence, Ordering::Relaxed);
        self.last_playback_underrun_nanos
            .store(monotonic_nanos(), Ordering::Relaxed);
        self.last_playback_sequence_lag_periods
            .store(sequence_lag_periods, Ordering::Relaxed);
        if sequence_lag_periods
            > self
                .max_playback_sequence_lag_periods
                .load(Ordering::Relaxed)
        {
            self.max_playback_sequence_lag_periods
                .store(sequence_lag_periods, Ordering::Relaxed);
        }
        if starts_outage {
            let next = self
                .playback_underruns
                .load(Ordering::Relaxed)
                .wrapping_add(1);
            self.playback_underruns.store(next, Ordering::Release);
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
        let current = *self.hot.current.get_mut();
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

#[repr(align(64))]
#[derive(Default)]
struct PlaybackCommitBarrier {
    started: AtomicU64,
    completed: AtomicU64,
}

impl PlaybackCommitBarrier {
    #[inline]
    fn begin_unflushed(&self) {
        // Single writer: the daemon's sole playback bridge.
        let next = self.started.load(Ordering::Relaxed).wrapping_add(1);
        self.started.store(next, Ordering::Release);
    }

    #[cfg(test)]
    fn begin(&self) {
        self.begin_unflushed();
        fence(Ordering::SeqCst);
    }

    #[inline]
    fn end(&self) {
        // Single writer: Release orders all commit work before wait_for() observes completion.
        let next = self.completed.load(Ordering::Relaxed).wrapping_add(1);
        self.completed.store(next, Ordering::Release);
    }

    fn wait_for_preexisting(&self) {
        self.wait_for(self.started.load(Ordering::SeqCst));
    }

    fn wait_for(&self, target: u64) {
        while sequence_is_before(self.completed.load(Ordering::Acquire), target) {
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
        rt_domain: Arc<RtReadDomain>,
        playback_epoch: Option<Arc<AtomicU64>>,
        playback_commits: Option<Arc<PlaybackCommitBarrier>>,
    ) -> Result<Self, SharedError> {
        Ok(Self {
            endpoint: Arc::new(EndpointSlot::with_rt_domain(
                SessionEndpoint::create(
                    0,
                    period_frames,
                    playback_channels,
                    capture_channels,
                    slot_count,
                )?,
                rt_domain,
            )),
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
            .endpoint
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
                self.endpoint.owner.store(0, Ordering::Release);
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
        if self.endpoint.owner.load(Ordering::Acquire) != session_id
            || self.endpoint.hot.active.load(Ordering::SeqCst) != 0
        {
            return false;
        }
        let endpoint = self.endpoint.load();
        if endpoint.session_id != session_id {
            return false;
        }
        endpoint.region.reset_slots();
        endpoint.events.drain();
        let generation = self
            .endpoint
            .hot
            .lifecycle_generation
            .fetch_add(1, Ordering::AcqRel)
            + 1;
        endpoint.region.set_lifecycle_generation(generation);
        self.endpoint
            .hot
            .lifecycle_hardware_generation
            .store(hardware_generation, Ordering::Release);
        endpoint.region.set_hardware_generation(hardware_generation);
        endpoint.region.reset_activation();
        endpoint.region.set_client_state(SHARED_CLIENT_STARTING);
        if self
            .endpoint
            .hot
            .active
            .compare_exchange(0, session_id, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            endpoint.region.set_client_state(SHARED_CLIENT_IDLE);
            return false;
        }
        self.endpoint.hot.armed.store(0, Ordering::Release);
        self.endpoint.hot.outage.store(0, Ordering::Release);
        self.endpoint.hot.warmup_blocks.store(0, Ordering::Release);
        true
    }

    fn stop(&self, session_id: u64) -> bool {
        if self.endpoint.hot.active.load(Ordering::SeqCst) != session_id {
            return false;
        }
        self.endpoint.hot.armed.store(0, Ordering::Release);
        self.endpoint.hot.outage.store(0, Ordering::Release);
        self.endpoint.hot.warmup_blocks.store(0, Ordering::Release);
        let stopped = self
            .endpoint
            .hot
            .active
            .compare_exchange(session_id, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if stopped {
            self.bump_playback_epoch();
            let endpoint = self.endpoint.load();
            endpoint.events.notify_playback_ready();
            drop(endpoint);
            self.wait_for_rt_idle();
            self.endpoint.hot.armed.store(0, Ordering::Release);
            self.endpoint.hot.outage.store(0, Ordering::Release);
            self.endpoint.hot.warmup_blocks.store(0, Ordering::Release);
            let endpoint = self.endpoint.load();
            endpoint.region.reset_slots();
            endpoint.events.drain();
        }
        stopped
    }

    fn close(&self, session_id: u64) -> bool {
        if self
            .endpoint
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
        let was_active = self.endpoint.hot.active.load(Ordering::SeqCst) == session_id;
        if was_active {
            self.endpoint.hot.armed.store(0, Ordering::Release);
            self.endpoint.hot.outage.store(0, Ordering::Release);
            self.endpoint.hot.warmup_blocks.store(0, Ordering::Release);
        }
        let stopped = self
            .endpoint
            .hot
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
        self.endpoint.hot.armed.store(0, Ordering::Release);
        self.endpoint.hot.outage.store(0, Ordering::Release);
        self.endpoint.hot.warmup_blocks.store(0, Ordering::Release);
        let endpoint = self.endpoint.load();
        endpoint.region.reset_slots();
        endpoint.events.drain();
        self.endpoint.owner.store(0, Ordering::Release);
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
        rt_domain: Arc<RtReadDomain>,
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
                rt_domain,
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
    pro_domain: Arc<RtReadDomain>,
    pro_capture_domain: Arc<RtReadDomain>,
    shared_playback_domain: Arc<RtReadDomain>,
    shared_capture_domain: Arc<RtReadDomain>,
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
        let pro_domain = Arc::new(RtReadDomain::default());
        let pro_capture_domain = Arc::new(RtReadDomain::default());
        let shared_playback_domain = Arc::new(RtReadDomain::default());
        let shared_capture_domain = Arc::new(RtReadDomain::default());
        let pro = SessionState::new(
            profile.device.period_size,
            profile.device.playback.channels,
            profile.device.capture.channels,
            sidealsa_protocol::SHARED_SLOT_COUNT,
            Arc::clone(&pro_domain),
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
                Arc::clone(&shared_playback_domain),
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
                Arc::clone(&shared_capture_domain),
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
            pro_domain,
            pro_capture_domain: Arc::clone(&pro_capture_domain),
            shared_playback_domain,
            shared_capture_domain,
            pro,
            pro_capture: SessionState::new(
                profile.device.period_size,
                0,
                profile.device.capture.channels,
                sidealsa_protocol::SHARED_SLOT_COUNT,
                pro_capture_domain,
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
        // Only PRO needs the ownership lock. SHARED start may spin on client-written
        // activation state, and its session ids are owned by a single connection.
        if self.pro_capture.endpoint.owner.load(Ordering::Acquire) == session_id
            || self.pro.endpoint.owner.load(Ordering::Acquire) == session_id
        {
            let _ownership = self
                .pro_ownership
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let hardware_generation = self.timeline.generation();
            if self.pro_capture.endpoint.owner.load(Ordering::Acquire) == session_id {
                return self.pro_capture.start(session_id, hardware_generation);
            }
            if self.pro.endpoint.owner.load(Ordering::Acquire) == session_id {
                if self.pro.endpoint.hot.active.load(Ordering::SeqCst) != 0 {
                    return false;
                }
                return self.pro.start(session_id, hardware_generation);
            }
            return false;
        }
        let hardware_generation = self.timeline.generation();
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
        if state.endpoint.hot.active.load(Ordering::SeqCst) != 0 {
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
        self.pro.endpoint.owner.load(Ordering::Acquire) == session_id
            || self.pro_capture.endpoint.owner.load(Ordering::Acquire) == session_id
            || self
                .shared
                .iter()
                .any(|port| port.session.endpoint.owner.load(Ordering::Acquire) == session_id)
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
            pair_wait: [None, None],
            pro_domain: Arc::clone(&self.pro_domain),
            pro_capture_domain: Arc::clone(&self.pro_capture_domain),
            shared_capture_domain: Arc::clone(&self.shared_capture_domain),
            directional_endpoint: Arc::clone(&self.pro_capture.endpoint),
            directional_capture_index: 0,
            pro_endpoint: Arc::clone(&self.pro.endpoint),
            pro_capture_index: 0,
            shared: capture_shared,
            timeline: Arc::clone(&self.timeline),
        };
        let playback = DaemonPlaybackBridge {
            pro_diagnostics: Arc::clone(&self.pro_diagnostics),
            pro_wait_timing: None,
            pro_domain: Arc::clone(&self.pro_domain),
            shared_playback_domain: Arc::clone(&self.shared_playback_domain),
            commit_pinned: false,
            pro_endpoint: Arc::clone(&self.pro.endpoint),
            playback_epoch: Arc::clone(&self.playback_epoch),
            playback_commits: Arc::clone(&self.playback_commits),
            defer_playback_notifications: false,
            pending_pro_notify: false,
            any_pending_shared_notify: false,
            any_prepared_shared: false,
            shared: playback_shared,
            prepared_shared_sequence: None,
            last_valid_pro: vec![0; self.period_frames * self.playback_channels].into_boxed_slice(),
            last_valid_pro_identity: None,
            pro_repeated_periods: 0,
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
    pair_wait: [Option<(u64, u64, u64)>; 2],
    pro_domain: Arc<RtReadDomain>,
    pro_capture_domain: Arc<RtReadDomain>,
    shared_capture_domain: Arc<RtReadDomain>,
    directional_endpoint: Arc<EndpointSlot>,
    directional_capture_index: usize,
    pro_endpoint: Arc<EndpointSlot>,
    pro_capture_index: usize,
    shared: Box<[SharedAudioPortBridge]>,
    timeline: Arc<HardwareTimeline>,
}

pub struct DaemonPlaybackBridge {
    pro_diagnostics: Arc<ProDiagnostics>,
    // Identity, sequence, cutoff, budget at entry to the direct client wait.
    pro_wait_timing: Option<(ProPlaybackIdentity, u64, u64, u64)>,
    pro_domain: Arc<RtReadDomain>,
    shared_playback_domain: Arc<RtReadDomain>,
    commit_pinned: bool,
    pro_endpoint: Arc<EndpointSlot>,
    playback_epoch: Arc<AtomicU64>,
    playback_commits: Arc<PlaybackCommitBarrier>,
    defer_playback_notifications: bool,
    pending_pro_notify: bool,
    any_pending_shared_notify: bool,
    any_prepared_shared: bool,
    shared: Box<[SharedAudioPortBridge]>,
    prepared_shared_sequence: Option<u64>,
    last_valid_pro: Box<[i32]>,
    last_valid_pro_identity: Option<ProPlaybackIdentity>,
    pro_repeated_periods: u32,
    core_miss_pro_sequence: Option<u64>,
    timeline: Arc<HardwareTimeline>,
}

impl Drop for DaemonPlaybackBridge {
    fn drop(&mut self) {
        if self.commit_pinned {
            self.commit_pinned = false;
            self.pro_domain.exit(RT_SLOT_PLAYBACK);
            self.shared_playback_domain.exit(RT_SLOT_PLAYBACK);
        }
    }
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

#[derive(Clone, Copy)]
struct ProPlaybackGateState {
    hardware_generation: u64,
    session_id: u64,
    blocked: bool,
}

#[inline]
fn ensure_activation(region: &SharedRegion, sequence: u64) -> bool {
    // Avoid an unconditional shared-memory CAS on every steady-state period once activated.
    !region.activation_ready() && region.establish_activation(sequence)
}

#[inline]
fn synchronize_pro_gate(
    slot: &EndpointSlot,
    endpoint: &SessionEndpoint,
    timeline: &HardwareTimeline,
    sequence: u64,
    notify_immediately: bool,
) -> ProPlaybackGateState {
    let hardware_generation = timeline.generation();
    endpoint.region.set_hardware_generation(hardware_generation);
    endpoint.region.set_playback_sequence(sequence);

    let session_id = slot.hot.active.load(Ordering::Acquire);
    let blocked = session_id != 0
        && endpoint.session_id == session_id
        && slot
            .hot
            .lifecycle_hardware_generation
            .load(Ordering::Acquire)
            != hardware_generation;
    if blocked {
        let _ = slot
            .hot
            .armed
            .compare_exchange(session_id, 0, Ordering::AcqRel, Ordering::Acquire);
        slot.hot.warmup_blocks.store(0, Ordering::Release);
        if notify_immediately {
            endpoint.events.notify_playback();
        }
    }
    ProPlaybackGateState {
        hardware_generation,
        session_id,
        blocked,
    }
}

enum ChannelMap {
    /// Logical channels match physical channels 0..physical_channels identically.
    Identity,
    /// Single physical channel `ch < physical_channels`.
    Mono(usize),
    /// Contiguous stereo pair `[start, start + 1]` with `start + 2 <= physical_channels`.
    StereoContiguous(usize),
    /// Contiguous slice `[start..start + count]` with `start + count <= physical_channels`.
    Contiguous { start: usize, count: usize },
    /// Arbitrary channel order or non-contiguous indices.
    General(Box<[usize]>),
}

impl ChannelMap {
    fn from_channels(channels: &[usize], physical_channels: usize) -> Self {
        match channels {
            [ch] if *ch < physical_channels => {
                if *ch == 0 && physical_channels == 1 {
                    Self::Identity
                } else {
                    Self::Mono(*ch)
                }
            }
            [a, b] if *b == a.wrapping_add(1) && a.saturating_add(2) <= physical_channels => {
                if *a == 0 && physical_channels == 2 {
                    Self::Identity
                } else {
                    Self::StereoContiguous(*a)
                }
            }
            [first, ..]
                if channels.len() <= physical_channels
                    && first.saturating_add(channels.len()) <= physical_channels
                    && channels
                        .iter()
                        .enumerate()
                        .all(|(i, &ch)| ch == first.wrapping_add(i)) =>
            {
                if *first == 0 && channels.len() == physical_channels {
                    Self::Identity
                } else {
                    Self::Contiguous {
                        start: *first,
                        count: channels.len(),
                    }
                }
            }
            _ => Self::General(channels.to_vec().into_boxed_slice()),
        }
    }

    #[inline]
    fn mix_into_physical(
        &self,
        period_frames: usize,
        physical_channels: usize,
        scratch: &[i32],
        physical: &mut [i32],
    ) {
        let total_physical = period_frames * physical_channels;
        let physical = &mut physical[..total_physical];
        match self {
            Self::Identity => {
                for (dst, &src) in physical.iter_mut().zip(&scratch[..total_physical]) {
                    *dst = dst.saturating_add(src);
                }
            }
            Self::Mono(ch) => {
                let ch = *ch;
                assert!(ch < physical_channels);
                for (frame_out, &sample) in physical
                    .chunks_exact_mut(physical_channels)
                    .zip(&scratch[..period_frames])
                {
                    frame_out[ch] = frame_out[ch].saturating_add(sample);
                }
            }
            Self::StereoContiguous(start) => {
                let start = *start;
                assert!(start + 2 <= physical_channels);
                let (stereo_frames, _) = scratch[..period_frames * 2].as_chunks::<2>();
                for (frame_out, frame_in) in physical
                    .chunks_exact_mut(physical_channels)
                    .zip(stereo_frames)
                {
                    let dst = &mut frame_out[start..start + 2];
                    dst[0] = dst[0].saturating_add(frame_in[0]);
                    dst[1] = dst[1].saturating_add(frame_in[1]);
                }
            }
            Self::Contiguous { start, count } => {
                let (start, count) = (*start, *count);
                assert!(start + count <= physical_channels);
                for (frame_out, frame_in) in physical
                    .chunks_exact_mut(physical_channels)
                    .zip(scratch[..period_frames * count].chunks_exact(count))
                {
                    for (dst, &src) in frame_out[start..start + count].iter_mut().zip(frame_in) {
                        *dst = dst.saturating_add(src);
                    }
                }
            }
            Self::General(channels) => {
                let logical_channels = channels.len();
                if logical_channels == 0 {
                    return;
                }
                for (frame_out, frame_in) in physical
                    .chunks_exact_mut(physical_channels)
                    .zip(scratch[..period_frames * logical_channels].chunks_exact(logical_channels))
                {
                    for (&physical_channel, &sample) in channels.iter().zip(frame_in) {
                        frame_out[physical_channel] =
                            frame_out[physical_channel].saturating_add(sample);
                    }
                }
            }
        }
    }

    #[inline]
    fn extract_from_physical<'a>(
        &self,
        period_frames: usize,
        physical_channels: usize,
        physical: &'a [i32],
        scratch: &'a mut [i32],
    ) -> &'a [i32] {
        let total_physical = period_frames * physical_channels;
        let physical = &physical[..total_physical];
        match self {
            Self::Identity => physical,
            Self::Mono(ch) => {
                let ch = *ch;
                assert!(ch < physical_channels);
                let out = &mut scratch[..period_frames];
                for (dst, frame_in) in out.iter_mut().zip(physical.chunks_exact(physical_channels))
                {
                    *dst = frame_in[ch];
                }
                out
            }
            Self::StereoContiguous(start) => {
                let start = *start;
                assert!(start + 2 <= physical_channels);
                let out = &mut scratch[..period_frames * 2];
                let (stereo_out, _) = out.as_chunks_mut::<2>();
                for (frame_out, frame_in) in stereo_out
                    .iter_mut()
                    .zip(physical.chunks_exact(physical_channels))
                {
                    let src = &frame_in[start..start + 2];
                    frame_out[0] = src[0];
                    frame_out[1] = src[1];
                }
                out
            }
            Self::Contiguous { start, count } => {
                let (start, count) = (*start, *count);
                assert!(start + count <= physical_channels);
                let out = &mut scratch[..period_frames * count];
                for (frame_out, frame_in) in out
                    .chunks_exact_mut(count)
                    .zip(physical.chunks_exact(physical_channels))
                {
                    frame_out.copy_from_slice(&frame_in[start..start + count]);
                }
                out
            }
            Self::General(channels) => {
                let logical_channels = channels.len();
                let out = &mut scratch[..period_frames * logical_channels];
                if logical_channels == 0 {
                    return out;
                }
                for (frame_out, frame_in) in out
                    .chunks_exact_mut(logical_channels)
                    .zip(physical.chunks_exact(physical_channels))
                {
                    for (dst, &physical_channel) in frame_out.iter_mut().zip(channels.iter()) {
                        *dst = frame_in[physical_channel];
                    }
                }
                out
            }
        }
    }
}

struct SharedAudioPortBridge {
    endpoint: Arc<EndpointSlot>,
    channel_map: ChannelMap,
    period_frames: usize,
    physical_channels: usize,
    capture_capacity_slots: usize,
    latency_periods: u64,
    repeat_on_underrun: bool,
    index: usize,
    scratch: Box<[i32]>,
    last_valid_playback: Option<SharedPlaybackIdentity>,
    repeated_periods: u32,
    observed_hardware_generation: Option<u64>,
    prepared: Option<PreparedSharedPlayback>,
    pending_notify: bool,
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
        let physical_channels = match port.direction {
            PortDirection::Playback => playback_channels,
            PortDirection::Capture => capture_channels,
        };
        let channel_map = ChannelMap::from_channels(&port.channels, physical_channels);
        Self {
            endpoint: Arc::clone(&port.session.endpoint),
            channel_map,
            period_frames,
            physical_channels,
            capture_capacity_slots,
            latency_periods: u64::from(latency_periods),
            repeat_on_underrun,
            index: 0,
            scratch: vec![0; port.logical_samples].into_boxed_slice(),
            last_valid_playback: None,
            repeated_periods: 0,
            observed_hardware_generation: None,
            prepared: None,
            pending_notify: false,
        }
    }

    /// # Safety
    /// Caller must hold an active pin on `domain` covering `self.endpoint`.
    #[inline]
    unsafe fn flush_pending_notify_pinned(&mut self, domain: &Arc<RtReadDomain>) {
        if self.pending_notify {
            self.pending_notify = false;
            unsafe { self.endpoint.load_pinned(domain) }
                .events
                .notify_playback();
        }
    }

    /// # Safety
    /// Caller must hold an active pin on `domain` covering `self.endpoint`.
    unsafe fn prepare_playback_pinned(
        &mut self,
        sequence: u64,
        hardware_generation: u64,
        timeline: &HardwareTimeline,
        defer_notify: bool,
        domain: &Arc<RtReadDomain>,
    ) {
        if self.prepared.take().is_some_and(|prepared| prepared.fresh) {
            self.last_valid_playback = None;
        }
        let endpoint = unsafe { self.endpoint.load_pinned(domain) };
        let hardware_generation_changed = self
            .observed_hardware_generation
            .replace(hardware_generation)
            .is_some_and(|previous| previous != hardware_generation);
        if hardware_generation_changed {
            self.last_valid_playback = None;
        }
        endpoint.region.set_hardware_generation(hardware_generation);
        endpoint.region.set_cycle_sequence(sequence);
        let session_id = self.endpoint.hot.active.load(Ordering::Acquire);
        if session_id == 0 || endpoint.session_id != session_id {
            self.last_valid_playback = None;
            return;
        }
        let generation = self
            .endpoint
            .hot
            .lifecycle_generation
            .load(Ordering::Acquire);
        if ensure_activation(&endpoint.region, sequence) {
            self.last_valid_playback = None;
            notify_or_defer(&mut self.pending_notify, endpoint, defer_notify);
            return;
        }
        let client_state = endpoint.region.client_state();
        if client_state == SHARED_CLIENT_STARTING {
            self.last_valid_playback = None;
            notify_or_defer(&mut self.pending_notify, endpoint, defer_notify);
            return;
        }
        if client_state != SHARED_CLIENT_RUNNING {
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
        let session_is_current = self.endpoint.hot.active.load(Ordering::Acquire) == session_id
            && self
                .endpoint
                .hot
                .lifecycle_generation
                .load(Ordering::Acquire)
                == generation
            && timeline.generation() == hardware_generation;
        if consumed && session_is_current {
            self.prepared = Some(PreparedSharedPlayback {
                sequence,
                identity,
                fresh: true,
            });
        } else if !consumed && session_is_current {
            let armed = self.endpoint.hot.armed.load(Ordering::Acquire) == session_id;
            let outage = self.endpoint.hot.outage.load(Ordering::Acquire) == session_id;
            if !armed && !outage {
                notify_or_defer(&mut self.pending_notify, endpoint, defer_notify);
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
                let _ = self.endpoint.hot.armed.compare_exchange(
                    session_id,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                self.endpoint
                    .hot
                    .outage
                    .store(session_id, Ordering::Release);
            }
            if self.repeat_on_underrun
                && self.last_valid_playback == Some(identity)
                && self.repeated_periods < MAX_REPEATED_PLAYBACK_PERIODS
            {
                self.repeated_periods += 1;
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
        notify_or_defer(&mut self.pending_notify, endpoint, defer_notify);
    }

    /// # Safety
    /// Caller must hold an active pin on `domain` covering `self.endpoint`.
    unsafe fn commit_prepared_pinned(
        &mut self,
        sequence: u64,
        physical: &mut [i32],
        hardware_generation: u64,
        domain: &Arc<RtReadDomain>,
    ) {
        let Some(prepared) = self.prepared.take() else {
            return;
        };
        if prepared.sequence != sequence
            || self.endpoint.hot.active.load(Ordering::Acquire) != prepared.identity.session_id
            || self
                .endpoint
                .hot
                .lifecycle_generation
                .load(Ordering::Acquire)
                != prepared.identity.lifecycle_generation
            || hardware_generation != prepared.identity.hardware_generation
        {
            self.last_valid_playback = None;
            return;
        }
        let endpoint = unsafe { self.endpoint.load_pinned(domain) };
        if endpoint.session_id != prepared.identity.session_id
            || endpoint.region.client_state() != SHARED_CLIENT_RUNNING
        {
            self.last_valid_playback = None;
            return;
        }
        if prepared.fresh {
            if self.endpoint.hot.outage.load(Ordering::Acquire) == prepared.identity.session_id {
                let _ = self.endpoint.hot.outage.compare_exchange(
                    prepared.identity.session_id,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            if self.endpoint.hot.armed.load(Ordering::Acquire) != prepared.identity.session_id {
                let _ = self.endpoint.hot.armed.compare_exchange(
                    0,
                    prepared.identity.session_id,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            self.last_valid_playback = Some(prepared.identity);
            self.repeated_periods = 0;
        }
        self.channel_map.mix_into_physical(
            self.period_frames,
            self.physical_channels,
            &self.scratch,
            physical,
        );
    }

    /// # Safety
    /// Caller must hold an active pin on `domain` covering `self.endpoint`.
    unsafe fn process_playback_pinned(
        &mut self,
        sequence: u64,
        physical: &mut [i32],
        hardware_generation: u64,
        timeline: &HardwareTimeline,
        defer_notify: bool,
        domain: &Arc<RtReadDomain>,
    ) {
        unsafe {
            self.prepare_playback_pinned(
                sequence,
                hardware_generation,
                timeline,
                defer_notify,
                domain,
            );
            self.commit_prepared_pinned(sequence, physical, timeline.generation(), domain);
        }
    }

    /// # Safety
    /// Caller must hold an active pin on `domain` covering `self.endpoint`.
    unsafe fn process_capture_pinned(
        &mut self,
        sequence: u64,
        physical: &[i32],
        hardware_generation: u64,
        timeline: &HardwareTimeline,
        domain: &Arc<RtReadDomain>,
    ) {
        let endpoint = unsafe { self.endpoint.load_pinned(domain) };
        endpoint.region.set_hardware_generation(hardware_generation);
        endpoint.region.set_cycle_sequence(sequence);
        let session_id = self.endpoint.hot.active.load(Ordering::Acquire);
        if session_id == 0
            || endpoint.session_id != session_id
            || endpoint.region.client_state() == SHARED_CLIENT_IDLE
        {
            return;
        }
        if ensure_activation(&endpoint.region, sequence) {
            endpoint.region.set_client_state(SHARED_CLIENT_RUNNING);
            return;
        }
        let samples = self.channel_map.extract_from_physical(
            self.period_frames,
            self.physical_channels,
            physical,
            &mut self.scratch,
        );
        let over_capacity = self.capture_capacity_slots < endpoint.slot_count
            && endpoint.region.ready_capture_slots() >= self.capture_capacity_slots;
        if over_capacity
            || !endpoint
                .region
                .try_publish_capture(&mut self.index, sequence, samples)
        {
            timeline.record_shared_overrun();
            endpoint.region.record_capture_discontinuity();
        }
        endpoint.events.notify_capture();
    }
}

impl DaemonCaptureBridge {
    /// # Safety
    /// Caller must hold active pins on both `self.pro_domain` and `self.pro_capture_domain`.
    unsafe fn activate_pair_pinned(
        &mut self,
        capture_sequence: u64,
        playback_sequence: u64,
        generation: u64,
    ) -> u64 {
        if self.pair_start.load(Ordering::Acquire) == 0 {
            return 0;
        }
        // Keep both domain pins through clearing the pending bits. Stop/close
        // drain these pins before resetting or reopening a lifecycle, so this
        // activation cannot clear a request belonging to a replacement session.
        let p = unsafe { self.pro_endpoint.load_pinned(&self.pro_domain) };
        let c = unsafe {
            self.directional_endpoint
                .load_pinned(&self.pro_capture_domain)
        };
        let active = [
            self.pro_endpoint.hot.active.load(Ordering::Acquire),
            self.directional_endpoint.hot.active.load(Ordering::Acquire),
        ];
        let lifecycle_hardware = [
            self.pro_endpoint
                .hot
                .lifecycle_hardware_generation
                .load(Ordering::Acquire),
            self.directional_endpoint
                .hot
                .lifecycle_hardware_generation
                .load(Ordering::Acquire),
        ];
        let mask = self.pair_start.load(Ordering::Acquire);
        let endpoints = [p, c];
        let eligible = std::array::from_fn::<_, 2, _>(|i| {
            mask & (1 << i) != 0
                && active[i] != 0
                && active[i] == endpoints[i].session_id
                && lifecycle_hardware[i] == generation
        });
        let pair_owners = [&self.pro_endpoint.owner, &self.directional_endpoint.owner];
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
            let peer_owner = pair_owners[i ^ 1].load(Ordering::Acquire);
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

    /// # Safety
    /// Caller must hold an active pin on `self.pro_domain`.
    unsafe fn publish_pro_capture_pinned(
        &mut self,
        playback_sequence: u64,
        hardware_generation: u64,
        capture: &[i32],
    ) {
        let endpoint = unsafe { self.pro_endpoint.load_pinned(&self.pro_domain) };
        endpoint.region.set_hardware_generation(hardware_generation);
        endpoint.region.set_cycle_sequence(playback_sequence);
        let session_id = self.pro_endpoint.hot.active.load(Ordering::Acquire);
        if session_id == 0 || endpoint.session_id != session_id {
            return;
        }
        // Check after active: seeing a newly started session must also observe
        // the preceding pair-start barrier, even if this cycle entered earlier.
        if self.pair_start.load(Ordering::Acquire) & 1 != 0 {
            return;
        }
        if !endpoint.has_capture {
            if self
                .pro_endpoint
                .hot
                .lifecycle_hardware_generation
                .load(Ordering::Acquire)
                == hardware_generation
                && endpoint.region.client_state() != SHARED_CLIENT_IDLE
            {
                let _ = ensure_activation(&endpoint.region, playback_sequence);
            }
            // Playback-only clients wake at capture-ready, before the hardware waits.
            endpoint.events.notify_playback();
            return;
        }
        if !ensure_activation(&endpoint.region, playback_sequence)
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
                endpoint.region.record_capture_discontinuity();
            }
        }
    }

    fn publish_shared_capture(&mut self, hardware_sequence: u64, capture: &[i32]) {
        if self.shared.is_empty() {
            return;
        }
        let pin = self.shared_capture_domain.enter(RT_SLOT_CAPTURE);
        let hardware_generation = self.timeline.generation();
        for port in &mut self.shared {
            // SAFETY: `pin` holds `shared_capture_domain` slot `RT_SLOT_CAPTURE`.
            unsafe {
                port.process_capture_pinned(
                    hardware_sequence,
                    capture,
                    hardware_generation,
                    &self.timeline,
                    &self.shared_capture_domain,
                );
            }
        }
        drop(pin);
    }

    /// # Safety
    /// Caller must hold an active pin on `self.pro_capture_domain`.
    unsafe fn publish_directional_capture_pinned(
        &mut self,
        sequence: u64,
        hardware_generation: u64,
        capture: &[i32],
    ) {
        let endpoint = unsafe {
            self.directional_endpoint
                .load_pinned(&self.pro_capture_domain)
        };
        endpoint.region.set_hardware_generation(hardware_generation);
        endpoint.region.set_cycle_sequence(sequence);
        let session_id = self.directional_endpoint.hot.active.load(Ordering::Acquire);
        if session_id == 0 || endpoint.session_id != session_id {
            return;
        }
        if self.pair_start.load(Ordering::Acquire) & 2 != 0
            || endpoint.region.client_state() == SHARED_CLIENT_IDLE
        {
            return;
        }
        if self
            .directional_endpoint
            .hot
            .lifecycle_hardware_generation
            .load(Ordering::Acquire)
            != hardware_generation
        {
            endpoint.events.notify_capture();
            return;
        }
        if ensure_activation(&endpoint.region, sequence) {
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
        {
            let guard = CapturePairGuard::enter(self);
            let hardware_generation = guard.bridge.timeline.generation();
            // SAFETY: `guard` holds both `pro_domain` and `pro_capture_domain`.
            let activated = unsafe {
                guard
                    .bridge
                    .activate_pair_pinned(sequence, sequence, hardware_generation)
            };
            if activated & 1 == 0 {
                unsafe {
                    guard
                        .bridge
                        .publish_pro_capture_pinned(sequence, hardware_generation, capture);
                }
            }
            if activated & 2 == 0 {
                unsafe {
                    guard.bridge.publish_directional_capture_pinned(
                        sequence,
                        hardware_generation,
                        capture,
                    );
                }
            }
        }
        self.publish_shared_capture(sequence, capture);
    }

    fn process_capture_for_playback(
        &mut self,
        hardware_sequence: u64,
        playback_sequence: u64,
        capture: &[i32],
    ) {
        let guard = CapturePairGuard::enter(self);
        let hardware_generation = guard.bridge.timeline.generation();
        // SAFETY: `guard` holds both `pro_domain` and `pro_capture_domain`.
        let activated = unsafe {
            guard.bridge.activate_pair_pinned(
                hardware_sequence,
                playback_sequence,
                hardware_generation,
            )
        };
        if activated & 2 == 0 {
            unsafe {
                guard.bridge.publish_directional_capture_pinned(
                    hardware_sequence,
                    hardware_generation,
                    capture,
                );
            }
        }
        if activated & 1 == 0 {
            unsafe {
                guard.bridge.publish_pro_capture_pinned(
                    playback_sequence,
                    hardware_generation,
                    capture,
                );
            }
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
        let _pin = (!self.commit_pinned).then(|| self.pro_domain.enter(RT_SLOT_PLAYBACK));
        // SAFETY: `pro_domain` slot `RT_SLOT_PLAYBACK` is held by `_pin` or `commit_pinned`.
        let endpoint = unsafe { self.pro_endpoint.load_pinned(&self.pro_domain) };
        synchronize_pro_gate(&self.pro_endpoint, endpoint, &self.timeline, sequence, true);
    }

    fn prepare_playback_mix(&mut self, sequence: u64) {
        let defer = self.defer_playback_notifications;
        let _pin =
            (!self.commit_pinned).then(|| self.shared_playback_domain.enter(RT_SLOT_PLAYBACK));
        let hardware_generation = self.timeline.generation();
        let mut any_prepared = false;
        let mut any_pending_notify = false;
        for port in &mut self.shared {
            // SAFETY: `shared_playback_domain` slot `RT_SLOT_PLAYBACK` is held by `_pin` or `commit_pinned`.
            unsafe {
                port.prepare_playback_pinned(
                    sequence,
                    hardware_generation,
                    &self.timeline,
                    defer,
                    &self.shared_playback_domain,
                );
            }
            any_prepared |= port.prepared.is_some();
            any_pending_notify |= port.pending_notify;
        }
        self.any_prepared_shared = any_prepared;
        self.any_pending_shared_notify |= any_pending_notify;
        self.prepared_shared_sequence = Some(sequence);
    }

    fn wait_for_playback_before(&mut self, sequence: u64, cutoff_nanos: u64) {
        let diagnostics_enabled = self.pro_diagnostics.enabled.load(Ordering::Relaxed);
        if !diagnostics_enabled && self.pro_endpoint.hot.active.load(Ordering::Acquire) == 0 {
            self.pro_wait_timing = None;
            return;
        }
        let entry_now = diagnostics_enabled.then(monotonic_nanos);
        let _pin = (!self.commit_pinned).then(|| self.pro_domain.enter(RT_SLOT_PLAYBACK));
        // SAFETY: `pro_domain` slot `RT_SLOT_PLAYBACK` is held by `_pin` or `commit_pinned`.
        let endpoint = unsafe { self.pro_endpoint.load_pinned(&self.pro_domain) };
        let session_id = self.pro_endpoint.hot.active.load(Ordering::Acquire);
        let hardware_generation = self.timeline.generation();
        self.pro_wait_timing = entry_now.map(|now| {
            (
                ProPlaybackIdentity {
                    session_id,
                    lifecycle_generation: self
                        .pro_endpoint
                        .hot
                        .lifecycle_generation
                        .load(Ordering::Acquire),
                    hardware_generation,
                },
                sequence,
                cutoff_nanos,
                cutoff_nanos.saturating_sub(now),
            )
        });
        if session_id == 0
            || endpoint.session_id != session_id
            || !endpoint.region.activation_ready()
        {
            return;
        }
        let client_state = endpoint.region.client_state();
        if client_state == SHARED_CLIENT_IDLE
            || (client_state == SHARED_CLIENT_STARTING
                && endpoint.region.start_sequence() == sequence)
            || self
                .pro_endpoint
                .hot
                .lifecycle_hardware_generation
                .load(Ordering::Acquire)
                != hardware_generation
            || endpoint.region.has_ready_playback(sequence)
        {
            return;
        }

        let wait_started_nanos = entry_now.unwrap_or_else(monotonic_nanos);
        let mut now = wait_started_nanos;
        loop {
            if cutoff_nanos <= now {
                break;
            }
            endpoint.events.drain_playback_ready();
            if self.pro_wait_should_stop(endpoint, session_id, hardware_generation, sequence) {
                break;
            }
            match endpoint.events.wait_playback_ready_before(cutoff_nanos) {
                PlaybackReadyWait::Ready | PlaybackReadyWait::Interrupted => {
                    if self.pro_wait_should_stop(
                        endpoint,
                        session_id,
                        hardware_generation,
                        sequence,
                    ) {
                        break;
                    }
                    now = monotonic_nanos();
                }
                PlaybackReadyWait::Timeout => break,
                PlaybackReadyWait::Failed => {
                    self.core_miss_pro_sequence = Some(sequence);
                    break;
                }
            }
        }
        self.timeline
            .record_pro_ready_wait(monotonic_nanos().saturating_sub(wait_started_nanos));
    }

    fn mark_playback_budget_exhausted(&mut self, sequence: u64) {
        self.core_miss_pro_sequence = Some(sequence);
    }

    fn begin_playback_commit(&mut self) {
        self.playback_commits.begin_unflushed();
        if !self.commit_pinned {
            self.pro_domain.begin_unflushed(RT_SLOT_PLAYBACK);
            self.shared_playback_domain
                .begin_unflushed(RT_SLOT_PLAYBACK);
            self.commit_pinned = true;
        }
        fence(Ordering::SeqCst);
        self.defer_playback_notifications = true;
    }

    fn end_playback_commit(&mut self) {
        // Flush deferred client wakeups after the ALSA playback write completes,
        // while the commit barrier and RT pins still keep endpoints alive.
        self.defer_playback_notifications = false;
        let pro_pin = (self.pending_pro_notify && !self.commit_pinned)
            .then(|| self.pro_domain.enter(RT_SLOT_PLAYBACK));
        if self.pending_pro_notify {
            self.pending_pro_notify = false;
            // SAFETY: `pro_domain` slot `RT_SLOT_PLAYBACK` is held by `pro_pin` or `commit_pinned`.
            unsafe { self.pro_endpoint.load_pinned(&self.pro_domain) }
                .events
                .notify_playback();
        }
        drop(pro_pin);
        let shared_pin = (self.any_pending_shared_notify && !self.commit_pinned)
            .then(|| self.shared_playback_domain.enter(RT_SLOT_PLAYBACK));
        if self.any_pending_shared_notify {
            self.any_pending_shared_notify = false;
            for port in &mut self.shared {
                // SAFETY: `shared_playback_domain` slot `RT_SLOT_PLAYBACK` is held by `shared_pin` or `commit_pinned`.
                unsafe { port.flush_pending_notify_pinned(&self.shared_playback_domain) };
            }
        }
        drop(shared_pin);
        if self.commit_pinned {
            self.commit_pinned = false;
            self.pro_domain.exit(RT_SLOT_PLAYBACK);
            self.shared_playback_domain.exit(RT_SLOT_PLAYBACK);
        }
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
            if !self.any_prepared_shared {
                return;
            }
            self.any_prepared_shared = false;
            let _pin =
                (!self.commit_pinned).then(|| self.shared_playback_domain.enter(RT_SLOT_PLAYBACK));
            let hardware_generation = self.timeline.generation();
            for port in &mut self.shared {
                // SAFETY: `shared_playback_domain` slot `RT_SLOT_PLAYBACK` is held by `_pin` or `commit_pinned`.
                unsafe {
                    port.commit_prepared_pinned(
                        sequence,
                        playback,
                        hardware_generation,
                        &self.shared_playback_domain,
                    );
                }
            }
        } else {
            self.any_prepared_shared = false;
            if self.shared.is_empty() {
                return;
            }
            let defer = self.defer_playback_notifications;
            let _pin =
                (!self.commit_pinned).then(|| self.shared_playback_domain.enter(RT_SLOT_PLAYBACK));
            let hardware_generation = self.timeline.generation();
            let mut any_pending_notify = false;
            for port in &mut self.shared {
                // SAFETY: `shared_playback_domain` slot `RT_SLOT_PLAYBACK` is held by `_pin` or `commit_pinned`.
                unsafe {
                    port.process_playback_pinned(
                        sequence,
                        playback,
                        hardware_generation,
                        &self.timeline,
                        defer,
                        &self.shared_playback_domain,
                    );
                }
                any_pending_notify |= port.pending_notify;
            }
            self.any_pending_shared_notify |= any_pending_notify;
        }
    }
}

#[inline]
fn notify_or_defer(pending: &mut bool, endpoint: &SessionEndpoint, defer: bool) {
    if defer {
        *pending = true;
    } else {
        endpoint.events.notify_playback();
    }
}

#[inline]
fn copy_and_check_nonzero(dst: &mut [i32], src: &[i32]) -> bool {
    assert_eq!(dst.len(), src.len());
    let mut acc = [0_i32; 8];
    let (dst_chunks, dst_rem) = dst.as_chunks_mut::<8>();
    let (src_chunks, src_rem) = src.as_chunks::<8>();
    for (d, s) in dst_chunks.iter_mut().zip(src_chunks) {
        *d = *s;
        for i in 0..8 {
            acc[i] |= s[i];
        }
    }
    let mut tail = 0_i32;
    for (d, &s) in dst_rem.iter_mut().zip(src_rem) {
        *d = s;
        tail |= s;
    }
    (acc[0] | acc[1] | acc[2] | acc[3] | acc[4] | acc[5] | acc[6] | acc[7] | tail) != 0
}

impl DaemonPlaybackBridge {
    #[inline]
    fn pro_wait_should_stop(
        &self,
        endpoint: &SessionEndpoint,
        session_id: u64,
        hardware_generation: u64,
        sequence: u64,
    ) -> bool {
        if self.pro_endpoint.hot.active.load(Ordering::Acquire) != session_id {
            return true;
        }
        let client_state = endpoint.region.client_state();
        if client_state == SHARED_CLIENT_IDLE
            || (client_state == SHARED_CLIENT_STARTING
                && endpoint.region.start_sequence() == sequence)
        {
            return true;
        }
        self.timeline.generation() != hardware_generation
            || self
                .pro_endpoint
                .hot
                .lifecycle_hardware_generation
                .load(Ordering::Acquire)
                != hardware_generation
            || endpoint.region.has_ready_playback(sequence)
    }

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
        let _pin = (!self.commit_pinned).then(|| self.pro_domain.enter(RT_SLOT_PLAYBACK));
        // SAFETY: `pro_domain` slot `RT_SLOT_PLAYBACK` is held by `_pin` or `commit_pinned`.
        let endpoint = unsafe { self.pro_endpoint.load_pinned(&self.pro_domain) };
        let gate = synchronize_pro_gate(
            &self.pro_endpoint,
            endpoint,
            &self.timeline,
            sequence,
            !self.defer_playback_notifications,
        );
        if gate.blocked && self.defer_playback_notifications {
            self.pending_pro_notify = true;
        }
        let core_deadline_miss = self.core_miss_pro_sequence.take() == Some(sequence);
        let session_id = gate.session_id;
        if gate.blocked || session_id == 0 || endpoint.session_id != session_id {
            playback.fill(0);
            self.last_valid_pro_identity = None;
            endpoint
                .region
                .set_playback_sequence(sequence.wrapping_add(1));
            return;
        }
        let identity = ProPlaybackIdentity {
            session_id,
            lifecycle_generation: self
                .pro_endpoint
                .hot
                .lifecycle_generation
                .load(Ordering::Acquire),
            hardware_generation: gate.hardware_generation,
        };
        if self.last_valid_pro_identity != Some(identity) {
            self.last_valid_pro_identity = None;
        }
        let client_state = endpoint.region.client_state();
        if endpoint.region.activation_ready() && client_state != SHARED_CLIENT_IDLE {
            let (outcome, published_nanos) = if client_state == SHARED_CLIENT_RUNNING {
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
            let identity_is_current = self.pro_endpoint.hot.active.load(Ordering::Acquire)
                == session_id
                && self
                    .pro_endpoint
                    .hot
                    .lifecycle_generation
                    .load(Ordering::Acquire)
                    == identity.lifecycle_generation
                && self.timeline.generation() == gate.hardware_generation;
            if consumed && identity_is_current {
                let sample_count = self.last_valid_pro.len();
                if playback.len() > sample_count {
                    playback[sample_count..].fill(0);
                }
                if let Some(elapsed) = capture_to_publish_nanos {
                    self.pro_diagnostics.record_accepted(elapsed);
                }
                let nonzero =
                    copy_and_check_nonzero(&mut self.last_valid_pro, &playback[..sample_count]);
                self.timeline.record_pro_playback_block(nonzero);
                self.last_valid_pro_identity = Some(identity);
                self.pro_repeated_periods = 0;
                if self.pro_endpoint.hot.armed.load(Ordering::Acquire) != session_id {
                    self.pro_endpoint
                        .hot
                        .warmup_blocks
                        .store(1, Ordering::Release);
                    let _ = self.pro_endpoint.hot.armed.compare_exchange(
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
                && self.pro_endpoint.hot.armed.load(Ordering::Acquire) == session_id
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
                if self.last_valid_pro_identity == Some(identity)
                    && self.pro_repeated_periods < MAX_REPEATED_PLAYBACK_PERIODS
                {
                    self.pro_repeated_periods += 1;
                    let sample_count = self.last_valid_pro.len();
                    playback[..sample_count].copy_from_slice(&self.last_valid_pro);
                    if playback.len() > sample_count {
                        playback[sample_count..].fill(0);
                    }
                } else {
                    playback.fill(0);
                }
            } else {
                playback.fill(0);
                if client_state == SHARED_CLIENT_STARTING
                    || endpoint.region.client_state() == SHARED_CLIENT_STARTING
                {
                    self.pro_endpoint
                        .hot
                        .warmup_blocks
                        .store(0, Ordering::Release);
                    self.last_valid_pro_identity = None;
                }
            }
            // Publish the consumed watermark before waking the client so it never
            // observes the stale sequence after poll returns.
            endpoint
                .region
                .set_playback_sequence(sequence.wrapping_add(1));
            notify_or_defer(
                &mut self.pending_pro_notify,
                endpoint,
                self.defer_playback_notifications,
            );
        } else {
            playback.fill(0);
            self.last_valid_pro_identity = None;
            endpoint
                .region
                .set_playback_sequence(sequence.wrapping_add(1));
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
        time::{Duration, Instant},
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
        let session = if state.pro.endpoint.owner.load(Ordering::Acquire) == session_id {
            &state.pro
        } else {
            &state
                .shared
                .iter()
                .find(|port| port.session.endpoint.owner.load(Ordering::Acquire) == session_id)
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
            .endpoint
            .hot
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
            .endpoint
            .hot
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
                .endpoint
                .hot
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
            .endpoint
            .hot
            .lifecycle_hardware_generation
            .store(u64::MAX, Ordering::Release);
        state
            .pro_capture
            .endpoint
            .hot
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
                .endpoint
                .hot
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
        let session = SessionState::new(4, 2, 2, 8, Arc::new(RtReadDomain::default()), None, None)
            .expect("session state should create");

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
    fn playback_commit_defers_client_notifications_until_after_write() {
        fn fd_readable(fd: std::os::fd::RawFd) -> bool {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut pfd, 1, 0) > 0 && pfd.revents & libc::POLLIN != 0 }
        }

        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let pro = open_pro(&state).0;
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(pro));
        assert!(state.start(shared.session_id));
        activate_session(&state, pro, 10);
        activate_session(&state, shared.session_id, 10);

        let pro_fd = state.pro.current().events.playback_fd();
        let shared_fd = state.shared[0].session.current().events.playback_fd();
        state.pro.current().events.drain();
        state.shared[0].session.current().events.drain();

        let mut pro_index = 0;
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut pro_index,
            11,
            &[42; 8]
        ));
        let mut shared_index = 0;
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut shared_index, 11, &[10; 8])
        );

        let (_, mut playback) = state.bridges();
        // Dirty buffer verifies active PRO hit overwrites cleanly without pre-zeroing.
        let mut output = [999; 8];
        playback.begin_playback_commit();
        playback.process_playback_before(11, u64::MAX, &mut output);
        playback.prepare_playback_mix(11);
        playback.commit_playback(11, &mut output);
        assert_eq!(output, [52; 8]);
        assert!(!fd_readable(pro_fd), "PRO wakeup must wait for ALSA write");
        assert!(
            !fd_readable(shared_fd),
            "SHARED wakeup must wait for ALSA write"
        );

        playback.end_playback_commit();
        assert!(fd_readable(pro_fd), "PRO wakeup must flush on commit end");
        assert!(
            fd_readable(shared_fd),
            "SHARED wakeup must flush on commit end"
        );

        // Inactive/miss path without repeat still zeroes a dirty output buffer.
        assert!(state.stop(pro));
        let mut dirty = [777; 8];
        playback.process_playback(12, &mut dirty);
        assert_eq!(dirty, [0; 8]);
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
        assert_eq!(
            state.shared[0]
                .session
                .endpoint
                .hot
                .armed
                .load(Ordering::Acquire),
            0
        );
        assert_eq!(
            state.shared[0]
                .session
                .endpoint
                .hot
                .outage
                .load(Ordering::Acquire),
            0
        );
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
    fn shared_playback_repeat_is_bounded_for_a_stalled_client() {
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
                .try_client_publish_playback(&mut producer_index, 0, &[10; 8])
        );
        let mut bridge = state.bridge();
        let mut output = [0; 8];
        bridge.process(0, &[0; 8], &mut output);
        assert_eq!(output, [10; 8]);
        let limit = u64::from(MAX_REPEATED_PLAYBACK_PERIODS);
        for sequence in 1..=limit {
            bridge.process(sequence, &[0; 8], &mut output);
            assert_eq!(output, [10; 8]);
        }
        bridge.process(limit + 1, &[0; 8], &mut output);
        assert_eq!(output, [0; 8]);

        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, limit + 2, &[20; 8])
        );
        bridge.process(limit + 2, &[0; 8], &mut output);
        assert_eq!(output, [20; 8]);
        bridge.process(limit + 3, &[0; 8], &mut output);
        assert_eq!(output, [20; 8]);
    }

    #[test]
    fn pro_playback_repeat_is_bounded_for_a_stalled_client() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let pro_session = open_pro(&state).0;
        assert!(state.start(pro_session));
        activate_session(&state, pro_session, 9);

        let (_, mut playback) = state.bridges();
        let mut pro_index = 0;
        assert!(state.pro.current().region.try_client_publish_playback(
            &mut pro_index,
            10,
            &[100; 8],
        ));
        let mut output = [0; 8];
        playback.process_playback(10, &mut output);
        playback.commit_playback(10, &mut output);
        assert_eq!(output, [100; 8]);

        let limit = u64::from(MAX_REPEATED_PLAYBACK_PERIODS);
        for sequence in 11..=10 + limit {
            playback.process_playback(sequence, &mut output);
            playback.commit_playback(sequence, &mut output);
            assert_eq!(output, [100; 8]);
        }
        playback.process_playback(11 + limit, &mut output);
        playback.commit_playback(11 + limit, &mut output);
        assert_eq!(output, [0; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, limit + 1);
    }

    #[test]
    fn hardware_generation_change_blocks_cache_but_not_shared_playback() {
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

        // The client keeps its lifecycle: no repeat crosses the restart, new blocks still play.
        playback.shared[0].observed_hardware_generation = Some(u64::MAX);
        state.shared[0]
            .session
            .endpoint
            .hot
            .lifecycle_hardware_generation
            .store(u64::MAX, Ordering::Release);
        playback.process_playback(1, &mut output);
        playback.commit_playback(1, &mut output);
        assert_eq!(output, [0; 8]);
        assert!(
            state.shared[0]
                .session
                .current()
                .region
                .try_client_publish_playback(&mut producer_index, 2, &[99; 8])
        );
        playback.process_playback(2, &mut output);
        playback.commit_playback(2, &mut output);
        assert_eq!(output, [99; 8]);
    }

    #[test]
    fn shared_start_during_hardware_recovery_still_activates() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let timeline = Arc::new(HardwareTimeline::default());
        let state = DaemonState::new(&profile, Arc::clone(&timeline)).expect("state should create");
        let shared = state
            .open_shared("line1")
            .expect("port should exist")
            .expect("shared port should open");
        assert!(state.start(shared.session_id));
        // The generation moved between START and the first cycle. A client waiting for
        // activation has a frozen pointer, so nothing else would ever restart it.
        state.shared[0]
            .session
            .endpoint
            .hot
            .lifecycle_hardware_generation
            .store(u64::MAX, Ordering::Release);

        let (_, mut playback) = state.bridges();
        let mut output = [0; 8];
        playback.process_playback(0, &mut output);
        playback.commit_playback(0, &mut output);

        assert!(state.shared[0].session.current().region.activation_ready());
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
            .endpoint
            .hot
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
            .endpoint
            .hot
            .lifecycle_hardware_generation
            .store(u64::MAX, Ordering::Release);
        playback.process_playback(12, &mut output);
        assert_eq!(output, [0; 8]);
        assert_eq!(timeline.snapshot().pro_deadline_misses, 0);
        assert_eq!(state.pro.endpoint.hot.armed.load(Ordering::Acquire), 0);

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
            .endpoint
            .hot
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
                .endpoint
                .hot
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

    const E1X2_BENCH_PROFILE: &str = r#"
        [device]
        name = "Topping E1x2 OTG"
        rate = 48000
        period_size = 64
        buffer_size = 256
        shared_buffer_size = 512
        duplex_link = true
        pro_latency_periods = 0
        pro_handoff_us = 1000
        shared_latency_periods = 0
        shared_playback_repeat_on_underrun = false

        [device.playback]
        device = "hw:OTG,0"
        channels = 8
        format = "S32_LE"

        [device.capture]
        device = "hw:OTG,0"
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

        [[ports.capture]]
        id = "input56"
        name = "Input 5/6"
        channels = [4, 5]

        [[ports.capture]]
        id = "input78"
        name = "Input 7/8"
        channels = [6, 7]

        [[ports.capture]]
        id = "input910"
        name = "Input 9/10"
        channels = [8, 9]
    "#;

    #[test]
    #[ignore = "release-mode daemon RT bridge microbenchmark; no audio hardware"]
    fn benchmark_daemon_rt_bridge_e1x2() {
        let profile = Profile::from_toml(E1X2_BENCH_PROFILE).expect("profile should parse");
        let capture = vec![0x1234_5600_i32; 64 * 10];
        let mut playback = vec![0_i32; 64 * 8];

        // 1. Idle E1x2 RT cycle (all 12 slots inactive, tracks sequence across all regions).
        {
            let timeline = Arc::new(HardwareTimeline::default());
            let state = DaemonState::new(&profile, timeline).expect("state should create");
            let (mut cap, mut play) = state.bridges();
            let iters = 100_000_u64;
            for seq in 0..1_000_u64 {
                play.prepare_playback(seq);
                cap.process_capture_for_playback(seq, seq, &capture);
                play.wait_for_playback_before(seq, u64::MAX);
                play.begin_playback_commit();
                play.process_playback_before(seq, u64::MAX, &mut playback);
                play.prepare_playback_mix(seq);
                play.commit_playback(seq, &mut playback);
                play.end_playback_commit();
                cap.process_deferred_capture(seq, &capture);
            }
            let started = Instant::now();
            for seq in 1_000..(1_000 + iters) {
                play.prepare_playback(seq);
                cap.process_capture_for_playback(seq, seq, &capture);
                play.wait_for_playback_before(seq, u64::MAX);
                play.begin_playback_commit();
                play.process_playback_before(seq, u64::MAX, &mut playback);
                play.prepare_playback_mix(seq);
                play.commit_playback(seq, &mut playback);
                play.end_playback_commit();
                cap.process_deferred_capture(seq, &capture);
            }
            let elapsed = started.elapsed();
            std::hint::black_box(&playback);
            eprintln!(
                "bench_idle_e1x2_cycle: {:.1} ns/period ({iters} iters in {:?})",
                elapsed.as_nanos() as f64 / iters as f64,
                elapsed
            );
        }

        // 2. Active E1x2 RT cycle: PRO + 2 shared playback (line1, line2) + 2 shared capture (mic1, input34).
        let mut run_active_bench = |label: &str, with_stats_contention: bool, silent_pro: bool| {
            let timeline = Arc::new(HardwareTimeline::default());
            let state =
                Arc::new(DaemonState::new(&profile, timeline).expect("state should create"));
            let pro = open_pro(&state).0;
            let line1 = state.open_shared("line1").unwrap().unwrap().session_id;
            let line2 = state.open_shared("line2").unwrap().unwrap().session_id;
            let mic1 = state.open_shared("mic1").unwrap().unwrap().session_id;
            let in34 = state.open_shared("input34").unwrap().unwrap().session_id;
            assert!(state.start(pro));
            assert!(state.start(line1));
            assert!(state.start(line2));
            assert!(state.start(mic1));
            assert!(state.start(in34));
            activate_session(&state, pro, 0);
            activate_session(&state, line1, 0);
            activate_session(&state, line2, 0);
            activate_session(&state, mic1, 0);
            activate_session(&state, in34, 0);

            let (mut cap, mut play) = state.bridges();
            let pro_samples = vec![if silent_pro { 0_i32 } else { 0x0010_0000_i32 }; 64 * 8];
            let stereo_samples = vec![0x0002_0000_i32; 64 * 2];
            let mut pro_cap_out = vec![0_i32; 64 * 10];
            let mut mic1_out = vec![0_i32; 64];
            let mut in34_out = vec![0_i32; 64 * 2];
            let mut pro_idx = 0;
            let mut l1_idx = 0;
            let mut l2_idx = 0;
            let mut pro_cap_idx = 0;
            let mut mic1_cap_idx = 0;
            let mut in34_cap_idx = 0;

            let stop_stats = Arc::new(AtomicBool::new(false));
            let stats_thread = with_stats_contention.then(|| {
                let s = Arc::clone(&state);
                let stop = Arc::clone(&stop_stats);
                std::thread::spawn(move || {
                    let mut count = 0_u64;
                    while !stop.load(Ordering::Relaxed) {
                        std::hint::black_box(s.stats());
                        count += 1;
                    }
                    count
                })
            });

            let iters = 50_000_u64;
            let mut total_bridge_nanos = 0_u128;
            let pro_guard = state.pro.current();
            let l1_guard = state.shared[0].session.current();
            let l2_guard = state.shared[1].session.current();
            let mic1_guard = state.shared[4].session.current();
            let in34_guard = state.shared[6].session.current();
            for seq in 1..=(1_000 + iters) {
                assert!(pro_guard.region.try_client_publish_playback(
                    &mut pro_idx,
                    seq,
                    &pro_samples
                ));
                assert!(l1_guard.region.try_client_publish_playback(
                    &mut l1_idx,
                    seq,
                    &stereo_samples
                ));
                assert!(l2_guard.region.try_client_publish_playback(
                    &mut l2_idx,
                    seq,
                    &stereo_samples
                ));

                let t0 = Instant::now();
                play.prepare_playback(seq);
                cap.process_capture_for_playback(seq, seq, &capture);
                play.wait_for_playback_before(seq, u64::MAX);
                play.begin_playback_commit();
                play.process_playback_before(seq, u64::MAX, &mut playback);
                play.prepare_playback_mix(seq);
                play.commit_playback(seq, &mut playback);
                play.end_playback_commit();
                cap.process_deferred_capture(seq, &capture);
                if seq > 1_000 {
                    total_bridge_nanos += t0.elapsed().as_nanos();
                }

                // Drain client capture slots so capture rings stay steady-state.
                let _ = pro_guard
                    .region
                    .try_client_read_capture(&mut pro_cap_idx, &mut pro_cap_out);
                let _ = mic1_guard
                    .region
                    .try_client_read_capture(&mut mic1_cap_idx, &mut mic1_out);
                let _ = in34_guard
                    .region
                    .try_client_read_capture(&mut in34_cap_idx, &mut in34_out);
                if seq % 256 == 0 {
                    pro_guard.events.drain();
                    l1_guard.events.drain();
                    l2_guard.events.drain();
                    mic1_guard.events.drain();
                    in34_guard.events.drain();
                }
            }
            drop((pro_guard, l1_guard, l2_guard, mic1_guard, in34_guard));
            stop_stats.store(true, Ordering::Relaxed);
            let stats_polls = stats_thread.map(|h| h.join().unwrap()).unwrap_or(0);
            std::hint::black_box(&playback);
            eprintln!(
                "{label}: {:.1} ns/period ({iters} iters, stats_polls={stats_polls})",
                total_bridge_nanos as f64 / iters as f64,
            );
        };

        run_active_bench("bench_active_e1x2_nonzero", false, false);
        run_active_bench("bench_active_e1x2_silent", false, true);
        run_active_bench("bench_active_e1x2_stats_contention", true, false);
    }

    #[test]
    fn endpoint_slot_hot_line_is_isolated_from_control_fields() {
        assert_eq!(size_of::<SessionHot>(), 64);
        assert!(std::mem::offset_of!(EndpointSlot, reader_epoch) >= 64);
        assert_eq!(std::mem::align_of::<PlaybackCommitBarrier>(), 64);
    }

    #[test]
    fn rt_read_domain_waits_for_earlier_pin_and_ignores_later_pins() {
        let domain = Arc::new(RtReadDomain::default());
        let earlier = domain.enter(RT_SLOT_CAPTURE);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiting = Arc::clone(&domain);
        let worker = std::thread::spawn(move || {
            waiting.wait_for_idle();
            done_tx.send(()).expect("completion should send");
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while domain.epoch.0.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "domain epoch should advance");
            std::thread::yield_now();
        }
        let later = domain.enter(RT_SLOT_PLAYBACK);
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(earlier);
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("later pin must not extend the drain");
        drop(later);
        worker.join().expect("drain worker should not panic");
    }

    #[test]
    fn dropping_playback_bridge_during_commit_releases_rt_pins() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = Arc::new(
            DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
                .expect("state should create"),
        );
        let session = open_pro(&state).0;
        assert!(state.start(session));
        let (_, mut playback) = state.bridges();
        playback.begin_playback_commit();
        // End the commit barrier counter explicitly while simulating a drop that skipped
        // clearing the domain pins, or drop before stop verifies pin release.
        playback.playback_commits.end();
        drop(playback);
        assert!(state.stop(session));
    }

    #[test]
    fn channel_map_all_variants_match_reference_mix_and_extract() {
        let cases: &[(usize, &[usize])] = &[
            (1, &[0]),
            (2, &[0, 1]),
            (8, &[0, 1, 2, 3, 4, 5, 6, 7]),
            (8, &[0]),
            (8, &[7]),
            (8, &[0, 1]),
            (8, &[6, 7]),
            (8, &[2, 3, 4, 5]),
            (8, &[7, 0, 3]),
            (10, &[8, 9]),
        ];
        let period_frames = 17;
        for &(physical_channels, channels) in cases {
            let map = ChannelMap::from_channels(channels, physical_channels);
            let mut physical: Vec<i32> = (0..period_frames * physical_channels)
                .map(|i| match i % 5 {
                    0 => i32::MAX - 10,
                    1 => i32::MIN + 10,
                    _ => (i as i32) * 13 - 500,
                })
                .collect();
            let logical: Vec<i32> = (0..period_frames * channels.len())
                .map(|i| match i % 4 {
                    0 => 100,
                    1 => -100,
                    _ => (i as i32) * 7 - 200,
                })
                .collect();

            // Reference mix.
            let mut expected_mix = physical.clone();
            for frame in 0..period_frames {
                for (l, &p) in channels.iter().enumerate() {
                    let idx = frame * physical_channels + p;
                    expected_mix[idx] =
                        expected_mix[idx].saturating_add(logical[frame * channels.len() + l]);
                }
            }
            map.mix_into_physical(period_frames, physical_channels, &logical, &mut physical);
            assert_eq!(
                physical, expected_mix,
                "mix mismatch for phys={physical_channels} ch={channels:?}"
            );

            // Reference extract.
            let mut expected_extract = vec![0_i32; period_frames * channels.len()];
            for frame in 0..period_frames {
                for (l, &p) in channels.iter().enumerate() {
                    expected_extract[frame * channels.len() + l] =
                        physical[frame * physical_channels + p];
                }
            }
            let mut scratch = vec![0_i32; period_frames * channels.len()];
            let extracted = map.extract_from_physical(
                period_frames,
                physical_channels,
                &physical,
                &mut scratch,
            );
            assert_eq!(
                extracted,
                expected_extract.as_slice(),
                "extract mismatch for phys={physical_channels} ch={channels:?}"
            );
            if matches!(map, ChannelMap::Identity) {
                assert_eq!(
                    extracted.as_ptr(),
                    physical.as_ptr(),
                    "Identity extract should avoid copying into scratch"
                );
            }
        }
    }

    #[test]
    fn copy_and_check_nonzero_handles_tails_and_boundaries() {
        for len in [0, 1, 7, 8, 9, 15, 16, 17, 512] {
            let zeros = vec![0_i32; len];
            let mut dst = vec![42_i32; len];
            assert!(!copy_and_check_nonzero(&mut dst, &zeros));
            assert_eq!(dst, zeros);

            for pos in [0, len / 2, len.saturating_sub(1)] {
                if len == 0 {
                    continue;
                }
                for val in [1_i32, -1, i32::MIN, i32::MAX] {
                    let mut src = vec![0_i32; len];
                    src[pos] = val;
                    let mut dst = vec![0_i32; len];
                    assert!(copy_and_check_nonzero(&mut dst, &src));
                    assert_eq!(dst, src);
                }
            }
        }
    }

    #[test]
    fn direct_pro_wait_ignores_stale_ready_notification_without_prepare_drain() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
            .expect("state should create");
        let (session, _, fds) = open_pro(&state);
        assert!(state.start(session));
        activate_session(&state, session, 9);

        // Simulate a stale playback_ready notification left over from a prior period.
        notify_fd(fds[3]);

        let (_, mut playback) = state.bridges();
        playback.prepare_playback(10);
        let (published_tx, published_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(10));
                let mut producer_index = 0;
                assert!(state.pro.current().region.try_client_publish_playback(
                    &mut producer_index,
                    10,
                    &[77; 8],
                ));
                published_tx.send(()).expect("publish signal should send");
                notify_fd(fds[3]);
            });
            playback.wait_for_playback_before(10, monotonic_nanos().saturating_add(1_000_000_000));
            published_rx
                .try_recv()
                .expect("stale event must be drained before sleeping for real publication");
        });
    }
}
