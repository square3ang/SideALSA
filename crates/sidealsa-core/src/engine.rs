use std::{
    io,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alsa::{
    Direction, ValueOr,
    pcm::{Access, AudioTstampType, Format, HwParams, PCM, State, Status, StatusBuilder},
    poll::{self, Descriptors, pollfd},
};
use sidealsa_config::{StartupLoopbackConfig, direct_pro_write_reserve_frames};
use thiserror::Error;

use crate::latency::LatencyGuard;
use crate::pro::{ProCaptureSink, ProPlaybackSource};
use crate::{
    DIRECT_MIN_PLAYBACK_QUEUE_PERIODS, HardwareConfig, HardwareStats, HardwareTimeline,
    LINKED_PHASE_OVERHEAD_DIVISOR, Profile, RoutingError, RoutingTable, SampleFormat,
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
    #[error(
        "ALSA linked duplex XRUN during {operation} (playback={playback}, capture={capture}): {source}"
    )]
    LinkedXrun {
        operation: &'static str,
        playback: bool,
        capture: bool,
        #[source]
        source: alsa::Error,
    },
    #[error("ALSA stream suspended during {operation} on {direction:?}: {source}")]
    Suspended {
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
    #[error("audio operation stopped")]
    Stopped,
    #[error(
        "linked startup remained unstable after {attempts} attempts (minimum capture-to-playback write interval {minimum_write_nanos} ns)"
    )]
    LinkedStartupUnstable {
        attempts: u32,
        minimum_write_nanos: u64,
    },
    #[error("linked startup was interrupted before hardware readiness")]
    LinkedStartupInterrupted,
    #[error("startup digital loopback normalization failed: {0}")]
    StartupLoopback(&'static str),
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
    playback_start_frames: alsa::pcm::Frames,
    playback_timer_scheduling: bool,
    duplex_link: bool,
    playback_channels: usize,
    capture_channels: usize,
    playback_silence: Vec<i32>,
    capture_scratch: Vec<i32>,
    started: bool,
    streams_linked: bool,
}

