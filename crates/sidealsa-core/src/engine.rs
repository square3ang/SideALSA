use std::{
    hint, io,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use alsa::{
    Direction, ValueOr,
    pcm::{Access, Format, HwParams, PCM, State},
};
use thiserror::Error;

use crate::pro::{ProCaptureSink, ProPlaybackSource};
use crate::{
    HardwareConfig, HardwareStats, HardwareTimeline, MAX_PRO_LATENCY_PERIODS, Profile,
    RoutingError, RoutingTable, SampleFormat,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamDirection {
    Playback,
    Capture,
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("invalid hardware configuration: {0}")]
    InvalidConfig(String),
    #[error("realtime scheduling setup failed during {operation}: {source}")]
    RealtimeScheduling {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("ALSA {operation} failed for {direction:?}: {source}")]
    Alsa {
        operation: &'static str,
        direction: StreamDirection,
        #[source]
        source: alsa::Error,
    },
    #[error("ALSA XRUN during {operation} on {direction:?}: {source}")]
    Xrun {
        operation: &'static str,
        direction: StreamDirection,
        #[source]
        source: alsa::Error,
    },
    #[error("audio worker thread panicked")]
    WorkerPanic,
    #[error("failed to spawn {worker} audio worker: {source}")]
    WorkerSpawn {
        worker: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("hardware clock wait failed: {0}")]
    ClockWait(#[source] io::Error),
    #[error("{direction:?} committed {actual} frames, {required} required")]
    ShortCommit {
        direction: StreamDirection,
        actual: i64,
        required: i64,
    },
}

pub struct DuplexEngine {
    config: HardwareConfig,
    playback_pcm: Option<PCM>,
    capture_pcm: Option<PCM>,
    timeline: Arc<HardwareTimeline>,
    routing: RoutingTable,
    period: alsa::pcm::Frames,
    buffer: alsa::pcm::Frames,
    playback_period_samples: usize,
    capture_period_samples: usize,
    playback_start_samples: usize,
    playback_start_frames: alsa::pcm::Frames,
    playback_timer_scheduling: bool,
    duplex_link: bool,
    playback_channels: usize,
    capture_channels: usize,
    playback_silence: Vec<i32>,
    capture_scratch: Vec<i32>,
    started: bool,
}

impl DuplexEngine {
    pub fn open(profile: Profile) -> Result<Self, EngineError> {
        if profile.device.pro_latency_periods > MAX_PRO_LATENCY_PERIODS {
            return Err(EngineError::InvalidConfig(format!(
                "pro_latency_periods must be <= {MAX_PRO_LATENCY_PERIODS}"
            )));
        }
        if profile.device.pro_latency_periods == 0
            && (!profile.device.playback_timer_scheduling
                || profile.device.buffer_size < profile.device.period_size.saturating_mul(2))
        {
            return Err(EngineError::InvalidConfig(
                "zero PRO lead requires timer scheduling and at least two hardware periods".into(),
            ));
        }
        let routing = RoutingTable::compile(&profile).map_err(routing_error_to_engine_error)?;
        let duplex_link = profile.device.effective_duplex_link();
        let config = profile.device;
        ensure_supported_format(&config.playback.format, StreamDirection::Playback)?;
        ensure_supported_format(&config.capture.format, StreamDirection::Capture)?;

        let period = i64::from(config.period_size);
        let buffer = i64::from(config.buffer_size);
        let playback_queue = i64::from(
            config
                .playback_queue_periods
                .map_or(config.buffer_size, |periods| {
                    config.period_size.saturating_mul(periods)
                }),
        );
        let playback_avail_min = if config.playback_timer_scheduling {
            period
        } else {
            buffer - playback_queue + period
        };
        let playback_period_samples = sample_count(config.period_size, config.playback.channels)?;
        let capture_period_samples = sample_count(config.period_size, config.capture.channels)?;
        let playback_buffer_samples = sample_count(config.buffer_size, config.playback.channels)?;
        let playback_start_frames = if config.playback_timer_scheduling {
            buffer.min(period.saturating_mul(2))
        } else {
            buffer
        };
        let playback_start_samples = usize::try_from(playback_start_frames)
            .ok()
            .and_then(|frames| frames.checked_mul(config.playback.channels as usize))
            .ok_or_else(|| EngineError::InvalidConfig("playback start size overflow".into()))?;
        let playback_channels = usize::try_from(config.playback.channels)
            .map_err(|_| EngineError::InvalidConfig("playback channels do not fit usize".into()))?;
        let capture_channels = usize::try_from(config.capture.channels)
            .map_err(|_| EngineError::InvalidConfig("capture channels do not fit usize".into()))?;
        let playback_timer_scheduling = config.playback_timer_scheduling;

        let playback_pcm = open_pcm(
            &config.playback.device,
            Direction::Playback,
            StreamDirection::Playback,
        )?;
        configure_pcm(
            &playback_pcm,
            &config,
            &config.playback,
            StreamDirection::Playback,
        )?;
        configure_sw_params(
            &playback_pcm,
            if playback_timer_scheduling || duplex_link {
                i64::MAX
            } else {
                buffer
            },
            playback_avail_min,
            StreamDirection::Playback,
        )?;

        let capture_pcm = open_pcm(
            &config.capture.device,
            Direction::Capture,
            StreamDirection::Capture,
        )?;
        configure_pcm(
            &capture_pcm,
            &config,
            &config.capture,
            StreamDirection::Capture,
        )?;
        configure_sw_params(&capture_pcm, buffer, period, StreamDirection::Capture)?;

        Ok(Self {
            config,
            playback_pcm: Some(playback_pcm),
            capture_pcm: Some(capture_pcm),
            timeline: Arc::new(HardwareTimeline::default()),
            routing,
            period,
            buffer,
            playback_period_samples,
            capture_period_samples,
            playback_start_samples,
            playback_start_frames,
            playback_timer_scheduling,
            duplex_link,
            playback_channels,
            capture_channels,
            playback_silence: vec![0; playback_buffer_samples],
            capture_scratch: vec![0; capture_period_samples],
            started: false,
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

    fn enter_realtime(&self) -> Result<Option<RealtimeGuard>, EngineError> {
        if !self.config.realtime {
            return Ok(None);
        }

        let previous_policy = unsafe { libc::sched_getscheduler(0) };
        if previous_policy < 0 {
            return Err(EngineError::RealtimeScheduling {
                operation: "read current policy",
                source: io::Error::last_os_error(),
            });
        }
        let mut previous_parameters = libc::sched_param { sched_priority: 0 };
        if unsafe { libc::sched_getparam(0, &mut previous_parameters) } != 0 {
            return Err(EngineError::RealtimeScheduling {
                operation: "read current priority",
                source: io::Error::last_os_error(),
            });
        }

        let parameters = libc::sched_param {
            sched_priority: self.config.realtime_priority as i32,
        };
        if unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &parameters) } != 0 {
            return Err(EngineError::RealtimeScheduling {
                operation: "set SCHED_FIFO",
                source: io::Error::last_os_error(),
            });
        }

        Ok(Some(RealtimeGuard {
            policy: previous_policy,
            priority: previous_parameters.sched_priority,
        }))
    }

    pub fn start(&mut self) -> Result<(), EngineError> {
        self.start_with_capture_lead(Duration::ZERO)
    }

    fn prepare_worker_start(&mut self) -> Result<bool, EngineError> {
        if self.started {
            return Ok(false);
        }
        if self.capture_pcm.is_none() {
            return Err(EngineError::InvalidConfig(
                "capture worker is active".into(),
            ));
        }

        if let Err(error) = self.prime_playback() {
            self.cleanup_startup_failure();
            return Err(error);
        }
        if self.duplex_link {
            let link_result = {
                let playback_pcm = self.playback_pcm.as_ref().ok_or_else(|| {
                    EngineError::InvalidConfig("playback worker is active".into())
                })?;
                let capture_pcm = self
                    .capture_pcm
                    .as_ref()
                    .ok_or_else(|| EngineError::InvalidConfig("capture worker is active".into()))?;
                alsa_call(
                    playback_pcm.link(capture_pcm),
                    "link duplex streams",
                    StreamDirection::Playback,
                )
            };
            if let Err(error) = link_result {
                self.cleanup_startup_failure();
                return Err(error);
            }
        }
        self.started = true;
        Ok(true)
    }

    fn prime_playback(&self) -> Result<(), EngineError> {
        let playback_pcm = self
            .playback_pcm
            .as_ref()
            .ok_or_else(|| EngineError::InvalidConfig("playback worker is active".into()))?;
        let written = write_playback_samples(
            playback_pcm,
            &self.playback_silence,
            self.playback_channels,
            self.playback_start_samples,
        )?;
        if written != self.playback_start_frames {
            return Err(EngineError::ShortCommit {
                direction: StreamDirection::Playback,
                actual: written,
                required: self.playback_start_frames,
            });
        }
        Ok(())
    }

    fn cleanup_startup_failure(&mut self) {
        if let Some(playback_pcm) = self.playback_pcm.as_ref() {
            let _ = playback_pcm.unlink();
        }
        if let Some(capture_pcm) = self.capture_pcm.as_ref() {
            let _ = capture_pcm.drop();
            let _ = capture_pcm.prepare();
        }
        if let Some(playback_pcm) = self.playback_pcm.as_ref() {
            let _ = playback_pcm.drop();
            let _ = playback_pcm.prepare();
        }
        self.started = false;
    }

    fn start_with_capture_lead(&mut self, capture_lead: Duration) -> Result<(), EngineError> {
        if self.started {
            return Ok(());
        }

        if let Err(error) = self.prime_playback() {
            self.cleanup_startup_failure();
            return Err(error);
        }

        let start_result = (|| {
            let playback_pcm = self
                .playback_pcm
                .as_ref()
                .ok_or_else(|| EngineError::InvalidConfig("playback worker is active".into()))?;
            let capture_pcm = self
                .capture_pcm
                .as_ref()
                .ok_or_else(|| EngineError::InvalidConfig("capture worker is active".into()))?;
            if self.duplex_link {
                alsa_call(
                    playback_pcm.link(capture_pcm),
                    "link duplex streams",
                    StreamDirection::Playback,
                )?;
                let start_result = alsa_call(
                    playback_pcm.start(),
                    "start linked duplex streams",
                    StreamDirection::Playback,
                );
                let unlink_result = alsa_call(
                    playback_pcm.unlink(),
                    "unlink running duplex streams",
                    StreamDirection::Playback,
                );
                return start_result.and(unlink_result);
            }

            if capture_pcm.state() != State::Running {
                alsa_call(
                    capture_pcm.start(),
                    "start capture stream",
                    StreamDirection::Capture,
                )?;
            }
            if !capture_lead.is_zero() {
                std::thread::sleep(capture_lead);
            }
            if playback_pcm.state() != State::Running {
                alsa_call(
                    playback_pcm.start(),
                    "start playback stream",
                    StreamDirection::Playback,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = start_result {
            self.cleanup_startup_failure();
            return Err(error);
        }
        self.started = true;
        Ok(())
    }

    pub fn run(&mut self, stop: &AtomicBool, max_periods: Option<u64>) -> Result<(), EngineError> {
        let _realtime = self.enter_realtime()?;
        let deferred_worker_start = self.prepare_worker_start()?;

        let done = AtomicBool::new(false);
        let playback_pcm = self
            .playback_pcm
            .take()
            .ok_or_else(|| EngineError::InvalidConfig("playback worker is active".into()))?;
        let capture_pcm = match self.capture_pcm.take() {
            Some(pcm) => pcm,
            None => {
                self.playback_pcm = Some(playback_pcm);
                return Err(EngineError::InvalidConfig(
                    "capture worker is active".into(),
                ));
            }
        };
        let playback_silence = &self.playback_silence;
        let capture_scratch = &mut self.capture_scratch;
        let timeline = &self.timeline;
        let capture_channels = self.capture_channels;
        let capture_period_samples = self.capture_period_samples;
        let playback_channels = self.playback_channels;
        let playback_period_samples = self.playback_period_samples;
        let period = self.period;
        let buffer = self.buffer;
        let capture_config = CaptureWorkerConfig {
            channels: capture_channels,
            period_samples: capture_period_samples,
            period,
            initial_pro_sequence: 0,
            realtime_priority: self
                .config
                .realtime
                .then_some((self.config.realtime_priority as i32 - 1).max(1)),
        };
        let playback_config = PlaybackWorkerConfig {
            silence: playback_silence,
            channels: playback_channels,
            period_samples: playback_period_samples,
            start_samples: self.playback_start_samples,
            start_frames: self.playback_start_frames,
            period,
            buffer,
            rate: self.config.rate,
            timer_scheduling: self.playback_timer_scheduling,
            sequence_lead: 0,
            guard_frames: period,
            realtime_priority: self
                .config
                .realtime
                .then_some(self.config.realtime_priority as i32),
        };
        let start_gate = DuplexStartGate::new(self.duplex_link);
        let capture_control = WorkerControl {
            timeline,
            stop,
            done: &done,
            start_gate: deferred_worker_start.then_some(&start_gate),
        };
        let playback_control = capture_control;
        std::thread::scope(|scope| -> Result<_, EngineError> {
            let capture_handle = std::thread::Builder::new()
                .spawn_scoped(scope, move || {
                    signal_worker_panic(capture_control, || {
                        capture_worker(
                            capture_pcm,
                            capture_scratch,
                            capture_config,
                            capture_control,
                            None,
                            None,
                        )
                    })
                })
                .map_err(|source| EngineError::WorkerSpawn {
                    worker: "capture",
                    source,
                })?;
            let playback_handle = match std::thread::Builder::new().spawn_scoped(scope, move || {
                signal_worker_panic(playback_control, || {
                    playback_worker(
                        playback_pcm,
                        playback_config,
                        playback_control,
                        None,
                        None,
                        None,
                        max_periods,
                    )
                })
            }) {
                Ok(handle) => handle,
                Err(source) => {
                    done.store(true, Ordering::Release);
                    return Err(EngineError::WorkerSpawn {
                        worker: "playback",
                        source,
                    });
                }
            };
            let playback_result = playback_handle
                .join()
                .map_err(|_| EngineError::WorkerPanic)?;
            let capture_result = capture_handle
                .join()
                .map_err(|_| EngineError::WorkerPanic)?;
            Ok((playback_result, capture_result))
        })
        .map(
            |((playback_pcm, playback_result), (capture_pcm, capture_result))| {
                self.playback_pcm = Some(playback_pcm);
                self.capture_pcm = Some(capture_pcm);
                let result = playback_result.and(capture_result);
                if result.is_err() {
                    self.cleanup_startup_failure();
                }
                result
            },
        )
        .inspect_err(|_| {
            self.started = false;
        })?
    }

    pub fn run_pro<C: ProCaptureSink, P: ProPlaybackSource>(
        &mut self,
        stop: &AtomicBool,
        max_periods: Option<u64>,
        mut capture_sink: C,
        mut playback_source: P,
    ) -> Result<(), EngineError> {
        let mut playback_scratch = vec![0; self.playback_period_samples];
        let sequence_lead = u64::from(self.config.pro_latency_periods);
        let pro_clock = ProClock::new(sequence_lead);
        let _realtime = self.enter_realtime()?;
        let deferred_worker_start = self.prepare_worker_start()?;

        let done = AtomicBool::new(false);
        let playback_pcm = self
            .playback_pcm
            .take()
            .ok_or_else(|| EngineError::InvalidConfig("playback worker is active".into()))?;
        let capture_pcm = match self.capture_pcm.take() {
            Some(pcm) => pcm,
            None => {
                self.playback_pcm = Some(playback_pcm);
                return Err(EngineError::InvalidConfig(
                    "capture worker is active".into(),
                ));
            }
        };
        let playback_silence = &self.playback_silence;
        let capture_scratch = &mut self.capture_scratch;
        let timeline = &self.timeline;
        let capture_config = CaptureWorkerConfig {
            channels: self.capture_channels,
            period_samples: self.capture_period_samples,
            period: self.period,
            initial_pro_sequence: sequence_lead,
            realtime_priority: self
                .config
                .realtime
                .then_some((self.config.realtime_priority as i32 - 1).max(1)),
        };
        let playback_config = PlaybackWorkerConfig {
            silence: playback_silence,
            channels: self.playback_channels,
            period_samples: self.playback_period_samples,
            start_samples: self.playback_start_samples,
            start_frames: self.playback_start_frames,
            period: self.period,
            buffer: self.buffer,
            rate: self.config.rate,
            timer_scheduling: self.playback_timer_scheduling,
            sequence_lead,
            guard_frames: self.period,
            realtime_priority: self
                .config
                .realtime
                .then_some(self.config.realtime_priority as i32),
        };
        let start_gate = DuplexStartGate::new(self.duplex_link);
        let capture_control = WorkerControl {
            timeline,
            stop,
            done: &done,
            start_gate: deferred_worker_start.then_some(&start_gate),
        };
        let playback_control = capture_control;
        std::thread::scope(|scope| -> Result<_, EngineError> {
            let capture_clock = &pro_clock;
            let playback_clock = &pro_clock;
            let capture_handle = std::thread::Builder::new()
                .spawn_scoped(scope, move || {
                    signal_worker_panic(capture_control, || {
                        capture_worker(
                            capture_pcm,
                            capture_scratch,
                            capture_config,
                            capture_control,
                            Some(capture_clock),
                            Some(&mut capture_sink),
                        )
                    })
                })
                .map_err(|source| EngineError::WorkerSpawn {
                    worker: "capture",
                    source,
                })?;
            let playback_handle = match std::thread::Builder::new().spawn_scoped(scope, move || {
                signal_worker_panic(playback_control, || {
                    playback_worker(
                        playback_pcm,
                        playback_config,
                        playback_control,
                        Some(playback_clock),
                        Some(&mut playback_scratch),
                        Some(&mut playback_source),
                        max_periods,
                    )
                })
            }) {
                Ok(handle) => handle,
                Err(source) => {
                    done.store(true, Ordering::Release);
                    return Err(EngineError::WorkerSpawn {
                        worker: "playback",
                        source,
                    });
                }
            };
            let playback_result = playback_handle
                .join()
                .map_err(|_| EngineError::WorkerPanic)?;
            let capture_result = capture_handle
                .join()
                .map_err(|_| EngineError::WorkerPanic)?;
            Ok((playback_result, capture_result))
        })
        .map(
            |((playback_pcm, playback_result), (capture_pcm, capture_result))| {
                self.playback_pcm = Some(playback_pcm);
                self.capture_pcm = Some(capture_pcm);
                let result = playback_result.and(capture_result);
                if result.is_err() {
                    self.cleanup_startup_failure();
                }
                result
            },
        )
        .inspect_err(|_| {
            self.started = false;
        })?
    }

    pub fn stop(&mut self) -> Result<(), EngineError> {
        if !self.started {
            return Ok(());
        }

        let capture_pcm = self
            .capture_pcm
            .as_ref()
            .ok_or_else(|| EngineError::InvalidConfig("capture worker is active".into()))?;
        let playback_pcm = self
            .playback_pcm
            .as_ref()
            .ok_or_else(|| EngineError::InvalidConfig("playback worker is active".into()))?;
        let capture_result = alsa_call(
            capture_pcm.drop(),
            "stop capture stream",
            StreamDirection::Capture,
        );
        let playback_result = alsa_call(
            playback_pcm.drop(),
            "stop playback stream",
            StreamDirection::Playback,
        );
        self.started = false;
        capture_result.and(playback_result)
    }
}

impl Drop for DuplexEngine {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct RealtimeGuard {
    policy: libc::c_int,
    priority: libc::c_int,
}

impl Drop for RealtimeGuard {
    fn drop(&mut self) {
        let parameters = libc::sched_param {
            sched_priority: self.priority,
        };
        unsafe {
            let _ = libc::sched_setscheduler(0, self.policy, &parameters);
        }
    }
}

fn set_current_realtime(priority: i32) -> Result<(), EngineError> {
    let parameters = libc::sched_param {
        sched_priority: priority,
    };
    if unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &parameters) } != 0 {
        Err(EngineError::RealtimeScheduling {
            operation: "set worker SCHED_FIFO",
            source: io::Error::last_os_error(),
        })
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct WorkerControl<'a> {
    timeline: &'a HardwareTimeline,
    stop: &'a AtomicBool,
    done: &'a AtomicBool,
    start_gate: Option<&'a DuplexStartGate>,
}

fn signal_worker_panic<T>(control: WorkerControl<'_>, worker: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(worker)) {
        Ok(result) => result,
        Err(payload) => {
            control.done.store(true, Ordering::Release);
            resume_unwind(payload);
        }
    }
}

struct DuplexStartGate {
    linked: bool,
    capture_ready: AtomicBool,
    playback_ready: AtomicBool,
    capture_started: AtomicBool,
    started: AtomicBool,
}

struct ProClock {
    sequence: std::sync::atomic::AtomicU64,
    epoch: AtomicU32,
}

impl ProClock {
    fn new(sequence: u64) -> Self {
        Self {
            sequence: std::sync::atomic::AtomicU64::new(sequence),
            epoch: AtomicU32::new(0),
        }
    }

    fn load(&self) -> u64 {
        self.sequence.load(Ordering::Acquire)
    }

    fn publish(&self, sequence: u64) {
        self.sequence.store(sequence, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::Release);
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                self.epoch.as_ptr(),
                libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                i32::MAX,
            );
        }
    }

    fn wait_for_publish(&self, epoch: u32) -> io::Result<()> {
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000,
        };
        let result = unsafe {
            libc::syscall(
                libc::SYS_futex,
                self.epoch.as_ptr(),
                libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                epoch,
                &timeout,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EAGAIN | libc::EINTR | libc::ETIMEDOUT) => Ok(()),
            _ => Err(error),
        }
    }
}

impl DuplexStartGate {
    fn new(linked: bool) -> Self {
        Self {
            linked,
            capture_ready: AtomicBool::new(false),
            playback_ready: AtomicBool::new(false),
            capture_started: AtomicBool::new(false),
            started: AtomicBool::new(false),
        }
    }
}

#[derive(Clone, Copy)]
struct PlaybackWorkerConfig<'a> {
    silence: &'a [i32],
    channels: usize,
    period_samples: usize,
    start_samples: usize,
    start_frames: alsa::pcm::Frames,
    period: alsa::pcm::Frames,
    buffer: alsa::pcm::Frames,
    rate: u32,
    timer_scheduling: bool,
    sequence_lead: u64,
    guard_frames: alsa::pcm::Frames,
    realtime_priority: Option<i32>,
}

#[derive(Clone, Copy)]
struct CaptureWorkerConfig {
    channels: usize,
    period_samples: usize,
    period: alsa::pcm::Frames,
    initial_pro_sequence: u64,
    realtime_priority: Option<i32>,
}

fn playback_worker(
    pcm: PCM,
    config: PlaybackWorkerConfig<'_>,
    control: WorkerControl<'_>,
    pro_clock: Option<&ProClock>,
    output_scratch: Option<&mut [i32]>,
    playback_source: Option<&mut dyn ProPlaybackSource>,
    max_periods: Option<u64>,
) -> (PCM, Result<(), EngineError>) {
    let startup_priority =
        playback_startup_priority(config.realtime_priority, control.start_gate.is_some());
    if let Some(priority) = startup_priority
        && let Err(error) = set_current_realtime(priority)
    {
        control.done.store(true, Ordering::Release);
        return (pcm, Err(error));
    }
    if let Some(gate) = control.start_gate {
        gate.playback_ready.store(true, Ordering::Release);
        while !gate.capture_ready.load(Ordering::Acquire) {
            if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
                control.done.store(true, Ordering::Release);
                return (pcm, Ok(()));
            }
            std::thread::yield_now();
        }
        while !gate.linked && !gate.capture_started.load(Ordering::Acquire) {
            if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
                control.done.store(true, Ordering::Release);
                return (pcm, Ok(()));
            }
            std::thread::yield_now();
        }
        if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
            control.done.store(true, Ordering::Release);
            return (pcm, Ok(()));
        }
        if config.realtime_priority != startup_priority
            && let Some(priority) = config.realtime_priority
            && let Err(error) = set_current_realtime(priority)
        {
            control.done.store(true, Ordering::Release);
            return (pcm, Err(error));
        }
        let start_result = alsa_call(
            pcm.start(),
            if gate.linked {
                "start linked duplex streams"
            } else {
                "start playback stream"
            },
            StreamDirection::Playback,
        );
        let unlink_result = if gate.linked {
            alsa_call(
                pcm.unlink(),
                "unlink running duplex streams",
                StreamDirection::Playback,
            )
        } else {
            Ok(())
        };
        if let Err(error) = start_result.and(unlink_result) {
            control.done.store(true, Ordering::Release);
            return (pcm, Err(error));
        }
        gate.started.store(true, Ordering::Release);
    }
    let result = playback_worker_loop(
        &pcm,
        config,
        control,
        pro_clock,
        output_scratch,
        playback_source,
        max_periods,
    );
    (pcm, result)
}

