use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use alsa::{
    Direction, ValueOr,
    pcm::{Access, Format, HwParams, PCM, State},
};
use thiserror::Error;

use crate::pro::{
    PopResult, ProCallback, ProRings, SpscAudioRing, playback_source_sequence, run_callback,
};
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
    playback_buffer_samples: usize,
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
        let routing = RoutingTable::compile(&profile).map_err(routing_error_to_engine_error)?;
        let config = profile.device;
        ensure_supported_format(&config.playback.format, StreamDirection::Playback)?;
        ensure_supported_format(&config.capture.format, StreamDirection::Capture)?;

        let period = i64::from(config.period_size);
        let buffer = i64::from(config.buffer_size);
        let playback_period_samples = sample_count(config.period_size, config.playback.channels)?;
        let capture_period_samples = sample_count(config.period_size, config.capture.channels)?;
        let playback_buffer_samples = sample_count(config.buffer_size, config.playback.channels)?;
        let playback_channels = usize::try_from(config.playback.channels)
            .map_err(|_| EngineError::InvalidConfig("playback channels do not fit usize".into()))?;
        let capture_channels = usize::try_from(config.capture.channels)
            .map_err(|_| EngineError::InvalidConfig("capture channels do not fit usize".into()))?;

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
        configure_sw_params(&playback_pcm, buffer, period, StreamDirection::Playback)?;

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
            playback_buffer_samples,
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
        if self.started {
            return Ok(());
        }

        let playback_pcm = self
            .playback_pcm
            .as_ref()
            .ok_or_else(|| EngineError::InvalidConfig("playback worker is active".into()))?;
        let capture_pcm = self
            .capture_pcm
            .as_ref()
            .ok_or_else(|| EngineError::InvalidConfig("capture worker is active".into()))?;
        let written = write_playback_samples(
            playback_pcm,
            &self.playback_silence,
            self.playback_channels,
            self.playback_buffer_samples,
        )?;
        if written != self.buffer {
            return Err(EngineError::ShortCommit {
                direction: StreamDirection::Playback,
                actual: written,
                required: self.buffer,
            });
        }

        if capture_pcm.state() != State::Running {
            alsa_call(
                capture_pcm.start(),
                "start capture stream",
                StreamDirection::Capture,
            )?;
        }
        self.started = true;
        if playback_pcm.state() != State::Running
            && let Err(error) = alsa_call(
                playback_pcm.start(),
                "start playback stream",
                StreamDirection::Playback,
            )
        {
            let _ = self.stop();
            return Err(error);
        }
        Ok(())
    }

    pub fn run(&mut self, stop: &AtomicBool, max_periods: Option<u64>) -> Result<(), EngineError> {
        let _realtime = self.enter_realtime()?;
        self.start()?;

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
        let playback_buffer_samples = self.playback_buffer_samples;
        let period = self.period;
        let buffer = self.buffer;
        let capture_config = CaptureWorkerConfig {
            channels: capture_channels,
            period_samples: capture_period_samples,
            period,
            input: None,
        };
        let playback_config = PlaybackWorkerConfig {
            silence: playback_silence,
            channels: playback_channels,
            period_samples: playback_period_samples,
            buffer_samples: playback_buffer_samples,
            period,
            buffer,
            pro_latency_periods: 0,
            output: None,
        };
        let capture_control = WorkerControl {
            timeline,
            stop,
            done: &done,
        };
        let playback_control = capture_control;
        std::thread::scope(|scope| -> Result<_, EngineError> {
            let capture_handle = scope.spawn(move || {
                capture_worker(
                    capture_pcm,
                    capture_scratch,
                    capture_config,
                    capture_control,
                )
            });
            let playback_handle = scope.spawn(move || {
                playback_worker(
                    playback_pcm,
                    playback_config,
                    playback_control,
                    None,
                    max_periods,
                )
            });
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
                playback_result.and(capture_result)
            },
        )?
    }

    pub fn run_pro<C: ProCallback>(
        &mut self,
        stop: &AtomicBool,
        max_periods: Option<u64>,
        callback: C,
    ) -> Result<(), EngineError> {
        let _realtime = self.enter_realtime()?;
        self.start()?;

        let done = AtomicBool::new(false);
        let rings = ProRings::new(self.capture_period_samples, self.playback_period_samples);
        let mut callback_capture = vec![0; self.capture_period_samples];
        let mut callback_playback = vec![0; self.playback_period_samples];
        let mut playback_scratch = vec![0; self.playback_period_samples];
        let playback_start_index =
            rings.prime_playback(&self.playback_silence[..self.playback_period_samples]);
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
            input: Some(rings.capture()),
        };
        let playback_config = PlaybackWorkerConfig {
            silence: playback_silence,
            channels: self.playback_channels,
            period_samples: self.playback_period_samples,
            buffer_samples: self.playback_buffer_samples,
            period: self.period,
            buffer: self.buffer,
            pro_latency_periods: self.config.pro_latency_periods,
            output: Some(rings.playback()),
        };
        let capture_control = WorkerControl {
            timeline,
            stop,
            done: &done,
        };
        let playback_control = capture_control;
        let callback_done = &done;
        let rings_ref = &rings;

        std::thread::scope(|scope| -> Result<_, EngineError> {
            let callback_handle = scope.spawn(move || {
                run_callback(
                    callback,
                    rings_ref,
                    stop,
                    callback_done,
                    &mut callback_capture,
                    &mut callback_playback,
                    playback_start_index,
                )
            });
            let capture_handle = scope.spawn(move || {
                capture_worker(
                    capture_pcm,
                    capture_scratch,
                    capture_config,
                    capture_control,
                )
            });
            let playback_handle = scope.spawn(move || {
                playback_worker(
                    playback_pcm,
                    playback_config,
                    playback_control,
                    Some(&mut playback_scratch),
                    max_periods,
                )
            });
            let playback_result = playback_handle
                .join()
                .map_err(|_| EngineError::WorkerPanic)?;
            let capture_result = capture_handle
                .join()
                .map_err(|_| EngineError::WorkerPanic)?;
            let callback_result = callback_handle
                .join()
                .map_err(|_| EngineError::WorkerPanic)?;
            Ok((playback_result, capture_result, callback_result))
        })
        .map(
            |((playback_pcm, playback_result), (capture_pcm, capture_result), callback_result)| {
                self.playback_pcm = Some(playback_pcm);
                self.capture_pcm = Some(capture_pcm);
                playback_result.and(capture_result).and(callback_result)
            },
        )?
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