impl DuplexEngine {
    pub fn open(profile: Profile) -> Result<Self, EngineError> {
        profile
            .validate()
            .map_err(|error| EngineError::InvalidConfig(error.to_string()))?;
        let duplex_link = profile.device.effective_duplex_link();
        let routing = RoutingTable::compile(&profile).map_err(routing_error_to_engine_error)?;
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
        )
        .min(buffer);
        let playback_avail_min =
            if config.uses_event_driven_linked_pro() || config.playback_timer_scheduling {
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
        let _playback_start_samples = usize::try_from(playback_start_frames)
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
            if config.uses_event_driven_linked_pro() {
                0
            } else if playback_timer_scheduling || duplex_link {
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
        configure_sw_params(
            &capture_pcm,
            if config.uses_event_driven_linked_pro() {
                0
            } else {
                buffer
            },
            // Direct duplex waits for a full client block, even with smaller USB periods.
            if config.uses_event_driven_linked_pro() {
                period
            } else {
                i64::from(config.effective_hardware_period_size())
            },
            StreamDirection::Capture,
        )?;

        Ok(Self {
            config,
            playback_pcm: Some(playback_pcm),
            capture_pcm: Some(capture_pcm),
            timeline: Arc::new(HardwareTimeline::default()),
            routing,
            period,
            buffer,
            playback_period_samples,
            playback_start_frames,
            playback_timer_scheduling,
            duplex_link,
            playback_channels,
            capture_channels,
            playback_silence: vec![0; playback_buffer_samples],
            capture_scratch: vec![0; capture_period_samples],
            started: false,
            streams_linked: false,
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
        RealtimeGuard::enter(self.config.realtime_priority as i32, "set SCHED_FIFO").map(Some)
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

        if let Err(error) = self.prepare_pcms().and_then(|()| self.prime_playback()) {
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
            self.playback_start_frames as usize,
            None,
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

    fn prepare_pcms(&self) -> Result<(), EngineError> {
        let capture_pcm = self
            .capture_pcm
            .as_ref()
            .ok_or_else(|| EngineError::InvalidConfig("capture worker is active".into()))?;
        let playback_pcm = self
            .playback_pcm
            .as_ref()
            .ok_or_else(|| EngineError::InvalidConfig("playback worker is active".into()))?;
        alsa_call(
            capture_pcm.prepare(),
            "prepare capture stream for startup",
            StreamDirection::Capture,
        )?;
        alsa_call(
            playback_pcm.prepare(),
            "prepare playback stream for startup",
            StreamDirection::Playback,
        )
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
        self.streams_linked = false;
    }

    fn linked_pro_sequence_lead(&self) -> Option<u64> {
        if !self.duplex_link {
            return None;
        }
        match self.config.pro_latency_periods {
            0 => Some(0),
            1 if self.buffer
                >= self.period + i64::from(self.config.effective_hardware_period_size()) =>
            {
                Some(1)
            }
            _ => None,
        }
    }

    fn prepare_linked_pro_start(
        &mut self,
        sequence_lead: u64,
    ) -> Result<alsa::pcm::Frames, EngineError> {
        if self.started {
            return Err(EngineError::InvalidConfig(
                "linked PRO cycle is already active".into(),
            ));
        }

        let hardware_period = i64::from(self.config.effective_hardware_period_size());
        let start_frames = if self.config.uses_event_driven_linked_pro() {
            direct_linked_start_frames(self.period, self.buffer, self.config.playback_queue_periods)
        } else if sequence_lead == 0 {
            let playback_floor = linked_zero_lead_playback_floor(
                i64::from(self.config.effective_linked_playback_guard_frames()),
                hardware_period,
                self.buffer,
                self.period,
            );
            linked_start_frames(
                self.period,
                playback_floor,
                self.buffer,
                self.config.rate,
                self.config.pro_handoff_nanos(),
            )
        } else {
            linked_ahead_start_frames(self.period, hardware_period, self.buffer)
        };
        let _start_samples = usize::try_from(start_frames)
            .ok()
            .and_then(|frames| frames.checked_mul(self.playback_channels))
            .ok_or_else(|| EngineError::InvalidConfig("playback start size overflow".into()))?;
        let playback_pcm = self
            .playback_pcm
            .as_ref()
            .ok_or_else(|| EngineError::InvalidConfig("playback worker is active".into()))?;
        let capture_pcm = self
            .capture_pcm
            .as_ref()
            .ok_or_else(|| EngineError::InvalidConfig("capture worker is active".into()))?;

        let start_result = (|| {
            if self.config.uses_event_driven_linked_pro() {
                alsa_call(
                    playback_pcm.link(capture_pcm),
                    "link direct duplex streams",
                    StreamDirection::Playback,
                )?;
                alsa_call(
                    playback_pcm.prepare(),
                    "prepare direct linked streams",
                    StreamDirection::Playback,
                )?;
            } else {
                self.prepare_pcms()?;
            }
            let written = write_playback_samples(
                playback_pcm,
                &self.playback_silence,
                self.playback_channels,
                start_frames as usize,
                None,
            )?;
            if written != start_frames {
                return Err(EngineError::ShortCommit {
                    direction: StreamDirection::Playback,
                    actual: written,
                    required: start_frames,
                });
            }
            if !self.config.uses_event_driven_linked_pro() {
                alsa_call(
                    playback_pcm.link(capture_pcm),
                    "link duplex streams",
                    StreamDirection::Playback,
                )?;
            }
            alsa_call(
                playback_pcm.start(),
                "start linked duplex streams",
                StreamDirection::Playback,
            )
        })();
        if let Err(error) = start_result {
            self.cleanup_startup_failure();
            return Err(error);
        }

        self.started = true;
        self.streams_linked = true;
        Ok(start_frames)
    }

    pub fn start(&mut self) -> Result<(), EngineError> {
        if self.started {
            return Ok(());
        }

        if let Err(error) = self.prepare_pcms().and_then(|()| self.prime_playback()) {
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
        self.run_split_workers::<NoopPro, NoopPro>(stop, max_periods, None, 0, None)
    }

    pub fn run_pro<C: ProCaptureSink, P: ProPlaybackSource>(
        &mut self,
        stop: &AtomicBool,
        max_periods: Option<u64>,
        capture_sink: C,
        playback_source: P,
    ) -> Result<(), EngineError> {
        self.run_pro_inner(stop, max_periods, capture_sink, playback_source, None)
    }

    pub fn run_pro_with_ready<C: ProCaptureSink, P: ProPlaybackSource>(
        &mut self,
        stop: &AtomicBool,
        max_periods: Option<u64>,
        capture_sink: C,
        playback_source: P,
        hardware_ready: &AtomicBool,
    ) -> Result<(), EngineError> {
        self.run_pro_inner(
            stop,
            max_periods,
            capture_sink,
            playback_source,
            Some(hardware_ready),
        )
    }

    fn run_pro_inner<C: ProCaptureSink, P: ProPlaybackSource>(
        &mut self,
        stop: &AtomicBool,
        max_periods: Option<u64>,
        mut capture_sink: C,
        mut playback_source: P,
        hardware_ready: Option<&AtomicBool>,
    ) -> Result<(), EngineError> {
        if let Some(sequence_lead) = self.linked_pro_sequence_lead() {
            return self.run_pro_linked(
                stop,
                max_periods,
                &mut capture_sink,
                &mut playback_source,
                hardware_ready,
                sequence_lead,
            );
        }

        self.run_split_workers(
            stop,
            max_periods,
            hardware_ready,
            u64::from(self.config.pro_latency_periods),
            Some((&mut capture_sink, &mut playback_source)),
        )
    }

    fn run_split_workers<C: ProCaptureSink, P: ProPlaybackSource>(
        &mut self,
        stop: &AtomicBool,
        max_periods: Option<u64>,
        hardware_ready: Option<&AtomicBool>,
        sequence_lead: u64,
        pro: Option<(&mut C, &mut P)>,
    ) -> Result<(), EngineError> {
        let is_pro = pro.is_some();
        let (capture_sink, playback_source) = pro.unzip();
        let mut playback_scratch = if is_pro {
            vec![0; self.playback_period_samples]
        } else {
            Vec::new()
        };
        let pro_clock = ProClock::new(sequence_lead);
        let deferred_worker_start = self.prepare_worker_start()?;

        let done = AtomicBool::new(false);
        let capture_cycle_generation = AtomicU64::new(u64::MAX);
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
        // Workers hand their PCM back through these slots, so a failed spawn keeps the device.
        let mut playback_slot = Some(playback_pcm);
        let mut capture_slot = Some(capture_pcm);
        let playback_silence = &self.playback_silence;
        let capture_scratch = &mut self.capture_scratch;
        let timeline = &self.timeline;
        let capture_config = CaptureWorkerConfig {
            channels: self.capture_channels,
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
            hardware_ready,
            capture_cycle_generation: is_pro.then_some(&capture_cycle_generation),
        };
        let playback_control = capture_control;
        let outcome = std::thread::scope(|scope| -> Result<_, EngineError> {
            let pro_clock = is_pro.then_some(&pro_clock);
            let capture_slot = &mut capture_slot;
            let playback_slot = &mut playback_slot;
            let playback_scratch = is_pro.then_some(&mut playback_scratch[..]);
            let capture_handle = std::thread::Builder::new()
                .spawn_scoped(scope, move || {
                    signal_worker_panic(capture_control, || {
                        let pcm = capture_slot.take().ok_or_else(|| {
                            EngineError::InvalidConfig("capture worker is active".into())
                        })?;
                        let (pcm, result) = capture_worker(
                            pcm,
                            capture_scratch,
                            capture_config,
                            capture_control,
                            pro_clock,
                            capture_sink,
                        );
                        *capture_slot = Some(pcm);
                        result
                    })
                })
                .map_err(|source| EngineError::WorkerSpawn {
                    worker: "capture",
                    source,
                })?;
            let playback_handle = match std::thread::Builder::new().spawn_scoped(scope, move || {
                signal_worker_panic(playback_control, || {
                    let pcm = playback_slot.take().ok_or_else(|| {
                        EngineError::InvalidConfig("playback worker is active".into())
                    })?;
                    let (pcm, result) = playback_worker(
                        pcm,
                        playback_config,
                        playback_control,
                        pro_clock,
                        playback_scratch,
                        playback_source,
                        max_periods,
                    );
                    *playback_slot = Some(pcm);
                    result
                })
            }) {
                Ok(handle) => handle,
                Err(source) => {
                    done.store(true, Ordering::Release);
                    let _ = capture_handle.join();
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
            Ok(playback_result.and(capture_result))
        });
        self.playback_pcm = playback_slot;
        self.capture_pcm = capture_slot;
        let result = outcome.and_then(|result| result);
        if result.is_err() {
            self.cleanup_startup_failure();
        }
        result
    }

    fn run_pro_linked<C: ProCaptureSink, P: ProPlaybackSource>(
        &mut self,
        stop: &AtomicBool,
        max_periods: Option<u64>,
        capture_sink: &mut C,
        playback_source: &mut P,
        hardware_ready: Option<&AtomicBool>,
        sequence_lead: u64,
    ) -> Result<(), EngineError> {
        let mut playback_scratch = vec![0; self.playback_period_samples];
        let _realtime = self.enter_realtime()?;
        let start_frames = self.prepare_linked_pro_start(sequence_lead)?;
        let _start_samples = usize::try_from(start_frames)
            .ok()
            .and_then(|frames| frames.checked_mul(self.playback_channels))
            .ok_or_else(|| EngineError::InvalidConfig("playback start size overflow".into()))?;

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
        let done = AtomicBool::new(false);
        let capture_cycle_generation = AtomicU64::new(u64::MAX);
        let hardware_period = i64::from(self.config.effective_hardware_period_size());
        let configured_playback_floor =
            i64::from(self.config.effective_linked_playback_guard_frames());
        let playback_floor = if sequence_lead == 0 {
            linked_zero_lead_playback_floor(
                configured_playback_floor,
                hardware_period,
                self.buffer,
                self.period,
            )
        } else {
            configured_playback_floor
        };
        let control = WorkerControl {
            timeline: &self.timeline,
            stop,
            done: &done,
            start_gate: None,
            hardware_ready,
            capture_cycle_generation: Some(&capture_cycle_generation),
        };
        let config = LinkedProConfig {
            playback_silence: &self.playback_silence,
            playback_channels: self.playback_channels,
            capture_channels: self.capture_channels,
            playback_period_samples: self.playback_period_samples,
            start_frames,
            period: self.period,
            buffer: self.buffer,
            hardware_period,
            playback_floor,
            direct_reserve_frames: i64::from(direct_pro_write_reserve_frames(
                self.config.period_size,
                self.config.rate,
            )),
            phase_max_attempts: self.config.linked_phase_max_attempts,
            rate: self.config.rate,
            handoff_nanos: self.config.pro_handoff_nanos(),
            sequence_lead,
            event_driven: self.config.uses_event_driven_linked_pro(),
            startup_loopback: self.config.startup_loopback,
        };
        let startup = if config.event_driven {
            normalize_startup_loopback(&playback_pcm, &capture_pcm, config, control)
                .map(|padding| config.start_frames.saturating_add(padding))
        } else {
            calibrate_linked_phase(
                &playback_pcm,
                &capture_pcm,
                &mut self.capture_scratch,
                config,
                control,
            )
            .map(|()| config.start_frames)
        };
        let result = startup.and_then(|initial_playback_lead| {
            run_catching_worker_panic(control, || {
                linked_pro_cycle_loop(
                    &playback_pcm,
                    &capture_pcm,
                    &mut self.capture_scratch,
                    &mut playback_scratch,
                    config,
                    initial_playback_lead,
                    control,
                    capture_sink,
                    playback_source,
                    max_periods,
                )
            })
        });
        self.playback_pcm = Some(playback_pcm);
        self.capture_pcm = Some(capture_pcm);
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.is_stopped() => {
                self.cleanup_startup_failure();
                Ok(())
            }
            Err(error) => {
                self.cleanup_startup_failure();
                Err(error)
            }
        }
    }

    pub fn stop(&mut self) -> Result<(), EngineError> {
        if !self.started && !self.streams_linked {
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
        if self.streams_linked {
            if self.started
                && alsa_call(
                    playback_pcm.drop(),
                    "stop linked duplex streams",
                    StreamDirection::Playback,
                )
                .is_ok()
            {
                self.started = false;
            }
            if self.started {
                alsa_call(
                    playback_pcm.unlink(),
                    "unlink running duplex streams",
                    StreamDirection::Playback,
                )?;
                self.streams_linked = false;
            } else {
                alsa_call(
                    playback_pcm.unlink(),
                    "unlink stopped duplex streams",
                    StreamDirection::Playback,
                )?;
                self.streams_linked = false;
                return Ok(());
            }
        }

        let playback_result = alsa_call(
            playback_pcm.drop(),
            "stop playback stream",
            StreamDirection::Playback,
        );
        let capture_result = alsa_call(
            capture_pcm.drop(),
            "stop capture stream",
            StreamDirection::Capture,
        );
        let result = playback_result.and(capture_result);
        if result.is_ok() {
            self.started = false;
        }
        result
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

impl RealtimeGuard {
    fn enter(priority: i32, operation: &'static str) -> Result<Self, EngineError> {
        let policy = unsafe { libc::sched_getscheduler(0) };
        if policy < 0 {
            return Err(EngineError::RealtimeScheduling {
                operation: "read current policy",
                source: io::Error::last_os_error(),
            });
        }
        let mut parameters = libc::sched_param { sched_priority: 0 };
        if unsafe { libc::sched_getparam(0, &mut parameters) } != 0 {
            return Err(EngineError::RealtimeScheduling {
                operation: "read current priority",
                source: io::Error::last_os_error(),
            });
        }

        set_current_realtime(priority, operation)?;
        Ok(Self {
            policy,
            priority: parameters.sched_priority,
        })
    }
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

fn set_current_realtime(priority: i32, operation: &'static str) -> Result<(), EngineError> {
    let parameters = libc::sched_param {
        sched_priority: priority,
    };
    if unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &parameters) } != 0 {
        Err(EngineError::RealtimeScheduling {
            operation,
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
    hardware_ready: Option<&'a AtomicBool>,
    capture_cycle_generation: Option<&'a AtomicU64>,
}

impl WorkerControl<'_> {
    fn should_stop(self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.done.load(Ordering::Acquire)
    }

    fn ensure_running(self) -> Result<(), EngineError> {
        if self.should_stop() {
            Err(EngineError::Stopped)
        } else {
            Ok(())
        }
    }

    #[inline]
    fn mark_capture_cycle_ready(self) {
        if self
            .hardware_ready
            .is_none_or(|ready| ready.load(Ordering::Relaxed))
        {
            return;
        }
        if let Some(generation) = self.capture_cycle_generation {
            generation.store(self.timeline.generation(), Ordering::Release);
        }
    }

    #[inline]
    fn publish_hardware_ready(self) {
        let Some(ready) = self.hardware_ready else {
            return;
        };
        if ready.load(Ordering::Relaxed) {
            return;
        }
        if self.stop.load(Ordering::Relaxed) || self.done.load(Ordering::Acquire) {
            return;
        }
        let generation = self.timeline.generation();
        if self
            .capture_cycle_generation
            .is_some_and(|capture| capture.load(Ordering::Acquire) == generation)
            && generation == self.timeline.generation()
        {
            ready.store(true, Ordering::Release);
        }
    }
}

struct NoopPro;

impl ProCaptureSink for NoopPro {
    fn process_capture(&mut self, _sequence: u64, _capture: &[i32]) {}
}

impl ProPlaybackSource for NoopPro {
    fn process_playback(&mut self, _sequence: u64, playback: &mut [i32]) {
        playback.fill(0);
    }
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

// Probe cycles per post-recovery phase attempt; kept short because clients are live.
const LINKED_PHASE_RECOVERY_CYCLES: u64 = 16;

#[repr(align(64))]
struct CacheAlignedAtomicU64(AtomicU64);

// Keep playback-written `sequence` and capture-written `capture_next` on
// separate cache lines so split workers never false-share every period.
struct ProClock {
    sequence: CacheAlignedAtomicU64,
    capture_next: CacheAlignedAtomicU64,
}

impl ProClock {
    fn new(sequence: u64) -> Self {
        Self {
            sequence: CacheAlignedAtomicU64(AtomicU64::new(sequence)),
            capture_next: CacheAlignedAtomicU64(AtomicU64::new(sequence)),
        }
    }

    #[inline]
    fn load(&self) -> u64 {
        self.sequence.0.load(Ordering::Acquire)
    }

    #[inline]
    fn publish(&self, sequence: u64) {
        self.sequence.0.store(sequence, Ordering::Release);
    }

    #[inline]
    fn capture_next(&self) -> u64 {
        self.capture_next.0.load(Ordering::Acquire)
    }

    #[inline]
    fn publish_capture_next(&self, sequence: u64) {
        self.capture_next.0.store(sequence, Ordering::Release);
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
    period: alsa::pcm::Frames,
    initial_pro_sequence: u64,
    realtime_priority: Option<i32>,
}

#[derive(Clone, Copy)]
struct DirectDuplexReadiness {
    playback_queued_frames: alsa::pcm::Frames,
    observed_nanos: u64,
}

#[derive(Clone, Copy)]
struct LinkedProConfig<'a> {
    playback_silence: &'a [i32],
    playback_channels: usize,
    capture_channels: usize,
    playback_period_samples: usize,
    start_frames: alsa::pcm::Frames,
    period: alsa::pcm::Frames,
    buffer: alsa::pcm::Frames,
    hardware_period: alsa::pcm::Frames,
    playback_floor: alsa::pcm::Frames,
    direct_reserve_frames: alsa::pcm::Frames,
    phase_max_attempts: u32,
    rate: u32,
    handoff_nanos: u64,
    sequence_lead: u64,
    event_driven: bool,
    startup_loopback: Option<StartupLoopbackConfig>,
}

const MAX_LINKED_POLL_DESCRIPTORS: usize = 32;

// Pre-populated ALSA poll descriptor set laid out as
// `[playback fds..., capture fds...]` so direct-duplex waits select the active
// subslice without calling `snd_pcm_poll_descriptors_count` / `_descriptors`
// or zero-initializing a stack array on every period.
struct LinkedPollCache {
    descriptors: [pollfd; MAX_LINKED_POLL_DESCRIPTORS],
    playback_count: usize,
    capture_count: usize,
}

impl LinkedPollCache {
    fn new(playback_pcm: &PCM, capture_pcm: &PCM) -> Result<Self, EngineError> {
        let mut cache = Self {
            descriptors: [pollfd {
                fd: 0,
                events: 0,
                revents: 0,
            }; MAX_LINKED_POLL_DESCRIPTORS],
            playback_count: 0,
            capture_count: 0,
        };
        cache.refresh(playback_pcm, capture_pcm)?;
        Ok(cache)
    }

    fn refresh(&mut self, playback_pcm: &PCM, capture_pcm: &PCM) -> Result<(), EngineError> {
        let playback_count = playback_pcm.count();
        let capture_count = capture_pcm.count();
        if playback_count.saturating_add(capture_count) > MAX_LINKED_POLL_DESCRIPTORS {
            return Err(EngineError::InvalidConfig(
                "linked PCM requires too many poll descriptors".into(),
            ));
        }
        let playback_filled = alsa_call(
            playback_pcm.fill(&mut self.descriptors[..playback_count]),
            "fill playback poll descriptors",
            StreamDirection::Playback,
        )?;
        let capture_filled = alsa_call(
            capture_pcm
                .fill(&mut self.descriptors[playback_filled..playback_filled + capture_count]),
            "fill capture poll descriptors",
            StreamDirection::Capture,
        )?;
        let total = playback_filled + capture_filled;
        for descriptor in &mut self.descriptors[..total] {
            descriptor.events |= libc::POLLERR;
            descriptor.revents = 0;
        }
        self.playback_count = playback_filled;
        self.capture_count = capture_filled;
        Ok(())
    }

    #[inline]
    fn active_fds_mut(&mut self, playback_ready: bool, capture_ready: bool) -> &mut [pollfd] {
        let total = self.playback_count + self.capture_count;
        let range = match (playback_ready, capture_ready) {
            (false, false) => 0..total,
            (true, false) => self.playback_count..total,
            (false, true) => 0..self.playback_count,
            (true, true) => 0..0,
        };
        let active = &mut self.descriptors[range];
        for descriptor in active.iter_mut() {
            descriptor.revents = 0;
        }
        active
    }
}

// Multiples of 32 24-bit LSBs distinguish 1 dB mixer steps while peaking below -98 dBFS.
const STARTUP_LOOPBACK_MARKERS: [[i32; 3]; 2] = [[8192, -8192, 16384], [-16384, 24576, -24576]];

struct StartupLoopbackProbe {
    markers: [i32; 3],
    interval: u64,
    delays: [Option<u64>; 3],
}

impl StartupLoopbackProbe {
    fn emit(&self, position: u64, mapped: &mut [i32], channels: usize, channel: usize) {
        mapped.fill(0);
        let end = position + (mapped.len() / channels) as u64;
        for (index, &marker) in self.markers.iter().enumerate() {
            let output_frame = (index as u64 + 1) * self.interval;
            if (position..end).contains(&output_frame) {
                mapped[(output_frame - position) as usize * channels + channel] = marker;
            }
        }
    }

    fn observe(
        &mut self,
        position: u64,
        output_end: u64,
        mapped: &[i32],
        channels: usize,
        channel: usize,
    ) -> Result<(), EngineError> {
        for (frame, samples) in mapped.chunks_exact(channels).enumerate() {
            let Some(index) = self
                .markers
                .iter()
                .position(|&marker| marker == samples[channel])
            else {
                continue;
            };
            let output_frame = (index as u64 + 1) * self.interval;
            if output_frame >= output_end {
                continue; // This marker has not been committed to playback yet.
            }
            let delay = (position + frame as u64).checked_sub(output_frame).ok_or(
                EngineError::StartupLoopback("probe arrived before its logical output frame"),
            )?;
            if self.delays[index].replace(delay).is_some() {
                return Err(EngineError::StartupLoopback("ambiguous repeated probe"));
            }
        }
        Ok(())
    }

    fn measured_delay(&self, expected: Option<u64>) -> Result<Option<u64>, EngineError> {
        if let Some(expected) = expected
            && self.delays.iter().flatten().any(|&delay| delay != expected)
        {
            return Err(EngineError::StartupLoopback(
                "padded delay does not match target",
            ));
        }
        let Some(first) = self.delays[0] else {
            return Ok(None);
        };
        if self.delays.iter().flatten().any(|&delay| delay != first) {
            return Err(EngineError::StartupLoopback("unstable loopback delay"));
        }
        Ok(self.delays.iter().all(Option::is_some).then_some(first))
    }
}

fn startup_loopback_padding(
    measured: u64,
    target: u32,
    start_frames: alsa::pcm::Frames,
    buffer: alsa::pcm::Frames,
    period: alsa::pcm::Frames,
) -> Result<usize, EngineError> {
    let padding = u64::from(target)
        .checked_sub(measured)
        .ok_or(EngineError::StartupLoopback(
            "unpadded loopback delay already exceeds target",
        ))?;
    let capacity = buffer
        .checked_sub(start_frames)
        .and_then(|frames| frames.checked_sub(period))
        .and_then(|frames| u64::try_from(frames).ok())
        .ok_or(EngineError::StartupLoopback(
            "no full client block reserve in playback buffer",
        ))?;
    if padding > capacity {
        return Err(EngineError::StartupLoopback(
            "padding would consume the client block reserve",
        ));
    }
    usize::try_from(padding)
        .map_err(|_| EngineError::StartupLoopback("padding frame count overflow"))
}

/// Discard startup capture before the client timeline origin, but preserve queued
/// playback zeros: padding advances the real ALSA app pointer, not logical output
/// ordinals. Exact, distinct quiet markers survive 24-bit S32_LE digital paths;
/// this intentionally cannot qualify an analog, scaled, or noisy loopback.
fn normalize_startup_loopback(
    playback_pcm: &PCM,
    capture_pcm: &PCM,
    config: LinkedProConfig<'_>,
    control: WorkerControl<'_>,
) -> Result<alsa::pcm::Frames, EngineError> {
    let Some(loopback) = config.startup_loopback else {
        return Ok(0);
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    // Requalification during recovery must not leave the engine looking unqualified,
    // or the next XRUN would be treated as a fatal startup interruption.
    let was_ready = control
        .hardware_ready
        .is_some_and(|ready| ready.swap(false, Ordering::AcqRel));
    let result = (|| {
        let period = config.period as usize;
        let interval = (u64::from(loopback.target_frames) + 2 * period as u64)
            .max(u64::from(config.rate) / 12 + 1);
        let mut applied_padding = 0 as alsa::pcm::Frames;
        for (phase, markers) in STARTUP_LOOPBACK_MARKERS.into_iter().enumerate() {
            let mut probe = StartupLoopbackProbe {
                markers,
                interval,
                delays: [None; 3],
            };
            let mut position = 0;
            let measured = loop {
                wait_for_startup_loopback(
                    &[
                        (playback_pcm, StreamDirection::Playback, period),
                        (capture_pcm, StreamDirection::Capture, period),
                    ],
                    control,
                    deadline,
                )?;
                transfer_startup_loopback(
                    capture_pcm,
                    StreamDirection::Capture,
                    config.capture_channels,
                    period,
                    control,
                    deadline,
                    |offset, mapped| {
                        probe.observe(
                            position + offset as u64,
                            position,
                            mapped,
                            config.capture_channels,
                            loopback.capture_channel as usize,
                        )
                    },
                )?;
                transfer_startup_loopback(
                    playback_pcm,
                    StreamDirection::Playback,
                    config.playback_channels,
                    period,
                    control,
                    deadline,
                    |offset, mapped| {
                        probe.emit(
                            position + offset as u64,
                            mapped,
                            config.playback_channels,
                            loopback.playback_channel as usize,
                        );
                        Ok(())
                    },
                )?;
                position += period as u64;
                let expected = (phase == 1).then_some(u64::from(loopback.target_frames));
                if let Some(delay) = probe.measured_delay(expected)? {
                    break delay;
                }
                if position >= 4 * interval {
                    return Err(EngineError::StartupLoopback(
                        "missing digital loopback probe",
                    ));
                }
            };
            if phase == 0 {
                let padding = startup_loopback_padding(
                    measured,
                    loopback.target_frames,
                    config.start_frames,
                    config.buffer,
                    config.period,
                )?;
                transfer_startup_loopback(
                    playback_pcm,
                    StreamDirection::Playback,
                    config.playback_channels,
                    padding,
                    control,
                    deadline,
                    |_, mapped| {
                        mapped.fill(0);
                        Ok(())
                    },
                )?;
                applied_padding = alsa::pcm::Frames::try_from(padding).unwrap_or(0);
                // Reset both measurement origins together. Do not count padding
                // as logical output or change the base prime used on recovery.
            }
        }
        check_startup_loopback_deadline(control, deadline)?;
        Ok(applied_padding)
    })();
    if result.is_ok()
        && was_ready
        && let Some(ready) = control.hardware_ready
    {
        ready.store(true, Ordering::Release);
    }
    result.map_err(|error| {
        record_linked_hardware_xruns(&error, control);
        startup_loopback_error(error)
    })
}

fn startup_loopback_error(error: EngineError) -> EngineError {
    if error.is_stopped() || matches!(&error, EngineError::StartupLoopback(_)) {
        error
    } else {
        // Every failed qualification, including a short commit, must suppress service retries.
        EngineError::StartupLoopback("PCM transfer failed during normalization")
    }
}

fn check_startup_loopback_deadline(
    control: WorkerControl<'_>,
    deadline: Instant,
) -> Result<(), EngineError> {
    control.ensure_running()?;
    if Instant::now() >= deadline {
        return Err(EngineError::StartupLoopback("two-second deadline expired"));
    }
    Ok(())
}

fn wait_for_startup_loopback(
    streams: &[(&PCM, StreamDirection, usize)],
    control: WorkerControl<'_>,
    deadline: Instant,
) -> Result<(), EngineError> {
    let mut descriptors = [pollfd {
        fd: 0,
        events: 0,
        revents: 0,
    }; 32];
    loop {
        check_startup_loopback_deadline(control, deadline)?;
        let mut used = 0;
        for &(pcm, direction, required) in streams {
            let available = alsa_call(
                pcm.avail_update(),
                "update startup loopback availability",
                direction,
            )?;
            if available >= required as i64 {
                continue;
            }
            let count = pcm.count();
            if count == 0 || count > descriptors.len() - used {
                return Err(EngineError::StartupLoopback(
                    "invalid PCM poll descriptor count",
                ));
            }
            let filled = alsa_call(
                pcm.fill(&mut descriptors[used..used + count]),
                "fill startup loopback poll descriptors",
                direction,
            )?;
            if filled != count {
                return Err(EngineError::StartupLoopback(
                    "incomplete PCM poll descriptors",
                ));
            }
            used += count;
        }
        if used == 0 {
            return check_startup_loopback_deadline(control, deadline);
        }
        for descriptor in &mut descriptors[..used] {
            descriptor.events |= libc::POLLERR;
            descriptor.revents = 0;
        }
        check_startup_loopback_deadline(control, deadline)?;
        match poll::poll(&mut descriptors[..used], 1) {
            Err(error) if error.errno() == libc::EINTR => continue,
            result => {
                alsa_call(result, "poll startup loopback", streams[0].1)?;
            }
        }
        if descriptors[..used]
            .iter()
            .any(|descriptor| descriptor.revents & (libc::POLLHUP | libc::POLLNVAL) != 0)
        {
            return Err(EngineError::StartupLoopback(
                "PCM poll disconnected or invalid",
            ));
        }
        // Recheck availability, including POLLERR, and poll only missing directions.
    }
}

fn transfer_startup_loopback(
    pcm: &PCM,
    direction: StreamDirection,
    channels: usize,
    frames: usize,
    control: WorkerControl<'_>,
    deadline: Instant,
    mut process: impl FnMut(usize, &mut [i32]) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    let io = unsafe { pcm.io_unchecked::<i32>() };
    let mut offset = 0;
    while offset < frames {
        check_startup_loopback_deadline(control, deadline)?;
        let available = alsa_call(
            pcm.avail_update(),
            "update startup loopback transfer availability",
            direction,
        )?;
        if available <= 0 {
            wait_for_startup_loopback(&[(pcm, direction, 1)], control, deadline)?;
            continue;
        }
        let mut mapped_frames = 0;
        let mut processed = Ok(());
        let committed = alsa_call(
            io.mmap((frames - offset).min(available as usize), |mapped| {
                mapped_frames = mapped.len() / channels;
                processed = process(offset, mapped);
                mapped_frames
            }),
            "transfer startup loopback frames",
            direction,
        )?;
        if committed != mapped_frames {
            return Err(EngineError::ShortCommit {
                direction,
                actual: committed as i64,
                required: mapped_frames as i64,
            });
        }
        processed?;
        offset += committed;
        if committed == 0 {
            wait_for_startup_loopback(&[(pcm, direction, 1)], control, deadline)?;
        }
        // Positive partial progress can be ring wrap, not a reason to wait for avail_min.
    }
    check_startup_loopback_deadline(control, deadline)
}

fn calibrate_linked_phase(
    playback_pcm: &PCM,
    capture_pcm: &PCM,
    capture_scratch: &mut [i32],
    config: LinkedProConfig<'_>,
    control: WorkerControl<'_>,
) -> Result<(), EngineError> {
    if config.phase_max_attempts == 0 {
        return Ok(());
    }

    let target_nanos =
        linked_phase_target_nanos(config.hardware_period, config.rate, config.handoff_nanos);
    // Consume one second of hardware time so startup-only schedule changes
    // finish before any client can observe the stream.
    let warmup_cycles = linked_phase_warmup_cycles(config.rate, config.period);
    for attempt in 1..=config.phase_max_attempts {
        let mut minimum_write_nanos = u64::MAX;
        let mut recovered = false;

        for _ in 0..warmup_cycles {
            if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
                return Ok(());
            }
            match linked_phase_calibration_cycle(
                playback_pcm,
                capture_pcm,
                capture_scratch,
                config,
                control,
            ) {
                Ok(Some(elapsed_nanos)) => {
                    minimum_write_nanos = minimum_write_nanos.min(elapsed_nanos);
                    if elapsed_nanos < target_nanos {
                        break;
                    }
                }
                Ok(None) => return Ok(()),
                Err(error) if error.is_stopped() => return Ok(()),
                Err(error) if error.is_recoverable() => {
                    if attempt == config.phase_max_attempts {
                        record_linked_hardware_xruns(&error, control);
                        control.timeline.record_linked_phase_calibration(
                            u64::from(attempt),
                            0,
                            false,
                        );
                        return Err(EngineError::LinkedStartupUnstable {
                            attempts: attempt,
                            minimum_write_nanos: 0,
                        });
                    }
                    let dither_frames = linked_phase_dither_frames(attempt, config.hardware_period);
                    recover_linked_streams(
                        playback_pcm,
                        capture_pcm,
                        &error,
                        control,
                        config,
                        dither_frames,
                    )?;
                    recovered = true;
                    break;
                }
                Err(error) => return Err(error),
            }
        }

        if recovered {
            control
                .timeline
                .record_linked_phase_calibration(u64::from(attempt), 0, false);
            continue;
        }

        let score_nanos = if minimum_write_nanos == u64::MAX {
            0
        } else {
            minimum_write_nanos
        };
        let target_met = score_nanos >= target_nanos;
        control.timeline.record_linked_phase_calibration(
            u64::from(attempt),
            score_nanos,
            target_met,
        );
        if target_met {
            return Ok(());
        }
        if attempt == config.phase_max_attempts {
            return Err(EngineError::LinkedStartupUnstable {
                attempts: attempt,
                minimum_write_nanos: score_nanos,
            });
        }

        let dither_frames = linked_phase_dither_frames(attempt, config.hardware_period);
        rebase_linked_streams(playback_pcm, capture_pcm, control, config, dither_frames)?;
    }
    Ok(())
}

/// A restart lands on an arbitrary USB phase, so the startup phase qualification no longer
/// holds. Unlike startup this runs inside a live session: keep each probe short and never
/// stop the engine just because the target phase could not be reached.
fn recalibrate_linked_phase(
    playback_pcm: &PCM,
    capture_pcm: &PCM,
    capture_scratch: &mut [i32],
    config: LinkedProConfig<'_>,
    control: WorkerControl<'_>,
) -> Result<(), EngineError> {
    if config.phase_max_attempts == 0 {
        return Ok(());
    }
    let target_nanos =
        linked_phase_target_nanos(config.hardware_period, config.rate, config.handoff_nanos);
    for attempt in 1..=config.phase_max_attempts {
        let mut minimum_write_nanos = u64::MAX;
        let mut recovered = false;
        for _ in 0..LINKED_PHASE_RECOVERY_CYCLES {
            match linked_phase_calibration_cycle(
                playback_pcm,
                capture_pcm,
                capture_scratch,
                config,
                control,
            ) {
                Ok(Some(elapsed_nanos)) => {
                    minimum_write_nanos = minimum_write_nanos.min(elapsed_nanos);
                    if elapsed_nanos < target_nanos {
                        break;
                    }
                }
                Ok(None) => return Ok(()),
                Err(error) if error.is_stopped() => return Ok(()),
                Err(error) if error.is_recoverable() => {
                    recover_linked_streams(
                        playback_pcm,
                        capture_pcm,
                        &error,
                        control,
                        config,
                        linked_phase_dither_frames(attempt, config.hardware_period),
                    )?;
                    recovered = true;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if recovered {
            control
                .timeline
                .record_linked_phase_calibration(u64::from(attempt), 0, false);
            continue;
        }
        let score_nanos = if minimum_write_nanos == u64::MAX {
            0
        } else {
            minimum_write_nanos
        };
        let target_met = score_nanos >= target_nanos;
        control.timeline.record_linked_phase_calibration(
            u64::from(attempt),
            score_nanos,
            target_met,
        );
        if target_met || attempt == config.phase_max_attempts {
            return Ok(());
        }
        rebase_linked_streams(
            playback_pcm,
            capture_pcm,
            control,
            config,
            linked_phase_dither_frames(attempt, config.hardware_period),
        )?;
    }
    Ok(())
}

fn linked_phase_calibration_cycle(
    playback_pcm: &PCM,
    capture_pcm: &PCM,
    capture_scratch: &mut [i32],
    config: LinkedProConfig<'_>,
    control: WorkerControl<'_>,
) -> Result<Option<u64>, EngineError> {
    let read = read_capture_samples(
        capture_pcm,
        capture_scratch,
        config.capture_channels,
        config.period as usize,
        Some(control),
    )?;
    if read != config.period {
        return Err(EngineError::ShortCommit {
            direction: StreamDirection::Capture,
            actual: read,
            required: config.period,
        });
    }
    if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
        return Ok(None);
    }

    let capture_read_nanos = monotonic_nanos();
    let playback_delay = alsa_call(
        playback_pcm.delay(),
        "read calibration playback delay",
        StreamDirection::Playback,
    )?;
    let handoff_started_nanos = monotonic_nanos();
    let target_nanos = bounded_pro_handoff_target(
        capture_read_nanos,
        handoff_started_nanos,
        config.handoff_nanos,
        playback_delay,
        config.period,
        config.rate,
    );
    wait_for_handoff_target(target_nanos, control)?;
    wait_for_playback_target(
        playback_pcm,
        config.buffer,
        config.playback_floor,
        config.rate,
        config.handoff_nanos,
        control,
    )?;
    let written = write_playback_samples(
        playback_pcm,
        config.playback_silence,
        config.playback_channels,
        config.period as usize,
        Some(control),
    )?;
    if written != config.period {
        return Err(EngineError::ShortCommit {
            direction: StreamDirection::Playback,
            actual: written,
            required: config.period,
        });
    }
    Ok(Some(monotonic_nanos().saturating_sub(capture_read_nanos)))
}

fn linked_phase_target_nanos(
    hardware_period: alsa::pcm::Frames,
    rate: u32,
    handoff_nanos: u64,
) -> u64 {
    let hardware_period_nanos = u64::try_from(hardware_period)
        .unwrap_or(1)
        .max(1)
        .saturating_mul(1_000_000_000)
        / u64::from(rate);
    let overhead_nanos = hardware_period_nanos / u64::from(LINKED_PHASE_OVERHEAD_DIVISOR);
    handoff_nanos.saturating_sub(overhead_nanos)
}

fn linked_phase_warmup_cycles(rate: u32, period: alsa::pcm::Frames) -> u64 {
    u64::from(rate).div_ceil(u64::try_from(period).unwrap_or(1).max(1))
}

fn linked_phase_dither_frames(
    attempt: u32,
    hardware_period: alsa::pcm::Frames,
) -> alsa::pcm::Frames {
    let hardware_period = u32::try_from(hardware_period).unwrap_or(1).max(1);
    alsa::pcm::Frames::from(attempt % hardware_period)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingProCapture {
    hardware_sequence: u64,
    playback_sequence: u64,
    target_nanos: u64,
}

fn take_pending_pro_capture(
    pending: &mut Option<PendingProCapture>,
    playback_sequence: u64,
) -> Option<PendingProCapture> {
    if pending
        .as_ref()
        .is_some_and(|pending| pending.playback_sequence == playback_sequence)
    {
        pending.take()
    } else {
        None
    }
}

fn bounded_pro_handoff_target(
    capture_read_nanos: u64,
    handoff_started_nanos: u64,
    handoff_nanos: u64,
    playback_delay: alsa::pcm::Frames,
    reserve_frames: alsa::pcm::Frames,
    rate: u32,
) -> u64 {
    let configured_target = handoff_started_nanos.saturating_add(handoff_nanos);
    let margin_frames = u64::try_from(playback_delay.saturating_sub(reserve_frames)).unwrap_or(0);
    let hardware_budget_nanos = margin_frames.saturating_mul(1_000_000_000) / u64::from(rate);
    configured_target.min(capture_read_nanos.saturating_add(hardware_budget_nanos))
}

fn wait_for_pro_handoff(
    pending: PendingProCapture,
    control: WorkerControl<'_>,
) -> Result<(), EngineError> {
    let now = monotonic_nanos();
    let wait_nanos = pending.target_nanos.saturating_sub(now);
    control.timeline.record_pro_wait_budget(wait_nanos);
    if wait_nanos == 0 {
        control.timeline.record_pro_core_deadline_miss();
        return Ok(());
    }
    wait_for_handoff_target(pending.target_nanos, control)
}

#[inline]
fn prepare_pro_playback<P: ProPlaybackSource + ?Sized>(playback_source: &mut P, sequence: u64) {
    playback_source.prepare_playback(sequence);
    playback_source.prepare_playback_mix(sequence);
}

#[inline]
fn render_pro_playback<P: ProPlaybackSource + ?Sized>(
    playback_source: &mut P,
    sequence: u64,
    cutoff_nanos: Option<u64>,
    playback: &mut [i32],
) {
    if let Some(cutoff_nanos) = cutoff_nanos {
        playback_source.process_playback_before(sequence, cutoff_nanos, playback);
    } else {
        playback_source.process_playback(sequence, playback);
    }
    playback_source.commit_playback(sequence, playback);
}

struct PlaybackCommitGuard<'a, P: ProPlaybackSource + ?Sized> {
    source: &'a mut P,
    active: bool,
}

impl<'a, P: ProPlaybackSource + ?Sized> PlaybackCommitGuard<'a, P> {
    #[inline]
    fn begin(source: &'a mut P) -> Self {
        source.begin_playback_commit();
        Self {
            source,
            active: true,
        }
    }

    #[inline]
    fn source(&mut self) -> &mut P {
        self.source
    }

    #[inline]
    fn source_ref(&self) -> &P {
        self.source
    }

    #[inline]
    fn finish(&mut self) {
        if self.active {
            self.active = false;
            self.source.end_playback_commit();
        }
    }
}

impl<P: ProPlaybackSource + ?Sized> Drop for PlaybackCommitGuard<'_, P> {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            let _ = catch_unwind(AssertUnwindSafe(|| self.source.end_playback_commit()));
        }
    }
}

// Stream recovery can take seconds; client STOP/CLOSE must not spin on the barrier meanwhile.
fn recover_with_commit_released<P: ProPlaybackSource + ?Sized, T>(
    playback_commit: &mut PlaybackCommitGuard<'_, P>,
    recover: impl FnOnce() -> Result<T, EngineError>,
) -> Result<T, EngineError> {
    playback_commit.finish();
    recover()
}

#[inline]
fn publish_deferred_capture<C: ProCaptureSink + ?Sized>(
    capture_sink: &mut C,
    pending: PendingProCapture,
    capture: &[i32],
) {
    capture_sink.process_deferred_capture(pending.hardware_sequence, capture);
}

fn run_catching_worker_panic(
    control: WorkerControl<'_>,
    body: impl FnOnce() -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(_) => {
            control.done.store(true, Ordering::Release);
            Err(EngineError::WorkerPanic)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn linked_pro_cycle_loop<C: ProCaptureSink, P: ProPlaybackSource>(
    playback_pcm: &PCM,
    capture_pcm: &PCM,
    capture_scratch: &mut [i32],
    playback_scratch: &mut [i32],
    config: LinkedProConfig<'_>,
    mut playback_lead_frames: alsa::pcm::Frames,
    control: WorkerControl<'_>,
    capture_sink: &mut C,
    playback_source: &mut P,
    max_periods: Option<u64>,
) -> Result<(), EngineError> {
    if uses_staged_packet_cycle(config.sequence_lead, config.period, config.hardware_period) {
        return linked_pro_packet_cycle_loop(
            playback_pcm,
            capture_pcm,
            capture_scratch,
            playback_scratch,
            config,
            control,
            capture_sink,
            playback_source,
            max_periods,
        );
    }

    if config.sequence_lead != 0 {
        return linked_pro_ahead_cycle_loop(
            playback_pcm,
            capture_pcm,
            capture_scratch,
            playback_scratch,
            config,
            control,
            capture_sink,
            playback_source,
            max_periods,
        );
    }

    let mut playback_position = 0_u64;
    let mut capture_position = 0_u64;
    let mut sequence = 0_u64;
    let mut periods_processed = 0_u64;

    let mut latency_guard = LatencyGuard::new(config.period, config.rate);
    let mut pending_rebase = false;
    let mut poll_cache = if config.event_driven {
        Some(LinkedPollCache::new(playback_pcm, capture_pcm)?)
    } else {
        None
    };
    'cycles: loop {
        if control.stop.load(Ordering::Relaxed)
            || max_periods.is_some_and(|limit| periods_processed >= limit)
        {
            control.done.store(true, Ordering::Release);
            return Ok(());
        }
        let (direct_readiness, initial_capture_avail) = if let Some(cache) = poll_cache.as_mut() {
            match wait_for_linked_duplex_period(
                playback_pcm,
                capture_pcm,
                cache,
                config.period,
                config.buffer,
                playback_lead_frames,
                control,
            ) {
                Ok((readiness, capture_avail)) => (Some(readiness), Some(capture_avail)),
                Err(error) if error.is_stopped() => {
                    control.done.store(true, Ordering::Release);
                    return Ok(());
                }
                Err(error) if error.is_recoverable() => {
                    pending_rebase = false;
                    playback_lead_frames = recover_linked_streams_during_cycle(
                        playback_pcm,
                        capture_pcm,
                        capture_scratch,
                        &error,
                        config,
                        control,
                    )?;
                    cache.refresh(playback_pcm, capture_pcm)?;
                    continue;
                }
                Err(error) => {
                    control.done.store(true, Ordering::Release);
                    return Err(error);
                }
            }
        } else {
            (None, None)
        };
        if let Some(readiness) = direct_readiness {
            latency_guard.ready(control.timeline.generation(), readiness.observed_nanos);
        }
        let read = match read_capture_samples_with_initial(
            capture_pcm,
            capture_scratch,
            config.capture_channels,
            config.period as usize,
            initial_capture_avail,
            Some(control),
        ) {
            Ok(read) => read,
            Err(error) if error.is_stopped() => {
                control.done.store(true, Ordering::Release);
                return Ok(());
            }
            Err(error) if error.is_recoverable() => {
                pending_rebase = false;
                playback_lead_frames = recover_linked_streams_during_cycle(
                    playback_pcm,
                    capture_pcm,
                    capture_scratch,
                    &error,
                    config,
                    control,
                )?;
                if let Some(cache) = poll_cache.as_mut() {
                    cache.refresh(playback_pcm, capture_pcm)?;
                }
                continue;
            }
            Err(error) => {
                control.done.store(true, Ordering::Release);
                return Err(error);
            }
        };
        if read != config.period {
            control.done.store(true, Ordering::Release);
            return Err(EngineError::ShortCommit {
                direction: StreamDirection::Capture,
                actual: read,
                required: config.period,
            });
        }
        if control.stop.load(Ordering::Relaxed) {
            continue;
        }

        if config.event_driven && pending_rebase {
            pending_rebase = false;
            // Rebase before publishing this capture block to any clients.
            playback_lead_frames =
                match rebase_linked_streams(playback_pcm, capture_pcm, control, config, 0) {
                    Ok(()) => {
                        let padding =
                            normalize_startup_loopback(playback_pcm, capture_pcm, config, control)?;
                        config.start_frames.saturating_add(padding)
                    }
                    Err(error) if error.is_recoverable() => recover_linked_streams_during_cycle(
                        playback_pcm,
                        capture_pcm,
                        capture_scratch,
                        &error,
                        config,
                        control,
                    )?,
                    Err(error) => return Err(error),
                };
            if let Some(cache) = poll_cache.as_mut() {
                cache.refresh(playback_pcm, capture_pcm)?;
            }
            continue 'cycles;
        }

        let capture_read_nanos = monotonic_nanos();
        // In direct event-driven mode, defer the two SNDRV_PCM_IOCTL_STATUS_EXT
        // ioctls until after write_playback_samples so they never sit on the
        // capture-to-write XRUN critical path.
        let (capture_status, playback_status_at_capture) = if !config.event_driven {
            (
                pcm_status_with_audio_timestamp(capture_pcm).ok(),
                pcm_status_with_audio_timestamp(playback_pcm).ok(),
            )
        } else {
            (None, None)
        };
        let playback_delay_at_capture = if config.event_driven {
            None
        } else {
            playback_status_at_capture
                .as_ref()
                .map(Status::get_delay)
                .or_else(|| playback_pcm.delay().ok())
        };
        if !config.event_driven
            && let Some(delay) = capture_status
                .as_ref()
                .map(Status::get_delay)
                .or_else(|| capture_pcm.delay().ok())
        {
            control.timeline.update_pcm_delay(
                StreamDirection::Capture,
                delay,
                config.period as u64,
            );
        }
        if let (Some(playback_status), Some(capture_status)) =
            (playback_status_at_capture.as_ref(), capture_status.as_ref())
        {
            control
                .timeline
                .record_duplex_pointer_phase(duplex_pointer_phase_nanos(
                    playback_status,
                    capture_status,
                ));
        }
        capture_position = capture_position.wrapping_add(config.period as u64);
        control.timeline.update_capture_position(capture_position);
        let capture_sequence = sequence;
        control
            .timeline
            .record_pro_capture_read(capture_sequence, capture_read_nanos);
        playback_source.prepare_playback(sequence);
        capture_sink.process_capture_for_playback(sequence, capture_sequence, capture_scratch);
        control.mark_capture_cycle_ready();
        if control.stop.load(Ordering::Relaxed) {
            continue;
        }

        let cutoff_nanos = if config.event_driven {
            let readiness = direct_readiness.ok_or_else(|| {
                EngineError::InvalidConfig("direct duplex readiness is unavailable".into())
            })?;
            let cutoff_nanos = direct_pro_cutoff_nanos_with_reserve(
                readiness,
                config.direct_reserve_frames,
                config.rate,
                config.handoff_nanos,
            );
            let wait_nanos = cutoff_nanos.saturating_sub(monotonic_nanos());
            control.timeline.record_pro_wait_budget(wait_nanos);
            if wait_nanos == 0 {
                playback_source.mark_playback_budget_exhausted(sequence);
            }
            playback_source.wait_for_playback_before(sequence, cutoff_nanos);
            cutoff_nanos
        } else {
            // A delayed hardware wake must consume the client handoff, not the
            // playback period that keeps ALSA running.
            let handoff_started_nanos = monotonic_nanos();
            let target_nanos = playback_delay_at_capture.map_or_else(
                || handoff_started_nanos.saturating_add(config.handoff_nanos),
                |playback_delay| {
                    bounded_pro_handoff_target(
                        capture_read_nanos,
                        handoff_started_nanos,
                        config.handoff_nanos,
                        playback_delay,
                        config.period,
                        config.rate,
                    )
                },
            );
            playback_source.prepare_playback_mix(sequence);
            let pending_capture = PendingProCapture {
                hardware_sequence: sequence,
                playback_sequence: sequence,
                target_nanos,
            };
            if let Err(error) = wait_for_pro_handoff(pending_capture, control) {
                if error.is_stopped() {
                    control.done.store(true, Ordering::Release);
                    return Ok(());
                }
                control.done.store(true, Ordering::Release);
                return Err(error);
            }
            target_nanos
        };

        let mut playback_commit = PlaybackCommitGuard::begin(playback_source);
        let playback_source = playback_commit.source();
        let playback_slice = &mut playback_scratch[..config.playback_period_samples];
        playback_source.process_playback_before(sequence, cutoff_nanos, playback_slice);
        if config.event_driven {
            playback_source.prepare_playback_mix(sequence);
        }
        if control.stop.load(Ordering::Relaxed) {
            continue;
        }
        playback_source.commit_playback(sequence, playback_slice);

        if !config.event_driven {
            match wait_for_playback_target(
                playback_pcm,
                config.buffer,
                config.playback_floor,
                config.rate,
                config.handoff_nanos,
                control,
            ) {
                Ok(()) => {}
                Err(error) if error.is_recoverable() => {
                    recover_with_commit_released(&mut playback_commit, || {
                        recover_linked_streams_during_cycle(
                            playback_pcm,
                            capture_pcm,
                            capture_scratch,
                            &error,
                            config,
                            control,
                        )
                    })?;
                    sequence = sequence.wrapping_add(1);
                    continue;
                }
                Err(error) => {
                    control.done.store(true, Ordering::Release);
                    return Err(error);
                }
            }
        }
        if control.stop.load(Ordering::Relaxed) {
            continue;
        }

        // Zero lead always writes one whole period; leads and packets use other loops.
        let write_frames = config.period;
        let write_samples = config.playback_period_samples;

        if !config.event_driven
            && let Ok(status) = playback_pcm.status()
        {
            let delay = status.get_delay();
            control.timeline.update_pcm_delay(
                StreamDirection::Playback,
                delay,
                write_frames as u64,
            );
            control.timeline.update_playback_delay_breakdown(
                delay,
                status.get_avail(),
                config.buffer,
            );
        }

        let write_started_nanos = monotonic_nanos();
        let write_result = write_playback_samples(
            playback_pcm,
            &playback_scratch[..write_samples],
            config.playback_channels,
            write_frames as usize,
            Some(control),
        );
        let write_finished_nanos = monotonic_nanos();
        control
            .timeline
            .record_playback_write(write_finished_nanos.saturating_sub(write_started_nanos));
        match write_result {
            Ok(written) if written == write_frames => {
                playback_position = playback_position.wrapping_add(write_frames as u64);
                control.timeline.update_playback_position(playback_position);
                if let Some(readiness) = direct_readiness {
                    latency_guard.completed(readiness.observed_nanos, write_finished_nanos);
                }
                control.timeline.record_capture_to_playback_write_duration(
                    capture_read_nanos,
                    write_finished_nanos,
                );
            }
            Ok(written) => {
                control.done.store(true, Ordering::Release);
                return Err(EngineError::ShortCommit {
                    direction: StreamDirection::Playback,
                    actual: written,
                    required: write_frames,
                });
            }
            Err(error) if error.is_recoverable() => {
                pending_rebase = false;
                playback_lead_frames = recover_with_commit_released(&mut playback_commit, || {
                    recover_linked_streams_during_cycle(
                        playback_pcm,
                        capture_pcm,
                        capture_scratch,
                        &error,
                        config,
                        control,
                    )
                })?;
                if let Some(cache) = poll_cache.as_mut() {
                    cache.refresh(playback_pcm, capture_pcm)?;
                }
                sequence = sequence.wrapping_add(1);
                continue;
            }
            Err(error) if error.is_stopped() => {
                control.done.store(true, Ordering::Release);
                return Ok(());
            }
            Err(error) => {
                control.done.store(true, Ordering::Release);
                return Err(error);
            }
        }
        playback_commit.finish();
        capture_sink.process_deferred_capture(sequence, capture_scratch);
        if config.event_driven && latency_guard.sample_due(sequence) {
            let capture_status = pcm_status_with_audio_timestamp(capture_pcm).ok();
            let playback_status = pcm_status_with_audio_timestamp(playback_pcm).ok();
            if let Some(delay) = capture_status
                .as_ref()
                .map(Status::get_delay)
                .or_else(|| capture_pcm.delay().ok())
            {
                control.timeline.update_pcm_delay(
                    StreamDirection::Capture,
                    delay,
                    config.period as u64,
                );
            }
            if let Some(status) = playback_status.as_ref() {
                let (pre_write_delay, pre_write_avail) = pre_write_playback_status(
                    status.get_delay(),
                    status.get_avail(),
                    write_frames,
                    config.buffer,
                );
                control.timeline.update_pcm_delay(
                    StreamDirection::Playback,
                    pre_write_delay,
                    config.period as u64,
                );
                control.timeline.update_playback_delay_breakdown(
                    pre_write_delay,
                    pre_write_avail,
                    config.buffer,
                );
            }
            if let (Some(playback_status), Some(capture_status)) =
                (playback_status.as_ref(), capture_status.as_ref())
            {
                control
                    .timeline
                    .record_duplex_pointer_phase(duplex_pointer_phase_nanos(
                        playback_status,
                        capture_status,
                    ));
                if latency_guard.sample(
                    write_finished_nanos,
                    playback_status.get_delay(),
                    capture_status.get_delay(),
                ) {
                    pending_rebase = true;
                }
            }
        }
        sequence = sequence.wrapping_add(1);
        periods_processed = periods_processed.wrapping_add(1);
        control.timeline.processed_frames(config.period as u64, 1);
        control.publish_hardware_ready();
    }
}

#[allow(clippy::too_many_arguments)]
fn linked_pro_ahead_cycle_loop<C: ProCaptureSink, P: ProPlaybackSource>(
    playback_pcm: &PCM,
    capture_pcm: &PCM,
    capture_scratch: &mut [i32],
    playback_scratch: &mut [i32],
    config: LinkedProConfig<'_>,
    control: WorkerControl<'_>,
    capture_sink: &mut C,
    playback_source: &mut P,
    max_periods: Option<u64>,
) -> Result<(), EngineError> {
    let mut playback_position = 0_u64;
    let mut capture_position = 0_u64;
    let mut sequence = 0_u64;
    let mut periods_processed = 0_u64;
    let mut pending_capture = None;

    loop {
        if control.should_stop() || max_periods.is_some_and(|limit| periods_processed >= limit) {
            control.done.store(true, Ordering::Release);
            return Ok(());
        }

        prepare_pro_playback(playback_source, sequence);
        let current_capture = take_pending_pro_capture(&mut pending_capture, sequence);
        if let Some(pending) = current_capture {
            wait_for_pro_handoff(pending, control)?;
        }
        let mut playback_commit = PlaybackCommitGuard::begin(playback_source);
        render_pro_playback(
            playback_commit.source(),
            sequence,
            current_capture.map(|pending| pending.target_nanos),
            &mut playback_scratch[..config.playback_period_samples],
        );
        if let Some(pending) = current_capture {
            publish_deferred_capture(capture_sink, pending, capture_scratch);
        }

        if let Err(error) = wait_for_playback_target(
            playback_pcm,
            config.buffer,
            config.playback_floor,
            config.rate,
            config.handoff_nanos,
            control,
        ) {
            if error.is_recoverable() {
                pending_capture = None;
                recover_with_commit_released(&mut playback_commit, || {
                    recover_linked_streams_during_cycle(
                        playback_pcm,
                        capture_pcm,
                        capture_scratch,
                        &error,
                        config,
                        control,
                    )
                })?;
                sequence = sequence.wrapping_add(1);
                continue;
            }
            if error.is_stopped() {
                control.done.store(true, Ordering::Release);
                return Ok(());
            }
            control.done.store(true, Ordering::Release);
            return Err(error);
        }
        control.ensure_running()?;

        if let Ok(status) = playback_pcm.status() {
            let delay = status.get_delay();
            control.timeline.update_pcm_delay(
                StreamDirection::Playback,
                delay,
                config.period as u64,
            );
            control.timeline.update_playback_delay_breakdown(
                delay,
                status.get_avail(),
                config.buffer,
            );
        }
        let write_started_nanos = monotonic_nanos();
        let write_result = write_playback_samples(
            playback_pcm,
            &playback_scratch[..config.playback_period_samples],
            config.playback_channels,
            config.period as usize,
            Some(control),
        );
        let write_finished_nanos = monotonic_nanos();
        control
            .timeline
            .record_playback_write(write_finished_nanos.saturating_sub(write_started_nanos));
        playback_commit.finish();
        match write_result {
            Ok(written) if written == config.period => {
                playback_position = playback_position.wrapping_add(config.period as u64);
                control.timeline.update_playback_position(playback_position);
                control
                    .timeline
                    .record_pro_playback_write(sequence, write_finished_nanos);
            }
            Ok(written) => {
                control.done.store(true, Ordering::Release);
                return Err(EngineError::ShortCommit {
                    direction: StreamDirection::Playback,
                    actual: written,
                    required: config.period,
                });
            }
            Err(error) if error.is_recoverable() => {
                pending_capture = None;
                recover_linked_streams_during_cycle(
                    playback_pcm,
                    capture_pcm,
                    capture_scratch,
                    &error,
                    config,
                    control,
                )?;
                sequence = sequence.wrapping_add(1);
                continue;
            }
            Err(error) if error.is_stopped() => {
                control.done.store(true, Ordering::Release);
                return Ok(());
            }
            Err(error) => {
                control.done.store(true, Ordering::Release);
                return Err(error);
            }
        }

        let read = match read_capture_samples(
            capture_pcm,
            capture_scratch,
            config.capture_channels,
            config.period as usize,
            Some(control),
        ) {
            Ok(read) => read,
            Err(error) if error.is_recoverable() => {
                pending_capture = None;
                recover_linked_streams_during_cycle(
                    playback_pcm,
                    capture_pcm,
                    capture_scratch,
                    &error,
                    config,
                    control,
                )?;
                sequence = sequence.wrapping_add(1);
                continue;
            }
            Err(error) if error.is_stopped() => {
                control.done.store(true, Ordering::Release);
                return Ok(());
            }
            Err(error) => {
                control.done.store(true, Ordering::Release);
                return Err(error);
            }
        };
        if read != config.period {
            control.done.store(true, Ordering::Release);
            return Err(EngineError::ShortCommit {
                direction: StreamDirection::Capture,
                actual: read,
                required: config.period,
            });
        }

        let capture_read_nanos = monotonic_nanos();
        if let Ok(delay) = capture_pcm.delay() {
            control.timeline.update_pcm_delay(
                StreamDirection::Capture,
                delay,
                config.period as u64,
            );
        }
        capture_position = capture_position.wrapping_add(config.period as u64);
        control.timeline.update_capture_position(capture_position);
        let playback_sequence = pro_target_sequence(sequence, config.sequence_lead);
        control
            .timeline
            .record_pro_capture_read(playback_sequence, capture_read_nanos);
        capture_sink.process_capture_for_playback(sequence, playback_sequence, capture_scratch);
        control.mark_capture_cycle_ready();
        pending_capture = Some(PendingProCapture {
            hardware_sequence: sequence,
            playback_sequence,
            target_nanos: monotonic_nanos().saturating_add(config.handoff_nanos),
        });
        sequence = sequence.wrapping_add(1);
        periods_processed = periods_processed.wrapping_add(1);
        control.timeline.processed_frames(config.period as u64, 1);
        control.publish_hardware_ready();
    }
}

fn uses_staged_packet_cycle(
    sequence_lead: u64,
    period: alsa::pcm::Frames,
    hardware_period: alsa::pcm::Frames,
) -> bool {
    sequence_lead != 0 && hardware_period < period
}

#[allow(clippy::too_many_arguments)]
fn linked_pro_packet_cycle_loop<C: ProCaptureSink, P: ProPlaybackSource>(
    playback_pcm: &PCM,
    capture_pcm: &PCM,
    capture_scratch: &mut [i32],
    playback_scratch: &mut [i32],
    config: LinkedProConfig<'_>,
    control: WorkerControl<'_>,
    capture_sink: &mut C,
    playback_source: &mut P,
    max_periods: Option<u64>,
) -> Result<(), EngineError> {
    let mut playback_position = 0_u64;
    let mut capture_position = 0_u64;
    let mut sequence = 0_u64;
    let mut periods_processed = 0_u64;
    let mut pending_capture = None;
    let mut staged_playback_epoch = None;
    let chunks = config.period / config.hardware_period;
    let playback_chunk_samples = usize::try_from(config.hardware_period)
        .ok()
        .and_then(|frames| frames.checked_mul(config.playback_channels))
        .ok_or_else(|| EngineError::InvalidConfig("playback chunk size overflow".into()))?;
    let capture_chunk_samples = usize::try_from(config.hardware_period)
        .ok()
        .and_then(|frames| frames.checked_mul(config.capture_channels))
        .ok_or_else(|| EngineError::InvalidConfig("capture chunk size overflow".into()))?;

    loop {
        if control.stop.load(Ordering::Relaxed)
            || max_periods.is_some_and(|limit| periods_processed >= limit)
        {
            control.done.store(true, Ordering::Release);
            return Ok(());
        }

        match linked_pro_packet_cycle(
            playback_pcm,
            capture_pcm,
            capture_scratch,
            playback_scratch,
            playback_chunk_samples,
            capture_chunk_samples,
            chunks,
            sequence,
            config,
            control,
            capture_sink,
            playback_source,
            &mut playback_position,
            &mut capture_position,
            &mut pending_capture,
            &mut staged_playback_epoch,
        ) {
            Ok(()) => {
                sequence = sequence.wrapping_add(1);
                periods_processed = periods_processed.wrapping_add(1);
                control.timeline.processed_frames(config.period as u64, 1);
                control.publish_hardware_ready();
            }
            Err(error) if error.is_stopped() => {
                control.done.store(true, Ordering::Release);
                return Ok(());
            }
            Err(error) if error.is_recoverable() => {
                playback_scratch[..config.playback_period_samples].fill(0);
                pending_capture = None;
                staged_playback_epoch = None;
                recover_linked_streams_during_cycle(
                    playback_pcm,
                    capture_pcm,
                    capture_scratch,
                    &error,
                    config,
                    control,
                )?;
                sequence = sequence.wrapping_add(1);
            }
            Err(error) => {
                control.done.store(true, Ordering::Release);
                return Err(error);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn linked_pro_packet_cycle<C: ProCaptureSink, P: ProPlaybackSource>(
    playback_pcm: &PCM,
    capture_pcm: &PCM,
    capture_scratch: &mut [i32],
    playback_scratch: &mut [i32],
    playback_chunk_samples: usize,
    capture_chunk_samples: usize,
    chunks: alsa::pcm::Frames,
    sequence: u64,
    config: LinkedProConfig<'_>,
    control: WorkerControl<'_>,
    capture_sink: &mut C,
    playback_source: &mut P,
    playback_position: &mut u64,
    capture_position: &mut u64,
    pending_capture: &mut Option<PendingProCapture>,
    staged_playback_epoch: &mut Option<u64>,
) -> Result<(), EngineError> {
    let mut playback_commit = PlaybackCommitGuard::begin(playback_source);
    let playback_epoch = playback_commit.source_ref().playback_epoch();
    if staged_playback_epoch.is_some_and(|staged| staged != playback_epoch) {
        playback_scratch[..config.playback_period_samples].fill(0);
    }
    prepare_pro_playback(playback_commit.source(), sequence);
    refill_linked_playback_chunk(
        playback_pcm,
        playback_scratch,
        chunks - 1,
        playback_chunk_samples,
        config,
        control,
        playback_position,
    )?;

    let current_capture = take_pending_pro_capture(pending_capture, sequence);
    if let Some(pending) = current_capture {
        wait_for_pro_handoff(pending, control)?;
    }
    render_pro_playback(
        playback_commit.source(),
        sequence,
        current_capture.map(|pending| pending.target_nanos),
        &mut playback_scratch[..config.playback_period_samples],
    );
    if let Some(pending) = current_capture {
        publish_deferred_capture(capture_sink, pending, capture_scratch);
    }

    for chunk in 0..chunks {
        if let Some(playback_chunk) = staged_playback_chunk_before_capture(chunk) {
            if playback_chunk == 0
                && let Ok(status) = playback_pcm.status()
            {
                let delay = status.get_delay();
                control.timeline.update_pcm_delay(
                    StreamDirection::Playback,
                    delay,
                    config.hardware_period as u64,
                );
                control.timeline.update_playback_delay_breakdown(
                    delay,
                    status.get_avail(),
                    config.buffer,
                );
            }
            let write_finished_nanos = refill_linked_playback_chunk(
                playback_pcm,
                playback_scratch,
                playback_chunk,
                playback_chunk_samples,
                config,
                control,
                playback_position,
            )?;
            if playback_chunk == 0 {
                control
                    .timeline
                    .record_pro_playback_write(sequence, write_finished_nanos);
            }
        }

        let capture_offset = usize::try_from(chunk)
            .ok()
            .and_then(|chunk| chunk.checked_mul(capture_chunk_samples))
            .ok_or_else(|| EngineError::InvalidConfig("capture chunk offset overflow".into()))?;
        let read = read_capture_samples(
            capture_pcm,
            &mut capture_scratch
                [capture_offset..capture_offset.saturating_add(capture_chunk_samples)],
            config.capture_channels,
            config.hardware_period as usize,
            Some(control),
        )?;
        if read != config.hardware_period {
            return Err(EngineError::ShortCommit {
                direction: StreamDirection::Capture,
                actual: read,
                required: config.hardware_period,
            });
        }
        *capture_position = capture_position.wrapping_add(config.hardware_period as u64);
        control.timeline.update_capture_position(*capture_position);

        if chunk + 1 == chunks {
            let capture_sequence = pro_target_sequence(sequence, config.sequence_lead);
            let capture_read_nanos = monotonic_nanos();
            if let Ok(delay) = capture_pcm.delay() {
                control.timeline.update_pcm_delay(
                    StreamDirection::Capture,
                    delay,
                    config.period as u64,
                );
            }
            control
                .timeline
                .record_pro_capture_read(capture_sequence, capture_read_nanos);
            capture_sink.process_capture_for_playback(sequence, capture_sequence, capture_scratch);
            control.mark_capture_cycle_ready();
            *pending_capture = Some(PendingProCapture {
                hardware_sequence: sequence,
                playback_sequence: capture_sequence,
                target_nanos: monotonic_nanos().saturating_add(config.handoff_nanos),
            });
        }
        if control.stop.load(Ordering::Relaxed) {
            return Err(EngineError::Stopped);
        }
    }
    *staged_playback_epoch = Some(playback_epoch);
    playback_commit.finish();
    Ok(())
}

fn staged_playback_chunk_before_capture(
    capture_chunk: alsa::pcm::Frames,
) -> Option<alsa::pcm::Frames> {
    (capture_chunk > 0).then(|| capture_chunk - 1)
}

fn refill_linked_playback_chunk(
    playback_pcm: &PCM,
    playback_scratch: &[i32],
    chunk: alsa::pcm::Frames,
    playback_chunk_samples: usize,
    config: LinkedProConfig<'_>,
    control: WorkerControl<'_>,
    playback_position: &mut u64,
) -> Result<u64, EngineError> {
    wait_for_playback_target(
        playback_pcm,
        config.buffer,
        config.playback_floor,
        config.rate,
        config.handoff_nanos,
        control,
    )?;
    if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
        return Err(EngineError::Stopped);
    }
    write_linked_playback_chunk(
        playback_pcm,
        playback_scratch,
        chunk,
        playback_chunk_samples,
        config,
        control,
        playback_position,
    )
}

fn write_linked_playback_chunk(
    playback_pcm: &PCM,
    playback_scratch: &[i32],
    chunk: alsa::pcm::Frames,
    playback_chunk_samples: usize,
    config: LinkedProConfig<'_>,
    control: WorkerControl<'_>,
    playback_position: &mut u64,
) -> Result<u64, EngineError> {
    let playback_offset = usize::try_from(chunk)
        .ok()
        .and_then(|chunk| chunk.checked_mul(playback_chunk_samples))
        .ok_or_else(|| EngineError::InvalidConfig("playback chunk offset overflow".into()))?;
    let write_started_nanos = monotonic_nanos();
    let written = write_playback_samples(
        playback_pcm,
        &playback_scratch[playback_offset..playback_offset.saturating_add(playback_chunk_samples)],
        config.playback_channels,
        config.hardware_period as usize,
        Some(control),
    );
    let write_finished_nanos = monotonic_nanos();
    control
        .timeline
        .record_playback_write(write_finished_nanos.saturating_sub(write_started_nanos));
    match written {
        Ok(written) if written == config.hardware_period => {
            *playback_position = playback_position.wrapping_add(config.hardware_period as u64);
            control
                .timeline
                .update_playback_position(*playback_position);
            Ok(write_finished_nanos)
        }
        Ok(written) => Err(EngineError::ShortCommit {
            direction: StreamDirection::Playback,
            actual: written,
            required: config.hardware_period,
        }),
        Err(error) => Err(error),
    }
}

fn playback_worker<P: ProPlaybackSource>(
    pcm: PCM,
    config: PlaybackWorkerConfig<'_>,
    control: WorkerControl<'_>,
    pro_clock: Option<&ProClock>,
    output_scratch: Option<&mut [i32]>,
    playback_source: Option<&mut P>,
    max_periods: Option<u64>,
) -> (PCM, Result<(), EngineError>) {
    let startup_priority =
        playback_startup_priority(config.realtime_priority, control.start_gate.is_some());
    let _scheduling = match startup_priority {
        Some(priority) => match RealtimeGuard::enter(priority, "set worker SCHED_FIFO") {
            Ok(guard) => Some(guard),
            Err(error) => {
                control.done.store(true, Ordering::Release);
                return (pcm, Err(error));
            }
        },
        None => None,
    };
    if let Some(gate) = control.start_gate {
        gate.playback_ready.store(true, Ordering::Release);
        while !gate.capture_ready.load(Ordering::Acquire) {
            if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
                control.done.store(true, Ordering::Release);
                unlink_unstarted_playback(&pcm, gate);
                return (pcm, Ok(()));
            }
            std::thread::yield_now();
        }
        while !gate.linked && !gate.capture_started.load(Ordering::Acquire) {
            if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
                control.done.store(true, Ordering::Release);
                unlink_unstarted_playback(&pcm, gate);
                return (pcm, Ok(()));
            }
            std::thread::yield_now();
        }
        if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
            control.done.store(true, Ordering::Release);
            unlink_unstarted_playback(&pcm, gate);
            return (pcm, Ok(()));
        }
        if config.realtime_priority != startup_priority
            && let Some(priority) = config.realtime_priority
            && let Err(error) = set_current_realtime(priority, "raise playback SCHED_FIFO")
        {
            control.done.store(true, Ordering::Release);
            unlink_unstarted_playback(&pcm, gate);
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
    let result = run_catching_worker_panic(control, || {
        playback_worker_loop(
            &pcm,
            config,
            control,
            pro_clock,
            output_scratch,
            playback_source,
            max_periods,
        )
    });
    (pcm, result)
}

// Workers that exit before starting must not leave the PCMs linked: `stop()` does not know
// about this link, and the next `link()` would fail with -EALREADY.
fn unlink_unstarted_playback(pcm: &PCM, gate: &DuplexStartGate) {
    if gate.linked {
        let _ = pcm.unlink();
    }
}

fn playback_startup_priority(priority: Option<i32>, deferred_start: bool) -> Option<i32> {
    if deferred_start {
        priority.map(|priority| (priority - 1).max(1))
    } else {
        priority
    }
}

fn playback_worker_loop<P: ProPlaybackSource>(
    pcm: &PCM,
    config: PlaybackWorkerConfig<'_>,
    control: WorkerControl<'_>,
    pro_clock: Option<&ProClock>,
    mut output_scratch: Option<&mut [i32]>,
    mut playback_source: Option<&mut P>,
    max_periods: Option<u64>,
) -> Result<(), EngineError> {
    let mut position = 0_u64;
    let mut sequence = 0_u64;
    let mut successful_periods = 0_u64;
    let mut announced_output = None;
    let mut prepared_output = None;

    loop {
        if control.stop.load(Ordering::Relaxed)
            || control.done.load(Ordering::Acquire)
            || period_limit_reached(successful_periods, max_periods)
        {
            control.done.store(true, Ordering::Release);
            return Ok(());
        }

        if !announced_output.is_some_and(|(announced, _, _)| announced == sequence) {
            let hardware_generation = control.timeline.generation();
            let playback_epoch = playback_source_epoch(playback_source.as_deref());
            if let Some(source) = playback_source.as_deref_mut() {
                source.prepare_playback(sequence);
            }
            announced_output = Some((sequence, hardware_generation, playback_epoch));
        }

        let wait_result = if config.timer_scheduling {
            wait_for_playback_target(
                pcm,
                config.buffer,
                config.guard_frames,
                config.rate,
                50_000,
                control,
            )
        } else {
            wait_for_transfer(pcm, StreamDirection::Playback, Some(control))
        };
        if let Err(error) = wait_result {
            if error.is_stopped() {
                control.done.store(true, Ordering::Release);
                return Ok(());
            }
            if error.is_recoverable() {
                match recover_stream(
                    pcm,
                    StreamDirection::Playback,
                    &error,
                    control,
                    Some((config.silence, config.channels, config.start_frames)),
                ) {
                    Ok(()) => {
                        if let Some(clock) = pro_clock
                            && realign_playback_after_recovery(
                                &mut sequence,
                                clock,
                                config.sequence_lead,
                            )
                        {
                            announced_output = None;
                            prepared_output = None;
                        }
                        continue;
                    }
                    Err(error) if error.is_stopped() => {
                        control.done.store(true, Ordering::Release);
                        return Ok(());
                    }
                    Err(error) => {
                        control.done.store(true, Ordering::Release);
                        return Err(error);
                    }
                }
            }
            control.done.store(true, Ordering::Release);
            return Err(error);
        }
        if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
            continue;
        }
        let mut playback_commit = playback_source
            .as_deref_mut()
            .map(PlaybackCommitGuard::begin);
        let hardware_generation = control.timeline.generation();
        let playback_epoch = playback_source_epoch(
            playback_commit
                .as_ref()
                .map(PlaybackCommitGuard::source_ref),
        );
        if !prepared_output_matches(
            prepared_output,
            sequence,
            hardware_generation,
            playback_epoch,
        ) {
            if announced_output_matches(
                announced_output,
                sequence,
                hardware_generation,
                playback_epoch,
            ) && let (Some(source), Some(scratch)) = (
                playback_commit.as_mut().map(PlaybackCommitGuard::source),
                output_scratch.as_deref_mut(),
            ) {
                let scratch = &mut scratch[..config.period_samples];
                source.process_playback(sequence, scratch);
                source.commit_playback(sequence, scratch);
            } else if let Some(scratch) = output_scratch.as_deref_mut() {
                scratch[..config.period_samples].fill(0);
            }
            if control.timeline.generation() != hardware_generation
                || playback_source_epoch(
                    playback_commit
                        .as_ref()
                        .map(PlaybackCommitGuard::source_ref),
                ) != playback_epoch
            {
                prepared_output = None;
                continue;
            }
            prepared_output = Some((sequence, hardware_generation, playback_epoch));
        }
        if let Ok(status) = pcm.status() {
            let delay = status.get_delay();
            control.timeline.update_pcm_delay(
                StreamDirection::Playback,
                delay,
                config.period as u64,
            );
            control.timeline.update_playback_delay_breakdown(
                delay,
                status.get_avail(),
                config.buffer,
            );
        }

        let write_generation = control.timeline.generation();
        let write_epoch = playback_source_epoch(
            playback_commit
                .as_ref()
                .map(PlaybackCommitGuard::source_ref),
        );
        if write_generation != hardware_generation {
            prepared_output = None;
            continue;
        }
        if !prepared_output_matches(prepared_output, sequence, write_generation, write_epoch) {
            if let Some(scratch) = output_scratch.as_deref_mut() {
                scratch[..config.period_samples].fill(0);
            }
            prepared_output = Some((sequence, write_generation, write_epoch));
        }

        let write_started_nanos = monotonic_nanos();
        control
            .timeline
            .record_pro_playback_write(sequence, write_started_nanos);
        let samples = output_scratch
            .as_deref()
            .map_or(config.silence, |scratch| &scratch[..config.period_samples]);
        let write_result = write_playback_samples(
            pcm,
            samples,
            config.channels,
            config.period as usize,
            Some(control),
        );
        let write_finished_nanos = monotonic_nanos();
        control
            .timeline
            .record_playback_write(write_finished_nanos.saturating_sub(write_started_nanos));
        if let Some(commit) = &mut playback_commit {
            commit.finish();
        }
        match write_result {
            Ok(written) if written == config.period => {
                position = position.wrapping_add(config.period as u64);
                sequence = sequence.wrapping_add(1);
                successful_periods = successful_periods.saturating_add(1);
                announced_output = None;
                prepared_output = None;
                if let Some(clock) = pro_clock {
                    clock.publish(pro_target_sequence(sequence, config.sequence_lead));
                }
                control.timeline.update_playback_position(position);
                control.timeline.processed_frames(config.period as u64, 1);
                control.publish_hardware_ready();
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
            Err(error) if error.is_recoverable() => {
                match recover_stream(
                    pcm,
                    StreamDirection::Playback,
                    &error,
                    control,
                    Some((config.silence, config.channels, config.start_frames)),
                ) {
                    Ok(()) => {
                        if let Some(clock) = pro_clock
                            && realign_playback_after_recovery(
                                &mut sequence,
                                clock,
                                config.sequence_lead,
                            )
                        {
                            announced_output = None;
                            prepared_output = None;
                        }
                    }
                    Err(error) if error.is_stopped() => {
                        control.done.store(true, Ordering::Release);
                        return Ok(());
                    }
                    Err(error) => {
                        control.done.store(true, Ordering::Release);
                        return Err(error);
                    }
                }
            }
            Err(error) if error.is_stopped() => {
                control.done.store(true, Ordering::Release);
                return Ok(());
            }
            Err(error) => {
                control.done.store(true, Ordering::Release);
                return Err(error);
            }
        }
    }
}

#[inline]
fn playback_source_epoch<P: ProPlaybackSource + ?Sized>(source: Option<&P>) -> u64 {
    source.map_or(0, ProPlaybackSource::playback_epoch)
}

fn prepared_output_matches(
    prepared: Option<(u64, u64, u64)>,
    sequence: u64,
    generation: u64,
    playback_epoch: u64,
) -> bool {
    prepared == Some((sequence, generation, playback_epoch))
}

fn announced_output_matches(
    announced: Option<(u64, u64, u64)>,
    sequence: u64,
    generation: u64,
    playback_epoch: u64,
) -> bool {
    announced == Some((sequence, generation, playback_epoch))
}

fn period_limit_reached(successful_periods: u64, max_periods: Option<u64>) -> bool {
    max_periods.is_some_and(|limit| successful_periods >= limit)
}

fn capture_worker<C: ProCaptureSink>(
    pcm: PCM,
    scratch: &mut [i32],
    config: CaptureWorkerConfig,
    control: WorkerControl<'_>,
    pro_clock: Option<&ProClock>,
    capture_sink: Option<&mut C>,
) -> (PCM, Result<(), EngineError>) {
    let _scheduling = match config.realtime_priority {
        Some(priority) => match RealtimeGuard::enter(priority, "set worker SCHED_FIFO") {
            Ok(guard) => Some(guard),
            Err(error) => {
                control.done.store(true, Ordering::Release);
                return (pcm, Err(error));
            }
        },
        None => None,
    };
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
    let result = run_catching_worker_panic(control, || {
        capture_worker_loop(&pcm, scratch, config, control, pro_clock, capture_sink)
    });
    (pcm, result)
}

fn capture_worker_loop<C: ProCaptureSink>(
    pcm: &PCM,
    scratch: &mut [i32],
    config: CaptureWorkerConfig,
    control: WorkerControl<'_>,
    pro_clock: Option<&ProClock>,
    mut capture_sink: Option<&mut C>,
) -> Result<(), EngineError> {
    let mut position = 0_u64;
    let mut hardware_sequence = 0_u64;
    let mut next_pro_sequence = config.initial_pro_sequence;
    loop {
        if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
            control.done.store(true, Ordering::Release);
            return Ok(());
        }

        match read_capture_samples(
            pcm,
            scratch,
            config.channels,
            config.period as usize,
            Some(control),
        ) {
            Ok(read) if read == config.period => {
                let capture_read_nanos = monotonic_nanos();
                if let Ok(delay) = pcm.delay() {
                    control.timeline.update_pcm_delay(
                        StreamDirection::Capture,
                        delay,
                        config.period as u64,
                    );
                }
                if control.should_stop() {
                    control.done.store(true, Ordering::Release);
                    return Ok(());
                }
                if let (Some(clock), Some(sink)) = (pro_clock, capture_sink.as_deref_mut()) {
                    let sequence = take_pro_capture_sequence(&mut next_pro_sequence, clock.load());
                    clock.publish_capture_next(next_pro_sequence);
                    control
                        .timeline
                        .record_pro_capture_read(sequence, capture_read_nanos);
                    sink.process_capture_for_playback(hardware_sequence, sequence, scratch);
                    sink.process_deferred_capture(hardware_sequence, scratch);
                }
                hardware_sequence = hardware_sequence.wrapping_add(1);
                position = position.wrapping_add(config.period as u64);
                control.timeline.update_capture_position(position);
                control.mark_capture_cycle_ready();
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
            Err(error) if error.is_recoverable() => {
                match recover_stream(pcm, StreamDirection::Capture, &error, control, None) {
                    Ok(()) => {}
                    Err(error) if error.is_stopped() => {
                        control.done.store(true, Ordering::Release);
                        return Ok(());
                    }
                    Err(error) => {
                        control.done.store(true, Ordering::Release);
                        return Err(error);
                    }
                }
            }
            Err(error) if error.is_stopped() => {
                control.done.store(true, Ordering::Release);
                return Ok(());
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

fn take_pro_capture_sequence(next: &mut u64, rebase_target: u64) -> u64 {
    align_sequence_forward(next, rebase_target);
    let sequence = *next;
    *next = next.wrapping_add(1);
    sequence
}

// Capture keeps running while playback recovers, so playback must skip the periods it lost;
// otherwise every playback XRUN permanently adds that many periods of round-trip latency.
// One period of slack absorbs normal capture/playback thread phase jitter.
fn realign_playback_after_recovery(sequence: &mut u64, clock: &ProClock, lead: u64) -> bool {
    let target = clock.capture_next().wrapping_sub(lead).wrapping_sub(1);
    if !sequence_before(*sequence, target) {
        return false;
    }
    *sequence = target;
    clock.publish(pro_target_sequence(target, lead));
    true
}

fn align_sequence_forward(next: &mut u64, target: u64) {
    if sequence_before(*next, target) {
        *next = target;
    }
}

fn sequence_before(sequence: u64, target: u64) -> bool {
    sequence != target && target.wrapping_sub(sequence) < (1_u64 << 63)
}

fn read_capture_samples(
    pcm: &PCM,
    scratch: &mut [i32],
    channels: usize,
    frames: usize,
    control: Option<WorkerControl<'_>>,
) -> Result<alsa::pcm::Frames, EngineError> {
    read_capture_samples_with_initial(pcm, scratch, channels, frames, None, control)
}

fn read_capture_samples_with_initial(
    pcm: &PCM,
    scratch: &mut [i32],
    channels: usize,
    frames: usize,
    mut initial_available: Option<alsa::pcm::Frames>,
    control: Option<WorkerControl<'_>>,
) -> Result<alsa::pcm::Frames, EngineError> {
    let io = unsafe { pcm.io_unchecked::<i32>() };
    let total_samples = frames * channels;
    let scratch = &mut scratch[..total_samples];
    let mut remaining_frames = frames;
    let required = frames as i64;
    let mut offset_samples = 0;

    while remaining_frames > 0 {
        let available = match initial_available.take() {
            Some(available) if available > 0 => available,
            _ => alsa_call(
                pcm.avail_update(),
                "update capture availability",
                StreamDirection::Capture,
            )?,
        };
        if available <= 0 {
            wait_for_transfer(pcm, StreamDirection::Capture, control)?;
            continue;
        }
        let available_frames = available as usize;
        let requested_frames = remaining_frames.min(available_frames);
        let requested_samples = requested_frames * channels;
        let mut mapped_frames = 0;
        let mut mapped_samples = 0;
        let processed = alsa_call(
            io.mmap(requested_frames, |mapped| {
                let (chunk_frames, chunk_samples) = if mapped.len() == requested_samples {
                    (requested_frames, requested_samples)
                } else {
                    let chunk_frames = mapped.len() / channels;
                    (chunk_frames, chunk_frames * channels)
                };
                mapped_frames = chunk_frames;
                mapped_samples = chunk_samples;
                scratch[offset_samples..offset_samples + chunk_samples]
                    .copy_from_slice(&mapped[..chunk_samples]);
                chunk_frames
            }),
            "read capture period",
            StreamDirection::Capture,
        )?;
        if processed != mapped_frames {
            return Err(EngineError::ShortCommit {
                direction: StreamDirection::Capture,
                actual: processed as i64,
                required: mapped_frames as i64,
            });
        }
        if processed == 0 {
            wait_for_transfer(pcm, StreamDirection::Capture, control)?;
            continue;
        }
        offset_samples += mapped_samples;
        remaining_frames -= processed;
        if remaining_frames > 0 {
            let leftover = available_frames - processed;
            if leftover > 0 {
                initial_available = Some(leftover as alsa::pcm::Frames);
            }
        }
    }
    Ok(required)
}

fn write_playback_samples(
    pcm: &PCM,
    samples: &[i32],
    channels: usize,
    frames: usize,
    control: Option<WorkerControl<'_>>,
) -> Result<alsa::pcm::Frames, EngineError> {
    let io = unsafe { pcm.io_unchecked::<i32>() };
    let total_samples = frames * channels;
    let samples = &samples[..total_samples];
    let mut remaining_frames = frames;
    let required = frames as i64;
    let mut offset_samples = 0;
    let mut known_available: Option<alsa::pcm::Frames> = None;

    while remaining_frames > 0 {
        let available = match known_available.take() {
            Some(available) if available > 0 => available,
            _ => alsa_call(
                pcm.avail_update(),
                "update playback availability",
                StreamDirection::Playback,
            )?,
        };
        if available <= 0 {
            wait_for_transfer(pcm, StreamDirection::Playback, control)?;
            continue;
        }
        let available_frames = available as usize;
        let requested_frames = remaining_frames.min(available_frames);
        let requested_samples = requested_frames * channels;
        let mut mapped_frames = 0;
        let mut mapped_samples = 0;
        let processed = alsa_call(
            io.mmap(requested_frames, |mapped| {
                let (chunk_frames, chunk_samples) = if mapped.len() == requested_samples {
                    (requested_frames, requested_samples)
                } else {
                    let chunk_frames = mapped.len() / channels;
                    (chunk_frames, chunk_frames * channels)
                };
                mapped_frames = chunk_frames;
                mapped_samples = chunk_samples;
                mapped[..chunk_samples]
                    .copy_from_slice(&samples[offset_samples..offset_samples + chunk_samples]);
                chunk_frames
            }),
            "write playback period",
            StreamDirection::Playback,
        )?;
        if processed != mapped_frames {
            return Err(EngineError::ShortCommit {
                direction: StreamDirection::Playback,
                actual: processed as i64,
                required: mapped_frames as i64,
            });
        }
        if processed == 0 {
            wait_for_transfer(pcm, StreamDirection::Playback, control)?;
            continue;
        }
        offset_samples += mapped_samples;
        remaining_frames -= processed;
        if remaining_frames > 0 {
            let leftover = available_frames - processed;
            if leftover > 0 {
                known_available = Some(leftover as alsa::pcm::Frames);
            }
        }
    }
    Ok(required)
}

fn wait_for_linked_duplex_period(
    playback_pcm: &PCM,
    capture_pcm: &PCM,
    poll_cache: &mut LinkedPollCache,
    period: alsa::pcm::Frames,
    buffer: alsa::pcm::Frames,
    playback_lead_frames: alsa::pcm::Frames,
    control: WorkerControl<'_>,
) -> Result<(DirectDuplexReadiness, alsa::pcm::Frames), EngineError> {
    const POLL_TIMEOUT_MS: i32 = 100;

    let mut playback_ready = false;
    let mut capture_ready = false;
    // Check mmap availability before polling so an already-writable playback ring
    // does not force an immediate dummy poll() wakeup before sleeping on capture.
    let mut polled = false;
    loop {
        control.ensure_running()?;
        if polled {
            let active = poll_cache.active_fds_mut(playback_ready, capture_ready);
            match poll::poll(active, POLL_TIMEOUT_MS) {
                Err(error) if error.errno() == libc::EINTR => continue,
                result => {
                    alsa_call(
                        result,
                        "poll linked duplex streams",
                        StreamDirection::Capture,
                    )?;
                }
            }
        }
        polled = true;

        let capture_result = capture_pcm.avail_update();
        // Query latency and preemption must consume, not extend, the queue budget;
        // skip the timestamp read when capture cannot satisfy this cycle yet.
        let observed_nanos = if matches!(&capture_result, Ok(avail) if *avail >= period) {
            monotonic_nanos()
        } else {
            0
        };
        let playback_result = playback_pcm.avail_update();
        // avail_update() already syncs hw_ptr and returns -EPIPE on XRUN; only
        // query snd_pcm_state() when an error or full-buffer boundary is seen.
        let check_states = !matches!(playback_result, Ok(avail) if avail < buffer)
            || !matches!(capture_result, Ok(avail) if avail < buffer);
        let playback_xrun = matches!(&playback_result, Err(error) if error.errno() == libc::EPIPE)
            || (check_states && playback_pcm.state() == State::XRun);
        let capture_xrun = matches!(&capture_result, Err(error) if error.errno() == libc::EPIPE)
            || (check_states && capture_pcm.state() == State::XRun);
        let (playback_available, capture_available) = if playback_xrun || capture_xrun {
            let source = playback_result
                .err()
                .or_else(|| capture_result.err())
                .unwrap_or_else(|| alsa::Error::new("snd_pcm_avail_update", libc::EPIPE));
            return Err(EngineError::LinkedXrun {
                operation: "update linked duplex availability",
                playback: playback_xrun,
                capture: capture_xrun,
                source,
            });
        } else {
            (
                alsa_call(
                    playback_result,
                    "update linked playback availability",
                    StreamDirection::Playback,
                )?,
                alsa_call(
                    capture_result,
                    "update linked capture availability",
                    StreamDirection::Capture,
                )?,
            )
        };
        playback_ready = playback_available >= period;
        capture_ready = capture_available >= period;
        if playback_ready && capture_ready {
            return Ok((
                DirectDuplexReadiness {
                    playback_queued_frames: effective_direct_playback_queued_frames(
                        buffer,
                        playback_available,
                        playback_lead_frames,
                        capture_available,
                    ),
                    observed_nanos,
                },
                capture_available,
            ));
        }
    }
}

#[inline]
fn effective_direct_playback_queued_frames(
    buffer: alsa::pcm::Frames,
    playback_available: alsa::pcm::Frames,
    playback_lead_frames: alsa::pcm::Frames,
    capture_available: alsa::pcm::Frames,
) -> alsa::pcm::Frames {
    // On USB UAC2 interfaces, playback_pcm.avail_update() can lag behind the
    // real hardware clock until an outbound URB retires, while capture_available
    // already reflects URB wakeup overshoot past the period boundary. Since
    // linked duplex keeps appl_ptr(playback) - appl_ptr(capture) == lead,
    // remaining playback frames cannot exceed lead - capture_available.
    let playback_queued = buffer.saturating_sub(playback_available).max(0);
    let linked_queued = playback_lead_frames
        .saturating_sub(capture_available)
        .max(0);
    playback_queued.min(linked_queued)
}

fn wait_for_transfer(
    pcm: &PCM,
    direction: StreamDirection,
    control: Option<WorkerControl<'_>>,
) -> Result<(), EngineError> {
    // 50 ms avoids spurious 1 ms timer wakeups on Q64+ periods (1.33 ms+) while
    // remaining twice as responsive to shutdown as linked duplex polling (100 ms).
    const TRANSFER_WAIT_TIMEOUT_MS: u32 = 50;
    loop {
        if control.is_some_and(|control| {
            control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire)
        }) {
            return Err(EngineError::Stopped);
        }
        if alsa_call(
            pcm.wait(Some(TRANSFER_WAIT_TIMEOUT_MS)),
            "wait for transfer",
            direction,
        )? {
            return Ok(());
        }
    }
}

fn wait_for_playback_target(
    pcm: &PCM,
    buffer: alsa::pcm::Frames,
    guard_frames: alsa::pcm::Frames,
    rate: u32,
    prewake_nanos: u64,
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
            control.timeline.record_playback_target_overshoot(
                u64::try_from(available - target_avail).unwrap_or(0),
            );
            return Ok(());
        }
        let remaining = target_avail - available;
        let sleep = playback_target_sleep(remaining, rate, prewake_nanos);
        std::thread::sleep(sleep);
    }
}

fn wait_for_handoff_target(
    target_nanos: u64,
    control: WorkerControl<'_>,
) -> Result<(), EngineError> {
    loop {
        if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
            return Err(EngineError::Stopped);
        }
        let remaining = target_nanos.saturating_sub(monotonic_nanos());
        if remaining == 0 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_nanos(remaining));
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

fn pcm_status_with_audio_timestamp(pcm: &PCM) -> alsa::Result<Status> {
    StatusBuilder::new()
        .audio_htstamp_config(AudioTstampType::Default, false)
        .build(pcm)
}

fn pre_write_playback_status(
    post_write_delay: i64,
    post_write_avail: i64,
    write_frames: i64,
    buffer: i64,
) -> (i64, i64) {
    (
        post_write_delay.saturating_sub(write_frames).max(0),
        post_write_avail.saturating_add(write_frames).min(buffer),
    )
}

fn duplex_pointer_phase_nanos(playback: &Status, capture: &Status) -> i64 {
    duplex_pointer_phase_from_timestamps(
        playback.get_htstamp(),
        playback.get_audio_htstamp(),
        capture.get_htstamp(),
        capture.get_audio_htstamp(),
    )
}

fn duplex_pointer_phase_from_timestamps(
    playback_system: libc::timespec,
    playback_audio: libc::timespec,
    capture_system: libc::timespec,
    capture_audio: libc::timespec,
) -> i64 {
    let playback_offset = timespec_nanos(playback_audio) - timespec_nanos(playback_system);
    let capture_offset = timespec_nanos(capture_audio) - timespec_nanos(capture_system);
    (playback_offset - capture_offset).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn timespec_nanos(timestamp: libc::timespec) -> i128 {
    i128::from(timestamp.tv_sec)
        .saturating_mul(1_000_000_000)
        .saturating_add(i128::from(timestamp.tv_nsec))
}

fn playback_target_sleep(remaining: alsa::pcm::Frames, rate: u32, prewake_nanos: u64) -> Duration {
    let frames = u64::try_from(remaining).unwrap_or(0);
    let nanos = frames.saturating_mul(1_000_000_000) / u64::from(rate);
    Duration::from_nanos(nanos.saturating_sub(prewake_nanos).max(1_000))
}

fn direct_linked_start_frames(
    period: alsa::pcm::Frames,
    buffer: alsa::pcm::Frames,
    queue_periods: Option<u32>,
) -> alsa::pcm::Frames {
    buffer.min(period.saturating_mul(i64::from(
        queue_periods.unwrap_or(DIRECT_MIN_PLAYBACK_QUEUE_PERIODS),
    )))
}

#[cfg_attr(not(test), allow(dead_code))]
fn direct_pro_cutoff_nanos(
    readiness: DirectDuplexReadiness,
    period: alsa::pcm::Frames,
    rate: u32,
    handoff_nanos: u64,
) -> u64 {
    // Keep the Q64/48k write margin (~333 us), but do not reserve a quarter
    // of a large DAW block: Q256 would otherwise lose 1.33 ms of DSP time.
    // Small periods retain their existing proportional reserve.
    let reserve_frames = i64::from(direct_pro_write_reserve_frames(
        u32::try_from(period).unwrap_or(u32::MAX),
        rate,
    ));
    direct_pro_cutoff_nanos_with_reserve(readiness, reserve_frames, rate, handoff_nanos)
}

#[inline]
fn direct_pro_cutoff_nanos_with_reserve(
    readiness: DirectDuplexReadiness,
    reserve_frames: alsa::pcm::Frames,
    rate: u32,
    handoff_nanos: u64,
) -> u64 {
    let wait_frames = readiness
        .playback_queued_frames
        .saturating_sub(reserve_frames);
    if wait_frames <= 0 {
        return readiness.observed_nanos;
    }
    let queue_budget_nanos = (wait_frames as u64).saturating_mul(1_000_000_000) / u64::from(rate);
    readiness
        .observed_nanos
        .saturating_add(handoff_nanos.min(queue_budget_nanos))
}

fn linked_start_frames(
    period: alsa::pcm::Frames,
    guard_frames: alsa::pcm::Frames,
    buffer: alsa::pcm::Frames,
    rate: u32,
    handoff_nanos: u64,
) -> alsa::pcm::Frames {
    let handoff_frames = handoff_nanos
        .saturating_mul(u64::from(rate))
        .div_ceil(1_000_000_000);
    let handoff_reserve =
        alsa::pcm::Frames::try_from(handoff_frames).unwrap_or(alsa::pcm::Frames::MAX);
    buffer.min(
        period
            .saturating_add(guard_frames)
            .saturating_add(handoff_reserve),
    )
}

fn linked_zero_lead_playback_floor(
    guard_frames: alsa::pcm::Frames,
    hardware_period: alsa::pcm::Frames,
    buffer: alsa::pcm::Frames,
    write_frames: alsa::pcm::Frames,
) -> alsa::pcm::Frames {
    // Reserve one period for ALSA availability granularity, one while the USB
    // driver advances its next transfer, and one for delayed RT wakeup.
    buffer
        .saturating_sub(write_frames)
        .min(guard_frames.saturating_add(hardware_period.saturating_mul(3)))
}

fn linked_ahead_start_frames(
    period: alsa::pcm::Frames,
    hardware_period: alsa::pcm::Frames,
    buffer: alsa::pcm::Frames,
) -> alsa::pcm::Frames {
    buffer.min(period.saturating_add(hardware_period))
}

fn rebase_linked_streams(
    playback_pcm: &PCM,
    capture_pcm: &PCM,
    control: WorkerControl<'_>,
    config: LinkedProConfig<'_>,
    dither_frames: alsa::pcm::Frames,
) -> Result<(), EngineError> {
    control.ensure_running()?;
    alsa_call(
        playback_pcm.unlink(),
        "unlink duplex streams for phase rebase",
        StreamDirection::Playback,
    )?;
    alsa_call(
        capture_pcm.drop(),
        "stop capture stream for phase rebase",
        StreamDirection::Capture,
    )?;
    alsa_call(
        playback_pcm.drop(),
        "stop playback stream for phase rebase",
        StreamDirection::Playback,
    )?;
    if config.event_driven {
        alsa_call(
            playback_pcm.link(capture_pcm),
            "relink direct streams for phase rebase",
            StreamDirection::Playback,
        )?;
        alsa_call(
            playback_pcm.prepare(),
            "prepare direct streams for phase rebase",
            StreamDirection::Playback,
        )?;
    } else {
        alsa_call(
            capture_pcm.prepare(),
            "prepare capture stream for phase rebase",
            StreamDirection::Capture,
        )?;
        alsa_call(
            playback_pcm.prepare(),
            "prepare playback stream for phase rebase",
            StreamDirection::Playback,
        )?;
    }

    control.ensure_running()?;
    let written = write_playback_samples(
        playback_pcm,
        config.playback_silence,
        config.playback_channels,
        config.start_frames as usize,
        Some(control),
    )?;
    if written != config.start_frames {
        return Err(EngineError::ShortCommit {
            direction: StreamDirection::Playback,
            actual: written,
            required: config.start_frames,
        });
    }
    control.ensure_running()?;
    if !config.event_driven {
        alsa_call(
            playback_pcm.link(capture_pcm),
            "relink duplex streams for phase rebase",
            StreamDirection::Playback,
        )?;
    }
    sleep_for_frames(dither_frames, config.rate);
    control.ensure_running()?;
    alsa_call(
        playback_pcm.start(),
        "restart linked duplex streams after phase rebase",
        StreamDirection::Playback,
    )?;
    control.timeline.reset_after_hardware_rebase();
    Ok(())
}

fn sleep_for_frames(frames: alsa::pcm::Frames, rate: u32) {
    if frames > 0 {
        let nanos = u64::try_from(frames)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            / u64::from(rate);
        std::thread::sleep(Duration::from_nanos(nanos));
    }
}

fn recover_linked_streams(
    playback_pcm: &PCM,
    capture_pcm: &PCM,
    error: &EngineError,
    control: WorkerControl<'_>,
    config: LinkedProConfig<'_>,
    dither_frames: alsa::pcm::Frames,
) -> Result<alsa::pcm::Frames, EngineError> {
    record_linked_hardware_xruns(error, control);
    control.ensure_running()?;
    alsa_call(
        playback_pcm.unlink(),
        "unlink duplex streams after stream failure",
        StreamDirection::Playback,
    )?;
    alsa_call(
        capture_pcm.drop(),
        "stop capture stream after stream failure",
        StreamDirection::Capture,
    )?;
    alsa_call(
        playback_pcm.drop(),
        "stop playback stream after stream failure",
        StreamDirection::Playback,
    )?;
    if config.event_driven {
        alsa_call(
            playback_pcm.link(capture_pcm),
            "relink direct duplex streams after stream failure",
            StreamDirection::Playback,
        )?;
        alsa_call(
            playback_pcm.prepare(),
            "prepare direct linked streams after stream failure",
            StreamDirection::Playback,
        )?;
    } else {
        alsa_call(
            capture_pcm.prepare(),
            "prepare capture stream after stream failure",
            StreamDirection::Capture,
        )?;
        alsa_call(
            playback_pcm.prepare(),
            "prepare playback stream after stream failure",
            StreamDirection::Playback,
        )?;
    }

    control.ensure_running()?;
    let written = write_playback_samples(
        playback_pcm,
        config.playback_silence,
        config.playback_channels,
        config.start_frames as usize,
        Some(control),
    )?;
    if written != config.start_frames {
        return Err(EngineError::ShortCommit {
            direction: StreamDirection::Playback,
            actual: written,
            required: config.start_frames,
        });
    }
    control.ensure_running()?;
    if !config.event_driven {
        alsa_call(
            playback_pcm.link(capture_pcm),
            "relink duplex streams after stream failure",
            StreamDirection::Playback,
        )?;
    }
    sleep_for_frames(dither_frames, config.rate);
    control.ensure_running()?;
    alsa_call(
        playback_pcm.start(),
        "restart linked duplex streams after stream failure",
        StreamDirection::Playback,
    )?;
    let padding = normalize_startup_loopback(playback_pcm, capture_pcm, config, control)?;
    control.timeline.reset_after_hardware_restart();
    Ok(config.start_frames.saturating_add(padding))
}

fn recover_linked_streams_during_cycle(
    playback_pcm: &PCM,
    capture_pcm: &PCM,
    capture_scratch: &mut [i32],
    error: &EngineError,
    config: LinkedProConfig<'_>,
    control: WorkerControl<'_>,
) -> Result<alsa::pcm::Frames, EngineError> {
    if control
        .hardware_ready
        .is_some_and(|ready| !ready.load(Ordering::Acquire))
    {
        // Never publish a replacement start that skipped startup qualification.
        record_linked_hardware_xruns(error, control);
        return Err(EngineError::LinkedStartupInterrupted);
    }
    let playback_lead =
        recover_linked_streams(playback_pcm, capture_pcm, error, control, config, 0)?;
    if !config.event_driven {
        recalibrate_linked_phase(playback_pcm, capture_pcm, capture_scratch, config, control)?;
    }
    Ok(playback_lead)
}

fn record_linked_hardware_xruns(error: &EngineError, control: WorkerControl<'_>) {
    match error {
        EngineError::LinkedXrun {
            playback, capture, ..
        } => {
            if *playback {
                control
                    .timeline
                    .record_hardware_xrun(StreamDirection::Playback);
            }
            if *capture {
                control
                    .timeline
                    .record_hardware_xrun(StreamDirection::Capture);
            }
        }
        // The linked peer stops too, but only the failing transfer's direction overran.
        EngineError::Xrun { direction, .. } => {
            control.timeline.record_hardware_xrun(*direction);
        }
        _ => {}
    }
}

// snd_pcm_recover() sleeps a whole second per -EAGAIN resume attempt and ignores the stop
// flag; poll resume briefly instead and fall back to prepare like alsa-lib does.
fn resume_suspended_stream(
    pcm: &PCM,
    direction: StreamDirection,
    control: WorkerControl<'_>,
) -> Result<(), EngineError> {
    for _ in 0..200 {
        match pcm.resume() {
            Ok(()) => return Ok(()),
            Err(error) if error.errno() == libc::EAGAIN => {
                control.ensure_running()?;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    control.ensure_running()?;
    alsa_call(pcm.prepare(), "prepare suspended stream", direction)
}

fn recover_stream(
    pcm: &PCM,
    direction: StreamDirection,
    error: &EngineError,
    control: WorkerControl<'_>,
    playback: Option<(&[i32], usize, alsa::pcm::Frames)>,
) -> Result<(), EngineError> {
    let errno = error.errno().unwrap_or(libc::EPIPE);
    if error.is_xrun() {
        control.timeline.record_hardware_xrun(direction);
    }
    control.ensure_running()?;
    if errno == libc::ESTRPIPE {
        resume_suspended_stream(pcm, direction, control)?;
    } else {
        alsa_call(pcm.recover(errno, true), "recover failed stream", direction)?;
    }
    control.ensure_running()?;

    let restarted = if pcm.state() != State::Running {
        if let Some((silence, channels, buffer)) = playback {
            control.ensure_running()?;
            let written =
                write_playback_samples(pcm, silence, channels, buffer as usize, Some(control))?;
            if written != buffer {
                return Err(EngineError::ShortCommit {
                    direction: StreamDirection::Playback,
                    actual: written,
                    required: buffer,
                });
            }
        }
        control.ensure_running()?;
        alsa_call(pcm.start(), "restart recovered stream", direction)?;
        true
    } else {
        false
    };
    if error.is_xrun() || restarted {
        control.timeline.reset_after_stream_restart();
    }
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
        .set_access(Access::MMapInterleaved)
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
        .set_period_size(
            i64::from(config.effective_hardware_period_size()),
            ValueOr::Nearest,
        )
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
        || actual_period != i64::from(config.effective_hardware_period_size())
        || actual_buffer != i64::from(config.buffer_size)
        || actual_channels != stream.channels
        || actual_format != Format::S32LE
        || actual_access != Access::MMapInterleaved
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
    result.map_err(|source| match source.errno() {
        libc::EPIPE => EngineError::Xrun {
            operation,
            direction,
            source,
        },
        libc::ESTRPIPE => EngineError::Suspended {
            operation,
            direction,
            source,
        },
        _ => EngineError::Alsa {
            operation,
            direction,
            source,
        },
    })
}

impl EngineError {
    fn is_xrun(&self) -> bool {
        matches!(self, Self::Xrun { .. } | Self::LinkedXrun { .. })
    }

    fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::Xrun { .. } | Self::LinkedXrun { .. } | Self::Suspended { .. }
        )
    }

    fn is_stopped(&self) -> bool {
        matches!(self, Self::Stopped)
    }

    fn errno(&self) -> Option<i32> {
        match self {
            Self::Xrun { source, .. }
            | Self::LinkedXrun { source, .. }
            | Self::Suspended { source, .. } => Some(source.errno()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DirectDuplexReadiness, EngineError, PendingProCapture, ProClock, STARTUP_LOOPBACK_MARKERS,
        StartupLoopbackProbe, StreamDirection, WorkerControl, align_sequence_forward, alsa_call,
        announced_output_matches, bounded_pro_handoff_target, check_startup_loopback_deadline,
        direct_linked_start_frames, direct_pro_cutoff_nanos, duplex_pointer_phase_from_timestamps,
        linked_ahead_start_frames, linked_phase_dither_frames, linked_phase_target_nanos,
        linked_phase_warmup_cycles, linked_start_frames, linked_zero_lead_playback_floor,
        period_limit_reached, playback_startup_priority, playback_target_sleep,
        prepared_output_matches, pro_target_sequence, realign_playback_after_recovery,
        record_linked_hardware_xruns, staged_playback_chunk_before_capture, startup_loopback_error,
        startup_loopback_padding, take_pending_pro_capture, take_pro_capture_sequence,
        uses_staged_packet_cycle,
    };
    use super::{
        LinkedPollCache, MAX_LINKED_POLL_DESCRIPTORS, PlaybackCommitGuard, pollfd,
        recover_with_commit_released, run_catching_worker_panic,
    };
    use crate::{HardwareTimeline, ProPlaybackSource};
    use std::{
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
        time::{Duration, Instant},
    };

    #[test]
    fn startup_loopback_padding_never_subtracts_or_relabels_frames() {
        assert_eq!(
            startup_loopback_padding(344, 376, 128, 256, 64).unwrap(),
            32
        );
        assert_eq!(startup_loopback_padding(376, 376, 128, 256, 64).unwrap(), 0);
        assert!(matches!(
            startup_loopback_padding(377, 376, 128, 256, 64),
            Err(EngineError::StartupLoopback(
                "unpadded loopback delay already exceeds target"
            ))
        ));
        assert!(startup_loopback_padding(u64::MAX, 376, 128, 256, 64).is_err());
    }

    #[test]
    fn startup_loopback_padding_keeps_one_full_client_block_writable() {
        let start = direct_linked_start_frames(64, 256, Some(2));
        assert_eq!(start, 128);
        assert_eq!(
            startup_loopback_padding(312, 376, start, 256, 64).unwrap(),
            64
        );
        assert_eq!(
            startup_loopback_padding(313, 376, start, 256, 64).unwrap(),
            63
        );
        assert!(startup_loopback_padding(311, 376, start, 256, 64).is_err());
        assert_eq!(
            startup_loopback_padding(376, 376, start, 192, 64).unwrap(),
            0
        );
        assert!(startup_loopback_padding(375, 376, start, 192, 64).is_err());
        assert!(startup_loopback_padding(376, 376, start, 191, 64).is_err());
        assert!(startup_loopback_padding(376, 376, start, 64, 64).is_err());
        // A later hardware start uses its own measurement and the unchanged base prime.
        assert_eq!(
            startup_loopback_padding(352, 376, start, 256, 64).unwrap(),
            24
        );
    }

    #[test]
    fn startup_loopback_requires_three_stable_delays_and_exact_verification() {
        let mut probe = StartupLoopbackProbe {
            markers: STARTUP_LOOPBACK_MARKERS[0],
            interval: 4001,
            delays: [None; 3],
        };
        assert_eq!(probe.measured_delay(None).unwrap(), None);
        probe.delays = [Some(344), Some(344), None];
        assert_eq!(probe.measured_delay(None).unwrap(), None);
        probe.delays[2] = Some(345);
        assert!(matches!(
            probe.measured_delay(None),
            Err(EngineError::StartupLoopback("unstable loopback delay"))
        ));
        probe.delays[2] = Some(344);
        assert_eq!(probe.measured_delay(None).unwrap(), Some(344));
        assert!(probe.measured_delay(Some(376)).is_err());
        probe.delays = [Some(376), Some(376), None];
        assert_eq!(probe.measured_delay(Some(376)).unwrap(), None);
        probe.delays[2] = Some(376);
        assert_eq!(probe.measured_delay(Some(376)).unwrap(), Some(376));
    }

    #[test]
    fn startup_loopback_ignores_unsent_inexact_and_other_channel_markers() {
        let [first, second, third] = STARTUP_LOOPBACK_MARKERS[0];
        let mut probe = StartupLoopbackProbe {
            markers: STARTUP_LOOPBACK_MARKERS[0],
            interval: 4001,
            delays: [None; 3],
        };
        probe.observe(4000, 4000, &[0, first], 2, 1).unwrap();
        probe
            .observe(4300, 4288, &[first, first - 1, second, first + 1], 2, 1)
            .unwrap();
        assert_eq!(probe.delays, [None; 3]);
        for (index, marker) in STARTUP_LOOPBACK_MARKERS[0].into_iter().enumerate() {
            let frame = (index as u64 + 1) * probe.interval + 344;
            probe
                .observe(frame, frame / 64 * 64, &[0, marker], 2, 1)
                .unwrap();
        }
        assert_eq!(probe.measured_delay(None).unwrap(), Some(344));
        assert!(matches!(
            probe.observe(12_400, 12_352, &[0, third], 2, 1),
            Err(EngineError::StartupLoopback("ambiguous repeated probe"))
        ));
    }

    #[test]
    fn startup_loopback_constant_input_cannot_qualify() {
        let mut probe = StartupLoopbackProbe {
            markers: STARTUP_LOOPBACK_MARKERS[0],
            interval: 4001,
            delays: [None; 3],
        };
        assert!(
            probe
                .observe(4352, 4352, &[STARTUP_LOOPBACK_MARKERS[0][0]; 64], 1, 0)
                .is_err()
        );
        assert_eq!(probe.measured_delay(None).unwrap(), None);
    }

    #[test]
    fn startup_loopback_detects_one_db_attenuation_after_24_bit_rounding() {
        let gain = 10_f64.powf(-1.0 / 20.0);
        for markers in STARTUP_LOOPBACK_MARKERS {
            let mut probe = StartupLoopbackProbe {
                markers,
                interval: 4001,
                delays: [None; 3],
            };
            for (index, marker) in markers.into_iter().enumerate() {
                let attenuated = ((f64::from(marker) * gain / 256.0).round() as i32) * 256;
                assert_ne!(marker, attenuated);
                let frame = (index as u64 + 1) * probe.interval + 344;
                probe
                    .observe(frame, frame / 64 * 64, &[attenuated], 1, 0)
                    .unwrap();
            }
            assert_eq!(probe.measured_delay(None).unwrap(), None);
        }
    }

    #[test]
    fn startup_loopback_transfer_failures_are_terminal() {
        assert!(matches!(
            startup_loopback_error(EngineError::ShortCommit {
                direction: StreamDirection::Playback,
                actual: 8,
                required: 32,
            }),
            EngineError::StartupLoopback(_)
        ));
        assert!(matches!(
            startup_loopback_error(EngineError::Stopped),
            EngineError::Stopped
        ));
    }

    #[test]
    fn startup_loopback_markers_preserve_ordinals_across_every_period_split() {
        for markers in STARTUP_LOOPBACK_MARKERS {
            for split in 1..64 {
                let mut probe = StartupLoopbackProbe {
                    markers,
                    interval: 4001,
                    delays: [None; 3],
                };
                let mut playback = [99; 64 * 2];
                probe.emit(3968, &mut playback[..split * 2], 2, 1);
                probe.emit(3968 + split as u64, &mut playback[split * 2..], 2, 1);
                for (frame, samples) in playback.as_chunks::<2>().0.iter().enumerate() {
                    assert_eq!(samples[0], 0);
                    assert_eq!(samples[1], if frame == 33 { markers[0] } else { 0 });
                }

                let mut capture = [0; 64 * 3];
                capture[25 * 3 + 2] = playback[33 * 2 + 1];
                probe
                    .observe(4352, 4352, &capture[..split * 3], 3, 2)
                    .unwrap();
                probe
                    .observe(4352 + split as u64, 4352, &capture[split * 3..], 3, 2)
                    .unwrap();
                assert_eq!(probe.delays, [Some(376), None, None]);
            }
        }
    }

    #[test]
    fn startup_loopback_deadline_and_shutdown_are_terminal() {
        let timeline = HardwareTimeline::default();
        let stop = AtomicBool::new(false);
        let done = AtomicBool::new(false);
        let control = WorkerControl {
            timeline: &timeline,
            stop: &stop,
            done: &done,
            start_gate: None,
            hardware_ready: None,
            capture_cycle_generation: None,
        };
        let error = check_startup_loopback_deadline(control, Instant::now()).unwrap_err();
        assert!(matches!(
            error,
            EngineError::StartupLoopback("two-second deadline expired")
        ));
        assert!(!error.is_recoverable());
        let deadline = Instant::now() + Duration::from_secs(2);
        check_startup_loopback_deadline(control, deadline).unwrap();
        stop.store(true, Ordering::Relaxed);
        assert!(matches!(
            check_startup_loopback_deadline(control, deadline),
            Err(EngineError::Stopped)
        ));
        stop.store(false, Ordering::Relaxed);
        done.store(true, Ordering::Release);
        assert!(matches!(
            check_startup_loopback_deadline(control, deadline),
            Err(EngineError::Stopped)
        ));
        assert_eq!(timeline.generation(), 0);
        assert_eq!(timeline.snapshot().periods_processed, 0);
    }

    #[test]
    fn playback_target_tracks_configured_lead() {
        assert_eq!(pro_target_sequence(4, 0), 4);
        assert_eq!(pro_target_sequence(4, 1), 5);
        assert_eq!(pro_target_sequence(9, 1), 10);
        assert_eq!(pro_target_sequence(u64::MAX, 1), 0);
    }

    #[test]
    fn handoff_target_is_consumed_only_by_its_exact_playback_sequence() {
        let target = PendingProCapture {
            hardware_sequence: u64::MAX,
            playback_sequence: 0,
            target_nanos: 250,
        };
        let mut pending = Some(target);

        assert_eq!(take_pending_pro_capture(&mut pending, u64::MAX), None);
        assert_eq!(pending, Some(target));
        assert_eq!(take_pending_pro_capture(&mut pending, 0), Some(target));
        assert_eq!(pending, None);
    }

    #[test]
    fn capture_publishes_one_monotonic_target_per_period() {
        let mut next = 4;
        assert_eq!(take_pro_capture_sequence(&mut next, 4), 4);
        assert_eq!(take_pro_capture_sequence(&mut next, 5), 5);
        assert_eq!(take_pro_capture_sequence(&mut next, 6), 6);
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
        assert_eq!(take_pro_capture_sequence(&mut next, 20), 50);
        assert_eq!(take_pro_capture_sequence(&mut next, 20), 51);
        assert_eq!(take_pro_capture_sequence(&mut next, 99), 99);
        assert_eq!(take_pro_capture_sequence(&mut next, 100), 100);
    }

    #[test]
    fn capture_advances_locally_when_playback_clock_lags() {
        let mut next = 2;

        assert_eq!(take_pro_capture_sequence(&mut next, 1), 2);
        assert_eq!(take_pro_capture_sequence(&mut next, 1), 3);
        assert_eq!(take_pro_capture_sequence(&mut next, 6), 6);
    }

    #[test]
    fn playback_recovery_catches_up_with_capture_without_moving_backward() {
        let clock = ProClock::new(2);
        let mut sequence = 0;
        assert!(!realign_playback_after_recovery(&mut sequence, &clock, 2));
        assert_eq!(sequence, 0);

        // Normal one-period phase jitter is left alone.
        clock.publish_capture_next(3);
        assert!(!realign_playback_after_recovery(&mut sequence, &clock, 2));
        assert_eq!(sequence, 0);

        // Capture ran 5 periods while playback was recovering.
        clock.publish_capture_next(8);
        assert!(realign_playback_after_recovery(&mut sequence, &clock, 2));
        assert_eq!(sequence, 5);
        assert_eq!(clock.load(), 7);

        clock.publish_capture_next(4);
        assert!(!realign_playback_after_recovery(&mut sequence, &clock, 2));
        assert_eq!(sequence, 5);
    }

    #[test]
    fn finite_period_limit_uses_local_success_count() {
        assert!(!period_limit_reached(0, Some(2)));
        assert!(!period_limit_reached(1, Some(2)));
        assert!(period_limit_reached(2, Some(2)));
        assert!(!period_limit_reached(u64::MAX, None));
    }

    #[test]
    fn playback_recovery_reuses_output_only_within_the_same_generation() {
        let prepared = Some((42, 7, 3));

        assert!(prepared_output_matches(prepared, 42, 7, 3));
        assert!(!prepared_output_matches(prepared, 42, 8, 3));
        assert!(!prepared_output_matches(prepared, 43, 7, 3));
        assert!(!prepared_output_matches(prepared, 42, 7, 4));
        assert!(announced_output_matches(prepared, 42, 7, 3));
        assert!(!announced_output_matches(prepared, 42, 8, 3));
        assert!(!announced_output_matches(prepared, 43, 7, 3));
        assert!(!announced_output_matches(prepared, 42, 7, 4));
    }

    #[test]
    fn alsa_suspend_is_recoverable_but_not_an_xrun() {
        let xrun = alsa_call::<()>(
            Err(alsa::Error::new("test", libc::EPIPE)),
            "test operation",
            StreamDirection::Playback,
        )
        .unwrap_err();
        let suspended = alsa_call::<()>(
            Err(alsa::Error::new("test", libc::ESTRPIPE)),
            "test operation",
            StreamDirection::Capture,
        )
        .unwrap_err();

        assert!(matches!(xrun, EngineError::Xrun { .. }));
        assert!(xrun.is_xrun());
        assert!(xrun.is_recoverable());
        assert!(matches!(suspended, EngineError::Suspended { .. }));
        assert!(!suspended.is_xrun());
        assert!(suspended.is_recoverable());
    }

    #[test]
    fn stream_recovery_runs_with_playback_commit_released() {
        struct CommitProbe<'a>(&'a AtomicBool);
        impl ProPlaybackSource for CommitProbe<'_> {
            fn begin_playback_commit(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
            fn end_playback_commit(&mut self) {
                self.0.store(false, Ordering::Relaxed);
            }
            fn process_playback(&mut self, _sequence: u64, _playback: &mut [i32]) {}
        }

        let timeline = HardwareTimeline::default();
        let stop = AtomicBool::new(false);
        let done = AtomicBool::new(false);
        let control = WorkerControl {
            timeline: &timeline,
            stop: &stop,
            done: &done,
            start_gate: None,
            hardware_ready: None,
            capture_cycle_generation: None,
        };
        let held = AtomicBool::new(false);
        let mut source = CommitProbe(&held);
        let mut commit = PlaybackCommitGuard::begin(&mut source);
        assert!(held.load(Ordering::Relaxed));

        let mut held_during_recovery = true;
        recover_with_commit_released(&mut commit, || {
            held_during_recovery = held.load(Ordering::Relaxed);
            Ok(())
        })
        .unwrap();

        assert!(!held_during_recovery);
        drop(commit);

        // Also verify loop-level panic catching releases the active commit barrier
        // and sets `control.done`.
        let panic_result = run_catching_worker_panic(control, || {
            let _commit = PlaybackCommitGuard::begin(&mut source);
            assert!(held.load(Ordering::Relaxed));
            panic!("simulated callback panic");
        });
        assert!(matches!(panic_result, Err(EngineError::WorkerPanic)));
        assert!(!held.load(Ordering::Relaxed));
        assert!(done.load(Ordering::Acquire));
    }

    #[test]
    fn linked_xrun_records_each_stopped_direction() {
        let timeline = HardwareTimeline::default();
        let stop = AtomicBool::new(false);
        let done = AtomicBool::new(false);
        let control = WorkerControl {
            timeline: &timeline,
            stop: &stop,
            done: &done,
            start_gate: None,
            hardware_ready: None,
            capture_cycle_generation: None,
        };
        let error = EngineError::LinkedXrun {
            operation: "test linked availability",
            playback: true,
            capture: true,
            source: alsa::Error::new("test", libc::EPIPE),
        };

        record_linked_hardware_xruns(&error, control);

        let stats = timeline.snapshot();
        assert_eq!(stats.hw_playback_xruns, 1);
        assert_eq!(stats.hw_capture_xruns, 1);

        let transfer_error = EngineError::Xrun {
            operation: "test linked capture transfer",
            direction: StreamDirection::Capture,
            source: alsa::Error::new("test", libc::EPIPE),
        };
        record_linked_hardware_xruns(&transfer_error, control);

        let stats = timeline.snapshot();
        assert_eq!(stats.hw_playback_xruns, 1);
        assert_eq!(stats.hw_capture_xruns, 2);
    }

    #[test]
    fn worker_control_reports_shutdown_before_recovery_work() {
        let timeline = HardwareTimeline::default();
        let stop = AtomicBool::new(true);
        let done = AtomicBool::new(false);
        let control = WorkerControl {
            timeline: &timeline,
            stop: &stop,
            done: &done,
            start_gate: None,
            hardware_ready: None,
            capture_cycle_generation: None,
        };

        assert!(matches!(
            control.ensure_running(),
            Err(EngineError::Stopped)
        ));
    }

    #[test]
    fn playback_target_sleep_leaves_bounded_prewake_margin() {
        assert_eq!(
            playback_target_sleep(64, 48_000, 50_000),
            Duration::from_nanos(1_283_333)
        );
        assert_eq!(
            playback_target_sleep(12, 48_000, 0),
            Duration::from_micros(250)
        );
        assert_eq!(
            playback_target_sleep(2, 48_000, 50_000),
            Duration::from_micros(1)
        );
    }

    #[test]
    fn effective_direct_playback_queued_bounds_stale_playback_by_capture_overshoot() {
        // Stale playback avail_update (68 queued) with 21-frame capture overshoot (85 avail)
        // on a 128-frame linked lead leaves only 43 queued frames.
        assert_eq!(
            super::effective_direct_playback_queued_frames(256, 188, 128, 85),
            43
        );
        // Fresh playback avail_update (already dropped to 43) is not double-subtracted.
        assert_eq!(
            super::effective_direct_playback_queued_frames(256, 213, 128, 85),
            43
        );
        // Scheduling hiccup with capture >= lead collapses remaining budget to zero.
        assert_eq!(
            super::effective_direct_playback_queued_frames(256, 188, 128, 130),
            0
        );
        // Nominal Q256 wakeup at exact period boundary preserves full 256-frame queue.
        assert_eq!(
            super::effective_direct_playback_queued_frames(512, 256, 512, 256),
            256
        );
        // Deeper startup queue (192 frames) retains full wait margin despite 21-frame overshoot.
        assert_eq!(
            super::effective_direct_playback_queued_frames(256, 128, 192, 85),
            107
        );
    }

    #[test]
    fn direct_linked_start_uses_the_profile_queue_depth() {
        assert_eq!(direct_linked_start_frames(64, 256, None), 128);
        assert_eq!(direct_linked_start_frames(64, 256, Some(2)), 128);
        assert_eq!(direct_linked_start_frames(64, 256, Some(3)), 192);
        assert_eq!(direct_linked_start_frames(64, 256, Some(4)), 256);
        assert_eq!(direct_linked_start_frames(64, 128, Some(2)), 128);
        for period in [64, 128, 192, 256] {
            for queue in [None, Some(1), Some(2), Some(u32::MAX)] {
                assert_eq!(direct_linked_start_frames(period, period, queue), period);
            }
        }
    }

    #[test]
    fn q64_automatic_handoff_does_not_expand_when_the_queue_is_fuller() {
        let mut profile =
            crate::Profile::from_toml(include_str!("../../../profiles/topping-e1x2.toml")).unwrap();
        profile.device.pro_handoff_auto = true;
        assert_eq!(profile.device.pro_handoff_nanos(), 1_000_000);
        for queued in [64, 96, 128, 192] {
            let ready = DirectDuplexReadiness {
                observed_nanos: 1_000_000_000,
                playback_queued_frames: queued,
            };
            assert_eq!(
                direct_pro_cutoff_nanos(ready, 64, 48000, profile.device.pro_handoff_nanos()),
                1_001_000_000
            );
        }
        profile.device.period_size = 256;
        profile.device.hardware_period_size = Some(256);
        assert_eq!(profile.device.pro_handoff_nanos(), 5_000_000);
    }

    #[test]
    fn automatic_dsp_budget_uses_available_period_without_extending_queue() {
        let readiness = DirectDuplexReadiness {
            observed_nanos: 1_000_000_000,
            playback_queued_frames: 256,
        };
        let manual = direct_pro_cutoff_nanos(readiness, 256, 48000, 1_000_000);
        let automatic = direct_pro_cutoff_nanos(readiness, 256, 48000, 256 * 1_000_000_000 / 48000);
        assert_eq!(manual, 1_001_000_000);
        assert_eq!(automatic, 1_005_000_000); // 16-frame / 333 us write reserve.
        let late = DirectDuplexReadiness {
            playback_queued_frames: 16,
            ..readiness
        };
        assert_eq!(
            direct_pro_cutoff_nanos(late, 256, 48000, 5_333_333),
            readiness.observed_nanos
        );
    }

    #[test]
    fn direct_cutoff_reserves_fallback_and_write_time() {
        let readiness = |playback_queued_frames| DirectDuplexReadiness {
            playback_queued_frames,
            observed_nanos: 1_000_000,
        };

        assert_eq!(
            direct_pro_cutoff_nanos(readiness(64), 64, 48_000, 750_000),
            1_750_000
        );
        assert_eq!(
            direct_pro_cutoff_nanos(readiness(48), 64, 48_000, 750_000),
            1_666_666
        );
        assert_eq!(
            direct_pro_cutoff_nanos(readiness(16), 64, 48_000, 750_000),
            1_000_000
        );
    }

    #[test]
    fn direct_cutoff_charges_elapsed_work_to_the_observed_queue_budget() {
        let readiness = DirectDuplexReadiness {
            playback_queued_frames: 64,
            observed_nanos: 1_000_000,
        };
        let cutoff = direct_pro_cutoff_nanos(readiness, 64, 48_000, 1_000_000);
        assert_eq!(cutoff, 2_000_000);
        // Availability-query time, capture publication and preemption all spend
        // the same budget; none may start a new full handoff after they finish.
        assert_eq!(cutoff.saturating_sub(1_200_000), 800_000);
        assert_eq!(cutoff.saturating_sub(2_100_000), 0);
    }

    #[test]
    fn playback_uses_capture_priority_until_deferred_start_is_ready() {
        assert_eq!(playback_startup_priority(Some(88), true), Some(87));
        assert_eq!(playback_startup_priority(Some(1), true), Some(1));
        assert_eq!(playback_startup_priority(Some(88), false), Some(88));
        assert_eq!(playback_startup_priority(None, true), None);
    }

    #[test]
    fn linked_start_primes_period_guard_and_handoff() {
        assert_eq!(linked_start_frames(64, 32, 192, 48_000, 250_000), 108);
        assert_eq!(linked_start_frames(64, 64, 192, 48_000, 250_000), 140);
        assert_eq!(linked_start_frames(64, 32, 128, 48_000, 250_000), 108);
        assert_eq!(linked_start_frames(64, 32, 192, 48_000, 500_000), 120);
    }

    #[test]
    fn zero_lead_refill_accounts_for_hardware_availability_granularity() {
        let playback_floor = linked_zero_lead_playback_floor(32, 32, 192, 64);
        assert_eq!(playback_floor, 128);
        assert_eq!(
            linked_start_frames(64, playback_floor, 192, 48_000, 250_000),
            192
        );
        assert_eq!(linked_zero_lead_playback_floor(32, 32, 128, 64), 64);
        assert_eq!(linked_zero_lead_playback_floor(64, 32, 192, 64), 128);

        let compact_floor = linked_zero_lead_playback_floor(32, 32, 256, 64);
        assert_eq!(compact_floor, 128);
        assert_eq!(
            linked_start_frames(64, compact_floor, 256, 48_000, 500_000),
            216
        );
        let reference_floor = linked_zero_lead_playback_floor(48, 32, 256, 64);
        assert_eq!(reference_floor, 144);
        assert_eq!(
            linked_start_frames(64, reference_floor, 256, 48_000, 500_000),
            232
        );
    }

    #[test]
    fn pro_handoff_is_clamped_before_playback_reserve_is_consumed() {
        assert_eq!(
            bounded_pro_handoff_target(1_000_000, 1_050_000, 500_000, 128, 64, 48_000),
            1_550_000
        );
        assert_eq!(
            bounded_pro_handoff_target(1_000_000, 1_050_000, 500_000, 80, 64, 48_000),
            1_333_333
        );
        assert_eq!(
            bounded_pro_handoff_target(1_000_000, 1_050_000, 500_000, 32, 64, 48_000),
            1_000_000
        );
    }

    #[test]
    fn pre_write_playback_status_subtracts_written_period_and_clamps() {
        assert_eq!(
            super::pre_write_playback_status(128, 188, 64, 256),
            (64, 252)
        );
        assert_eq!(super::pre_write_playback_status(40, 220, 64, 256), (0, 256));
    }

    #[test]
    fn duplex_pointer_phase_normalizes_status_query_time() {
        let timestamp = |nanos: i64| libc::timespec {
            tv_sec: nanos / 1_000_000_000,
            tv_nsec: nanos % 1_000_000_000,
        };

        assert_eq!(
            duplex_pointer_phase_from_timestamps(
                timestamp(10_000_300_000),
                timestamp(2_000_300_000),
                timestamp(10_000_900_000),
                timestamp(2_000_400_000),
            ),
            500_000
        );
    }

    #[test]
    fn linked_process_ahead_primes_one_logical_and_one_physical_period() {
        assert_eq!(linked_ahead_start_frames(64, 32, 192), 96);
        assert_eq!(linked_ahead_start_frames(64, 32, 80), 80);
    }

    #[test]
    fn staged_packet_order_carries_final_chunk_into_next_cycle() {
        assert!(uses_staged_packet_cycle(1, 64, 32));
        assert!(!uses_staged_packet_cycle(0, 64, 32));
        assert!(!uses_staged_packet_cycle(1, 64, 64));
        assert_eq!(staged_playback_chunk_before_capture(0), None);
        assert_eq!(staged_playback_chunk_before_capture(1), Some(0));
        assert_eq!(staged_playback_chunk_before_capture(2), Some(1));
    }

    #[test]
    fn linked_phase_warms_for_one_second_and_preserves_processing_margin() {
        assert_eq!(linked_phase_warmup_cycles(48_000, 64), 750);
        assert_eq!(linked_phase_warmup_cycles(48_000, 128), 375);
        assert_eq!(linked_phase_target_nanos(32, 48_000, 500_000), 416_667);
        assert_eq!(linked_phase_target_nanos(25, 48_000, 250_000), 184_896);
        assert_eq!(linked_phase_dither_frames(1, 32), 1);
        assert_eq!(linked_phase_dither_frames(31, 32), 31);
        assert_eq!(linked_phase_dither_frames(32, 32), 0);
    }

    #[test]
    fn hardware_ready_requires_capture_then_playback_commit() {
        let timeline = HardwareTimeline::default();
        let stop = AtomicBool::new(false);
        let done = AtomicBool::new(false);
        let capture_generation = AtomicU64::new(u64::MAX);
        let hardware_ready = AtomicBool::new(false);
        let control = WorkerControl {
            timeline: &timeline,
            stop: &stop,
            done: &done,
            start_gate: None,
            hardware_ready: Some(&hardware_ready),
            capture_cycle_generation: Some(&capture_generation),
        };

        control.publish_hardware_ready();
        assert!(!hardware_ready.load(Ordering::Acquire));
        control.mark_capture_cycle_ready();
        timeline.reset_after_hardware_rebase();
        control.publish_hardware_ready();
        assert!(!hardware_ready.load(Ordering::Acquire));
        control.mark_capture_cycle_ready();
        control.publish_hardware_ready();
        assert!(hardware_ready.load(Ordering::Acquire));

        // Once ready, steady-state fast path skips capture_cycle_generation writes,
        // and requalification (`false -> true`) still works cleanly.
        capture_generation.store(u64::MAX, Ordering::Release);
        control.mark_capture_cycle_ready();
        assert_eq!(capture_generation.load(Ordering::Acquire), u64::MAX);

        hardware_ready.store(false, Ordering::Release);
        control.publish_hardware_ready();
        assert!(!hardware_ready.load(Ordering::Acquire));
        control.mark_capture_cycle_ready();
        control.publish_hardware_ready();
        assert!(hardware_ready.load(Ordering::Acquire));
    }

    #[test]
    fn linked_poll_cache_selects_active_subslice_and_clears_revents() {
        let mut cache = LinkedPollCache {
            descriptors: [pollfd {
                fd: 0,
                events: 0,
                revents: 0,
            }; MAX_LINKED_POLL_DESCRIPTORS],
            playback_count: 2,
            capture_count: 1,
        };
        cache.descriptors[0] = pollfd {
            fd: 10,
            events: libc::POLLOUT | libc::POLLERR,
            revents: libc::POLLOUT,
        };
        cache.descriptors[1] = pollfd {
            fd: 11,
            events: libc::POLLOUT | libc::POLLERR,
            revents: libc::POLLERR,
        };
        cache.descriptors[2] = pollfd {
            fd: 20,
            events: libc::POLLIN | libc::POLLERR,
            revents: libc::POLLIN,
        };

        let both = cache.active_fds_mut(false, false);
        assert_eq!(both.len(), 3);
        assert_eq!(
            both.iter().map(|p| p.fd).collect::<Vec<_>>(),
            vec![10, 11, 20]
        );
        assert!(
            both.iter()
                .all(|p| p.revents == 0 && (p.events & libc::POLLERR) != 0)
        );
        both[2].revents = libc::POLLIN;

        let capture_only = cache.active_fds_mut(true, false);
        assert_eq!(capture_only.len(), 1);
        assert_eq!(capture_only[0].fd, 20);
        assert_eq!(capture_only[0].revents, 0);

        cache.descriptors[0].revents = libc::POLLOUT;
        let playback_only = cache.active_fds_mut(false, true);
        assert_eq!(playback_only.len(), 2);
        assert_eq!(
            playback_only.iter().map(|p| p.fd).collect::<Vec<_>>(),
            vec![10, 11]
        );
        assert!(playback_only.iter().all(|p| p.revents == 0));

        assert!(cache.active_fds_mut(true, true).is_empty());
    }

    #[test]
    fn pro_clock_separates_playback_and_capture_cache_lines() {
        let clock = ProClock::new(4);
        let seq_line = (&clock.sequence.0 as *const AtomicU64 as usize) / 64;
        let cap_line = (&clock.capture_next.0 as *const AtomicU64 as usize) / 64;
        assert_ne!(seq_line, cap_line);
    }
}