fn playback_startup_priority(priority: Option<i32>, deferred_start: bool) -> Option<i32> {
    if deferred_start {
        priority.map(|priority| (priority - 1).max(1))
    } else {
        priority
    }
}

fn playback_worker_loop(
    pcm: &PCM,
    config: PlaybackWorkerConfig<'_>,
    control: WorkerControl<'_>,
    pro_clock: Option<&ProClock>,
    output_scratch: Option<&mut [i32]>,
    mut playback_source: Option<&mut dyn ProPlaybackSource>,
    max_periods: Option<u64>,
) -> Result<(), EngineError> {
    let mut position = 0_u64;
    let mut sequence = 0_u64;
    let mut output_scratch = output_scratch;
    let mut prepared_sequence = None;

    loop {
        if control.stop.load(Ordering::Relaxed)
            || control.done.load(Ordering::Acquire)
            || max_periods
                .is_some_and(|limit| control.timeline.snapshot().periods_processed >= limit)
        {
            control.done.store(true, Ordering::Release);
            return Ok(());
        }

        if prepared_sequence != Some(sequence) {
            let wait_budget = if config.timer_scheduling {
                match playback_preparation_budget(
                    pcm,
                    config.buffer,
                    config.guard_frames,
                    config.period,
                    config.rate,
                ) {
                    Ok(budget) => budget,
                    Err(error) if error.is_xrun() => {
                        if let Err(error) = recover_stream(
                            pcm,
                            StreamDirection::Playback,
                            &error,
                            control.timeline,
                            Some((
                                config.silence,
                                config.channels,
                                config.start_samples,
                                config.start_frames,
                            )),
                        ) {
                            control.done.store(true, Ordering::Release);
                            return Err(error);
                        }
                        continue;
                    }
                    Err(error) => {
                        control.done.store(true, Ordering::Release);
                        return Err(error);
                    }
                }
            } else {
                Duration::ZERO
            };

            if let (Some(source), Some(scratch)) = (
                playback_source.as_deref_mut(),
                output_scratch.as_deref_mut(),
            ) {
                scratch[..config.period_samples].fill(0);
                if catch_unwind(AssertUnwindSafe(|| {
                    source.process_playback(
                        sequence,
                        &mut scratch[..config.period_samples],
                        wait_budget,
                    );
                }))
                .is_err()
                {
                    control.done.store(true, Ordering::Release);
                    return Err(EngineError::WorkerPanic);
                }
            }
            prepared_sequence = Some(sequence);
        }

        let samples = output_scratch
            .as_deref()
            .map_or(config.silence, |scratch| &scratch[..config.period_samples]);

        let wait_result = if config.timer_scheduling {
            wait_for_playback_target(
                pcm,
                config.buffer,
                config.guard_frames,
                config.rate,
                control,
            )
        } else {
            wait_for_transfer(pcm, StreamDirection::Playback)
        };
        if let Err(error) = wait_result {
            if error.is_xrun() {
                if let Err(error) = recover_stream(
                    pcm,
                    StreamDirection::Playback,
                    &error,
                    control.timeline,
                    Some((
                        config.silence,
                        config.channels,
                        config.start_samples,
                        config.start_frames,
                    )),
                ) {
                    control.done.store(true, Ordering::Release);
                    return Err(error);
                }
                continue;
            }
            control.done.store(true, Ordering::Release);
            return Err(error);
        }
        if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
            continue;
        }
        if let Ok(delay) = pcm.delay() {
            control.timeline.update_pcm_delay(
                StreamDirection::Playback,
                delay,
                config.period as u64,
            );
        }

        match write_playback_samples(pcm, samples, config.channels, config.period_samples) {
            Ok(written) if written == config.period => {
                position = position.wrapping_add(config.period as u64);
                sequence = sequence.wrapping_add(1);
                prepared_sequence = None;
                if let Some(clock) = pro_clock {
                    clock.publish(pro_target_sequence(sequence, config.sequence_lead));
                }
                control.timeline.update_playback_position(position);
                control.timeline.processed_frames(config.period as u64, 1);
            }
            Ok(written) => {
                let error = EngineError::ShortCommit {
                    direction: StreamDirection::Playback,
                    actual: written,
                    required: config.period,
                };
                control.done.store(true, Ordering::Release);
                return Err(error);
            }
            Err(error) if error.is_xrun() => {
                if let Err(error) = recover_stream(
                    pcm,
                    StreamDirection::Playback,
                    &error,
                    control.timeline,
                    Some((
                        config.silence,
                        config.channels,
                        config.start_samples,
                        config.start_frames,
                    )),
                ) {
                    control.done.store(true, Ordering::Release);
                    return Err(error);
                }
            }
            Err(error) => {
                control.done.store(true, Ordering::Release);
                return Err(error);
            }
        }
    }
}