#[derive(Clone, Copy)]
struct WorkerControl<'a> {
    timeline: &'a HardwareTimeline,
    stop: &'a AtomicBool,
    done: &'a AtomicBool,
}

#[derive(Clone, Copy)]
struct PlaybackWorkerConfig<'a> {
    silence: &'a [i32],
    channels: usize,
    period_samples: usize,
    buffer_samples: usize,
    period: alsa::pcm::Frames,
    buffer: alsa::pcm::Frames,
    pro_latency_periods: u32,
    output: Option<&'a SpscAudioRing>,
}

#[derive(Clone, Copy)]
struct CaptureWorkerConfig<'a> {
    channels: usize,
    period_samples: usize,
    period: alsa::pcm::Frames,
    input: Option<&'a SpscAudioRing>,
}

fn playback_worker(
    pcm: PCM,
    config: PlaybackWorkerConfig<'_>,
    control: WorkerControl<'_>,
    output_scratch: Option<&mut [i32]>,
    max_periods: Option<u64>,
) -> (PCM, Result<(), EngineError>) {
    let result = playback_worker_loop(&pcm, config, control, output_scratch, max_periods);
    (pcm, result)
}

fn playback_worker_loop(
    pcm: &PCM,
    config: PlaybackWorkerConfig<'_>,
    control: WorkerControl<'_>,
    output_scratch: Option<&mut [i32]>,
    max_periods: Option<u64>,
) -> Result<(), EngineError> {
    let mut position = 0_u64;
    let mut sequence = 0_u64;
    let mut output_scratch = output_scratch;

    loop {
        if control.stop.load(Ordering::Relaxed)
            || control.done.load(Ordering::Acquire)
            || max_periods
                .is_some_and(|limit| control.timeline.snapshot().periods_processed >= limit)
        {
            control.done.store(true, Ordering::Release);
            return Ok(());
        }

        let samples = match playback_source_sequence(sequence, config.pro_latency_periods) {
            Some(expected_sequence) => match (config.output, output_scratch.as_deref_mut()) {
                (Some(output), Some(scratch)) => {
                    if matches!(
                        output.try_pop_exact(expected_sequence, scratch),
                        PopResult::Ready
                    ) {
                        &scratch[..config.period_samples]
                    } else {
                        control.timeline.record_pro_core_deadline_miss();
                        config.silence
                    }
                }
                (Some(_), None) => {
                    control.timeline.record_pro_core_deadline_miss();
                    config.silence
                }
                (None, _) => config.silence,
            },
            None => config.silence,
        };

        match write_playback_samples(pcm, samples, config.channels, config.period_samples) {
            Ok(written) if written == config.period => {
                position = position.wrapping_add(config.period as u64);
                sequence = sequence.wrapping_add(1);
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
                        config.buffer_samples,
                        config.buffer,
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
    config: CaptureWorkerConfig<'_>,
    control: WorkerControl<'_>,
) -> (PCM, Result<(), EngineError>) {
    let result = capture_worker_loop(&pcm, scratch, config, control);
    (pcm, result)
}

fn capture_worker_loop(
    pcm: &PCM,
    scratch: &mut [i32],
    config: CaptureWorkerConfig<'_>,
    control: WorkerControl<'_>,
) -> Result<(), EngineError> {
    let mut position = 0_u64;
    let mut next_pro_sequence = 1_u64;
    let mut input_index = 0;

    loop {
        if control.stop.load(Ordering::Relaxed) || control.done.load(Ordering::Acquire) {
            control.done.store(true, Ordering::Release);
            return Ok(());
        }

        match read_capture_samples(pcm, scratch, config.channels, config.period_samples) {
            Ok(read) if read == config.period => {
                if let Some(input) = config.input {
                    // Capture can run ahead of playback; publish only next hardware sequence.
                    let available_sequence = control.timeline.periods_processed().wrapping_add(1);
                    if next_pro_sequence < available_sequence.saturating_sub(1) {
                        next_pro_sequence = available_sequence.saturating_sub(1);
                    }
                    if next_pro_sequence <= available_sequence
                        && input.try_push(&mut input_index, next_pro_sequence, scratch)
                    {
                        next_pro_sequence = next_pro_sequence.wrapping_add(1);
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

fn recover_stream(
    pcm: &PCM,
    direction: StreamDirection,
    error: &EngineError,
    timeline: &HardwareTimeline,
    playback: Option<(&[i32], usize, usize, alsa::pcm::Frames)>,
) -> Result<(), EngineError> {
    let errno = error.errno().unwrap_or(libc::EPIPE);
    alsa_call(pcm.recover(errno, true), "recover stream XRUN", direction)?;
    timeline.hardware_xrun(direction);

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
    buffer: alsa::pcm::Frames,
    period: alsa::pcm::Frames,
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
        .set_avail_min(period)
        .map_err(|source| EngineError::Alsa {
            operation: "set software availability threshold",
            direction,
            source,
        })?;
    params
        .set_start_threshold(buffer)
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