fn capture_worker(
    pcm: PCM,
    scratch: &mut [i32],
    config: CaptureWorkerConfig,
    control: WorkerControl<'_>,
    pro_clock: Option<&ProClock>,
    capture_sink: Option<&mut dyn ProCaptureSink>,
) -> (PCM, Result<(), EngineError>) {
    if let Some(priority) = config.realtime_priority
        && let Err(error) = set_current_realtime(priority)
    {
        control.done.store(true, Ordering::Release);
        return (pcm, Err(error));
    }
    if let Some(gate) = control.start_gate {
        gate.capture_ready.store(true, Ordering::Release);
        while !gate.playback_ready.load(Ordering::Acquire) {
            if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
                control.done.store(true, Ordering::Release);
                return (pcm, Ok(()));
            }
            std::thread::yield_now();
        }
        if !gate.linked {
            if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
                control.done.store(true, Ordering::Release);
                return (pcm, Ok(()));
            }
            if let Err(error) = alsa_call(
                pcm.start(),
                "start capture stream",
                StreamDirection::Capture,
            ) {
                control.done.store(true, Ordering::Release);
                return (pcm, Err(error));
            }
            gate.capture_started.store(true, Ordering::Release);
        }
        while !gate.started.load(Ordering::Acquire) {
            if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
                control.done.store(true, Ordering::Release);
                return (pcm, Ok(()));
            }
            std::thread::yield_now();
        }
    }
    let result = capture_worker_loop(&pcm, scratch, config, control, pro_clock, capture_sink);
    (pcm, result)
}

fn capture_worker_loop(
    pcm: &PCM,
    scratch: &mut [i32],
    config: CaptureWorkerConfig,
    control: WorkerControl<'_>,
    pro_clock: Option<&ProClock>,
    mut capture_sink: Option<&mut dyn ProCaptureSink>,
) -> Result<(), EngineError> {
    let mut position = 0_u64;
    let mut next_pro_sequence = config.initial_pro_sequence;
    let mut pro_generation = control.timeline.generation();
    loop {
        if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
            control.done.store(true, Ordering::Release);
            return Ok(());
        }

        match read_capture_samples(pcm, scratch, config.channels, config.period_samples) {
            Ok(read) if read == config.period => {
                if let Ok(delay) = pcm.delay() {
                    control.timeline.update_pcm_delay(
                        StreamDirection::Capture,
                        delay,
                        config.period as u64,
                    );
                }
                if let (Some(clock), Some(sink)) = (pro_clock, capture_sink.as_deref_mut()) {
                    let (current_generation, rebase_target) = match wait_for_pro_capture_target(
                        next_pro_sequence,
                        control.timeline,
                        clock,
                        control,
                    ) {
                        Ok(Some(target)) => target,
                        Ok(None) => {
                            control.done.store(true, Ordering::Release);
                            return Ok(());
                        }
                        Err(error) => {
                            control.done.store(true, Ordering::Release);
                            return Err(error);
                        }
                    };
                    let sequence = take_pro_capture_sequence(
                        &mut next_pro_sequence,
                        &mut pro_generation,
                        current_generation,
                        rebase_target,
                    );
                    if catch_unwind(AssertUnwindSafe(|| {
                        sink.process_capture(sequence, scratch);
                    }))
                    .is_err()
                    {
                        control.done.store(true, Ordering::Release);
                        return Err(EngineError::WorkerPanic);
                    }
                }
                position = position.wrapping_add(config.period as u64);
                control.timeline.update_capture_position(position);
            }
            Ok(read) => {
                let error = EngineError::ShortCommit {
                    direction: StreamDirection::Capture,
                    actual: read,
                    required: config.period,
                };
                control.done.store(true, Ordering::Release);
                return Err(error);
            }
            Err(error) if error.is_xrun() => {
                if let Err(error) = recover_stream(
                    pcm,
                    StreamDirection::Capture,
                    &error,
                    control.timeline,
                    None,
                ) {
                    control.done.store(true, Ordering::Release);
                    return Err(error);
                }
            }
            Err(error) => {
                control.done.store(true, Ordering::Release);
                return Err(error);
            }
        }
    }
}

fn pro_target_sequence(playback_sequence: u64, lead: u64) -> u64 {
    playback_sequence.wrapping_add(lead)
}

fn take_pro_capture_sequence(
    next: &mut u64,
    generation: &mut u64,
    current_generation: u64,
    rebase_target: u64,
) -> u64 {
    if *generation != current_generation {
        *generation = current_generation;
    }
    align_sequence_forward(next, rebase_target);
    let sequence = *next;
    *next = next.wrapping_add(1);
    sequence
}

fn align_sequence_forward(next: &mut u64, target: u64) {
    if sequence_before(*next, target) {
        *next = target;
    }
}

fn stable_rebase_target(timeline: &HardwareTimeline, clock: &ProClock) -> (u64, u64) {
    loop {
        let generation = timeline.generation();
        let target = clock.load();
        if generation == timeline.generation() {
            return (generation, target);
        }
    }
}

fn wait_for_pro_capture_target(
    next: u64,
    timeline: &HardwareTimeline,
    clock: &ProClock,
    control: WorkerControl<'_>,
) -> Result<Option<(u64, u64)>, EngineError> {
    loop {
        let epoch = clock.epoch.load(Ordering::Acquire);
        let target = stable_rebase_target(timeline, clock);
        if pro_capture_target_ready(next, target.1) {
            return Ok(Some(target));
        }
        if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
            return Ok(None);
        }
        clock
            .wait_for_publish(epoch)
            .map_err(EngineError::ClockWait)?;
    }
}

fn pro_capture_target_ready(next: u64, target: u64) -> bool {
    next == target || sequence_before(next, target)
}

fn sequence_before(sequence: u64, target: u64) -> bool {
    sequence != target && target.wrapping_sub(sequence) < (1_u64 << 63)
}

fn read_capture_samples(
    pcm: &PCM,
    scratch: &mut [i32],
    channels: usize,
    sample_count: usize,
) -> Result<alsa::pcm::Frames, EngineError> {
    let io = unsafe { pcm.io_unchecked::<i32>() };
    let mut remaining_frames = sample_count / channels;
    let required = remaining_frames as i64;
    let mut offset_samples = 0;

    while remaining_frames > 0 {
        let end_samples = offset_samples + remaining_frames * channels;
        let processed = alsa_call(
            io.readi(&mut scratch[offset_samples..end_samples]),
            "read capture period",
            StreamDirection::Capture,
        )?;
        if processed == 0 {
            return Err(EngineError::ShortCommit {
                direction: StreamDirection::Capture,
                actual: 0,
                required,
            });
        }
        offset_samples += processed * channels;
        remaining_frames -= processed;
        if remaining_frames > 0 {
            wait_for_transfer(pcm, StreamDirection::Capture)?;
        }
    }
    Ok(required)
}

fn write_playback_samples(
    pcm: &PCM,
    samples: &[i32],
    channels: usize,
    sample_count: usize,
) -> Result<alsa::pcm::Frames, EngineError> {
    let io = unsafe { pcm.io_unchecked::<i32>() };
    let mut remaining_frames = sample_count / channels;
    let required = remaining_frames as i64;
    let mut offset_samples = 0;

    while remaining_frames > 0 {
        let end_samples = offset_samples + remaining_frames * channels;
        let processed = alsa_call(
            io.writei(&samples[offset_samples..end_samples]),
            "write playback period",
            StreamDirection::Playback,
        )?;
        if processed == 0 {
            return Err(EngineError::ShortCommit {
                direction: StreamDirection::Playback,
                actual: 0,
                required,
            });
        }
        offset_samples += processed * channels;
        remaining_frames -= processed;
        if remaining_frames > 0 {
            wait_for_transfer(pcm, StreamDirection::Playback)?;
        }
    }
    Ok(required)
}

fn wait_for_transfer(pcm: &PCM, direction: StreamDirection) -> Result<(), EngineError> {
    loop {
        if alsa_call(pcm.wait(Some(1)), "wait for transfer", direction)? {
            return Ok(());
        }
    }
}

fn wait_for_playback_target(
    pcm: &PCM,
    buffer: alsa::pcm::Frames,
    guard_frames: alsa::pcm::Frames,
    rate: u32,
    control: WorkerControl<'_>,
) -> Result<(), EngineError> {
    let target_avail = buffer - guard_frames;
    loop {
        if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
            return Ok(());
        }
        let available = alsa_call(
            pcm.avail_update(),
            "update playback availability",
            StreamDirection::Playback,
        )?;
        if available >= target_avail {
            return Ok(());
        }
        let remaining = target_avail - available;
        let sleep = playback_target_sleep(remaining, rate);
        if sleep.is_zero() {
            hint::spin_loop();
        } else {
            std::thread::sleep(sleep);
        }
    }
}

fn playback_target_sleep(remaining: alsa::pcm::Frames, rate: u32) -> Duration {
    const PREWAKE_NANOS: u64 = 50_000;

    let frames = u64::try_from(remaining).unwrap_or(0);
    let nanos = frames.saturating_mul(1_000_000_000) / u64::from(rate);
    Duration::from_nanos(nanos.saturating_sub(PREWAKE_NANOS))
}

fn playback_preparation_budget(
    pcm: &PCM,
    buffer: alsa::pcm::Frames,
    guard_frames: alsa::pcm::Frames,
    period: alsa::pcm::Frames,
    rate: u32,
) -> Result<Duration, EngineError> {
    let target_avail = buffer - guard_frames;
    let available = alsa_call(
        pcm.avail_update(),
        "update playback preparation availability",
        StreamDirection::Playback,
    )?;
    let budget_frames = preparation_budget_frames(available, target_avail, period);
    Ok(Duration::from_nanos(
        budget_frames.saturating_mul(1_000_000_000) / u64::from(rate),
    ))
}

fn preparation_budget_frames(
    available: alsa::pcm::Frames,
    target_avail: alsa::pcm::Frames,
    period: alsa::pcm::Frames,
) -> u64 {
    let remaining = target_avail.saturating_sub(available);
    let reserve = (period / 4).max(1);
    u64::try_from(remaining.saturating_sub(reserve)).unwrap_or(0)
}

fn recover_stream(
    pcm: &PCM,
    direction: StreamDirection,
    error: &EngineError,
    timeline: &HardwareTimeline,
    playback: Option<(&[i32], usize, usize, alsa::pcm::Frames)>,
) -> Result<(), EngineError> {
    let errno = error.errno().unwrap_or(libc::EPIPE);
    timeline.record_hardware_xrun(direction);
    alsa_call(pcm.recover(errno, true), "recover stream XRUN", direction)?;

    if pcm.state() != State::Running {
        if let Some((silence, channels, buffer_samples, buffer)) = playback {
            let written = write_playback_samples(pcm, silence, channels, buffer_samples)?;
            if written != buffer {
                return Err(EngineError::ShortCommit {
                    direction: StreamDirection::Playback,
                    actual: written,
                    required: buffer,
                });
            }
        }
        alsa_call(pcm.start(), "restart stream after XRUN", direction)?;
    }
    timeline.reset_after_hardware_xrun();
    Ok(())
}

fn open_pcm(
    device: &str,
    direction: Direction,
    stream_direction: StreamDirection,
) -> Result<PCM, EngineError> {
    PCM::new(device, direction, false).map_err(|source| EngineError::Alsa {
        operation: "open PCM",
        direction: stream_direction,
        source,
    })
}

fn configure_pcm(
    pcm: &PCM,
    config: &HardwareConfig,
    stream: &crate::PcmConfig,
    direction: StreamDirection,
) -> Result<(), EngineError> {
    let params = HwParams::any(pcm).map_err(|source| EngineError::Alsa {
        operation: "create hardware parameters",
        direction,
        source,
    })?;
    params
        .set_access(Access::RWInterleaved)
        .map_err(|source| EngineError::Alsa {
            operation: "set interleaved access",
            direction,
            source,
        })?;
    params
        .set_format(Format::S32LE)
        .map_err(|source| EngineError::Alsa {
            operation: "set sample format",
            direction,
            source,
        })?;
    params
        .set_channels(stream.channels)
        .map_err(|source| EngineError::Alsa {
            operation: "set channel count",
            direction,
            source,
        })?;
    params
        .set_rate(config.rate, ValueOr::Nearest)
        .map_err(|source| EngineError::Alsa {
            operation: "set sample rate",
            direction,
            source,
        })?;
    params
        .set_period_size(i64::from(config.period_size), ValueOr::Nearest)
        .map_err(|source| EngineError::Alsa {
            operation: "set period size",
            direction,
            source,
        })?;
    params
        .set_buffer_size(i64::from(config.buffer_size))
        .map_err(|source| EngineError::Alsa {
            operation: "set buffer size",
            direction,
            source,
        })?;
    pcm.hw_params(&params).map_err(|source| EngineError::Alsa {
        operation: "apply hardware parameters",
        direction,
        source,
    })?;

    let current = pcm
        .hw_params_current()
        .map_err(|source| EngineError::Alsa {
            operation: "read negotiated hardware parameters",
            direction,
            source,
        })?;
    let actual_rate = current.get_rate().map_err(|source| EngineError::Alsa {
        operation: "read negotiated sample rate",
        direction,
        source,
    })?;
    let actual_period = current
        .get_period_size()
        .map_err(|source| EngineError::Alsa {
            operation: "read negotiated period size",
            direction,
            source,
        })?;
    let actual_buffer = current
        .get_buffer_size()
        .map_err(|source| EngineError::Alsa {
            operation: "read negotiated buffer size",
            direction,
            source,
        })?;
    let actual_channels = current.get_channels().map_err(|source| EngineError::Alsa {
        operation: "read negotiated channel count",
        direction,
        source,
    })?;
    let actual_format = current.get_format().map_err(|source| EngineError::Alsa {
        operation: "read negotiated sample format",
        direction,
        source,
    })?;
    let actual_access = current.get_access().map_err(|source| EngineError::Alsa {
        operation: "read negotiated access mode",
        direction,
        source,
    })?;

    if actual_rate != config.rate
        || actual_period != i64::from(config.period_size)
        || actual_buffer != i64::from(config.buffer_size)
        || actual_channels != stream.channels
        || actual_format != Format::S32LE
        || actual_access != Access::RWInterleaved
    {
        return Err(EngineError::InvalidConfig(format!(
            "{direction:?} negotiated rate={actual_rate}, period={actual_period}, buffer={actual_buffer}, channels={actual_channels}, format={actual_format}, access={actual_access:?}"
        )));
    }
    Ok(())
}

fn configure_sw_params(
    pcm: &PCM,
    start_threshold: alsa::pcm::Frames,
    avail_min: alsa::pcm::Frames,
    direction: StreamDirection,
) -> Result<(), EngineError> {
    let params = pcm
        .sw_params_current()
        .map_err(|source| EngineError::Alsa {
            operation: "read software parameters",
            direction,
            source,
        })?;
    params
        .set_avail_min(avail_min)
        .map_err(|source| EngineError::Alsa {
            operation: "set software availability threshold",
            direction,
            source,
        })?;
    params
        .set_start_threshold(start_threshold)
        .map_err(|source| EngineError::Alsa {
            operation: "set software start threshold",
            direction,
            source,
        })?;
    params
        .set_tstamp_mode(true)
        .map_err(|source| EngineError::Alsa {
            operation: "enable ALSA timestamps",
            direction,
            source,
        })?;
    params
        .set_tstamp_type(alsa::pcm::TstampType::Monotonic)
        .map_err(|source| EngineError::Alsa {
            operation: "set ALSA timestamp type",
            direction,
            source,
        })?;
    pcm.sw_params(&params).map_err(|source| EngineError::Alsa {
        operation: "apply software parameters",
        direction,
        source,
    })
}

fn ensure_supported_format(
    format: &SampleFormat,
    direction: StreamDirection,
) -> Result<(), EngineError> {
    if *format == SampleFormat::S32Le {
        Ok(())
    } else {
        Err(EngineError::InvalidConfig(format!(
            "unsupported {direction:?} sample format"
        )))
    }
}

fn sample_count(frames: u32, channels: u32) -> Result<usize, EngineError> {
    usize::try_from(u64::from(frames) * u64::from(channels))
        .map_err(|_| EngineError::InvalidConfig("sample count does not fit usize".into()))
}

fn routing_error_to_engine_error(error: RoutingError) -> EngineError {
    EngineError::InvalidConfig(error.to_string())
}

fn alsa_call<T>(
    result: alsa::Result<T>,
    operation: &'static str,
    direction: StreamDirection,
) -> Result<T, EngineError> {
    result.map_err(|source| {
        if source.errno() == libc::EPIPE || source.errno() == libc::ESTRPIPE {
            EngineError::Xrun {
                operation,
                direction,
                source,
            }
        } else {
            EngineError::Alsa {
                operation,
                direction,
                source,
            }
        }
    })
}

impl EngineError {
    fn is_xrun(&self) -> bool {
        matches!(self, Self::Xrun { .. })
    }

    fn errno(&self) -> Option<i32> {
        match self {
            Self::Xrun { source, .. } => Some(source.errno()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        align_sequence_forward, playback_startup_priority, playback_target_sleep,
        preparation_budget_frames, pro_capture_target_ready, pro_target_sequence,
        take_pro_capture_sequence,
    };
    use std::time::Duration;

    #[test]
    fn playback_target_tracks_configured_lead() {
        assert_eq!(pro_target_sequence(4, 0), 4);
        assert_eq!(pro_target_sequence(4, 1), 5);
        assert_eq!(pro_target_sequence(9, 1), 10);
        assert_eq!(pro_target_sequence(u64::MAX, 1), 0);
    }

    #[test]
    fn capture_publishes_one_monotonic_target_per_period() {
        let mut next = 4;
        let mut generation = 0;
        assert_eq!(
            take_pro_capture_sequence(&mut next, &mut generation, 0, 4),
            4
        );
        assert_eq!(
            take_pro_capture_sequence(&mut next, &mut generation, 0, 5),
            5
        );
        assert_eq!(
            take_pro_capture_sequence(&mut next, &mut generation, 0, 6),
            6
        );
    }

    #[test]
    fn first_capture_aligns_to_playback_after_startup_queueing() {
        let mut next = 1;

        align_sequence_forward(&mut next, 3);

        assert_eq!(next, 3);
    }

    #[test]
    fn capture_advances_to_target_without_moving_backward() {
        let mut next = 50;
        let mut generation = 3;
        assert_eq!(
            take_pro_capture_sequence(&mut next, &mut generation, 3, 20),
            50
        );
        assert_eq!(
            take_pro_capture_sequence(&mut next, &mut generation, 4, 20),
            51
        );
        assert_eq!(
            take_pro_capture_sequence(&mut next, &mut generation, 4, 99),
            99
        );
        assert_eq!(
            take_pro_capture_sequence(&mut next, &mut generation, 4, 100),
            100
        );
    }

    #[test]
    fn capture_waits_for_a_unique_playback_target() {
        assert!(!pro_capture_target_ready(2, 1));
        assert!(pro_capture_target_ready(2, 2));
        assert!(pro_capture_target_ready(2, 3));
        assert!(pro_capture_target_ready(u64::MAX, 0));
    }

    #[test]
    fn preparation_budget_never_consumes_hardware_reserve() {
        assert_eq!(preparation_budget_frames(64, 128, 64), 48);
        assert_eq!(preparation_budget_frames(112, 128, 64), 0);
        assert_eq!(preparation_budget_frames(128, 128, 64), 0);
        assert_eq!(preparation_budget_frames(160, 128, 64), 0);
    }

    #[test]
    fn playback_target_sleep_leaves_bounded_prewake_margin() {
        assert_eq!(
            playback_target_sleep(64, 48_000),
            Duration::from_nanos(1_283_333)
        );
        assert_eq!(playback_target_sleep(2, 48_000), Duration::ZERO);
    }

    #[test]
    fn playback_uses_capture_priority_until_deferred_start_is_ready() {
        assert_eq!(playback_startup_priority(Some(88), true), Some(87));
        assert_eq!(playback_startup_priority(Some(1), true), Some(1));
        assert_eq!(playback_startup_priority(Some(88), false), Some(88));
        assert_eq!(playback_startup_priority(None, true), None);
    }
}
