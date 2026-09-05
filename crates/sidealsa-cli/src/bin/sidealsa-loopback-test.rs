use std::{collections::VecDeque, error::Error, path::PathBuf, thread, time::Duration};

use sidealsa_client::SideAlsaClient;

const PULSE_AMPLITUDE: i32 = 1 << 29;
const FIRST_PULSE_FRAME: u64 = 65;
const MAX_PHASE_OBSERVATIONS: usize = 128;

#[derive(Clone, Copy, Default)]
struct PhaseObservation {
    sequence: u64,
    previous_observation_nanos: u64,
    observed_nanos: u64,
    delay_frames: u64,
}

#[derive(Debug)]
struct Args {
    socket: PathBuf,
    periods: u64,
    output_channel: usize,
    input_channel: usize,
    pulse_interval_frames: u64,
    delay_ms: u64,
    delay_every: u64,
    expect_pro_misses: bool,
}

struct LoopbackTracker {
    base_sequence: Option<u64>,
    next_pulse_frame: u64,
    pending: VecDeque<u64>,
    period_frames: u64,
    capture_sequence_lead: u64,
    rate: u32,
    interval_frames: u64,
    output_channel: usize,
    input_channel: usize,
    scheduled_pulses: u64,
    measurements: u64,
    minimum_frames: u64,
    maximum_frames: u64,
    total_frames: u128,
    lost_pulses: u64,
    last_delay_frames: Option<u64>,
    last_observation_nanos: u64,
    phase_observations: [PhaseObservation; MAX_PHASE_OBSERVATIONS],
    phase_observations_count: usize,
    phase_observations_dropped: u64,
}

impl LoopbackTracker {
    fn new(args: &Args, period_frames: u32, capture_sequence_lead: u32, rate: u32) -> Self {
        Self {
            base_sequence: None,
            next_pulse_frame: FIRST_PULSE_FRAME,
            pending: VecDeque::new(),
            period_frames: u64::from(period_frames),
            capture_sequence_lead: u64::from(capture_sequence_lead),
            rate,
            interval_frames: args.pulse_interval_frames,
            output_channel: args.output_channel,
            input_channel: args.input_channel,
            scheduled_pulses: 0,
            measurements: 0,
            minimum_frames: u64::MAX,
            maximum_frames: 0,
            total_frames: 0,
            lost_pulses: 0,
            last_delay_frames: None,
            last_observation_nanos: 0,
            phase_observations: [PhaseObservation::default(); MAX_PHASE_OBSERVATIONS],
            phase_observations_count: 0,
            phase_observations_dropped: 0,
        }
    }

    fn observe_capture(&mut self, sequence: u64, capture: &[i32], channels: usize) {
        let Some(block_start) = self.relative_capture_block_start(sequence) else {
            return;
        };

        for frame in 0..self.period_frames as usize {
            let sample = capture[frame * channels + self.input_channel];
            if i64::from(sample).abs() < i64::from(PULSE_AMPLITUDE / 2) {
                continue;
            }

            let capture_frame = block_start.saturating_add(frame as u64);
            while let Some(&output_frame) = self.pending.front() {
                let delay = capture_frame.saturating_sub(output_frame);
                if capture_frame < output_frame {
                    break;
                }
                if delay >= self.interval_frames {
                    self.pending.pop_front();
                    self.lost_pulses = self.lost_pulses.saturating_add(1);
                    continue;
                }

                self.pending.pop_front();
                self.measurements = self.measurements.saturating_add(1);
                self.minimum_frames = self.minimum_frames.min(delay);
                self.maximum_frames = self.maximum_frames.max(delay);
                self.total_frames = self.total_frames.saturating_add(u128::from(delay));
                let mut time = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                // Match tracefs's mono clock; keep formatting out of the period loop.
                let observed_nanos =
                    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } == 0 {
                        (time.tv_sec as u64)
                            .saturating_mul(1_000_000_000)
                            .saturating_add(time.tv_nsec as u64)
                    } else {
                        0
                    };
                self.record_phase(sequence, delay, observed_nanos);
                break;
            }
        }
        let block_end = block_start.saturating_add(self.period_frames);
        while self.pending.front().is_some_and(|output_frame| {
            output_frame.saturating_add(self.interval_frames) < block_end
        }) {
            self.pending.pop_front();
            self.lost_pulses = self.lost_pulses.saturating_add(1);
        }
    }

    fn record_phase(&mut self, sequence: u64, delay_frames: u64, observed_nanos: u64) {
        if self.last_delay_frames != Some(delay_frames) {
            if let Some(slot) = self
                .phase_observations
                .get_mut(self.phase_observations_count)
            {
                *slot = PhaseObservation {
                    sequence,
                    previous_observation_nanos: self.last_observation_nanos,
                    observed_nanos,
                    delay_frames,
                };
                self.phase_observations_count += 1;
            } else {
                self.phase_observations_dropped = self.phase_observations_dropped.saturating_add(1);
            }
            self.last_delay_frames = Some(delay_frames);
        }
        self.last_observation_nanos = observed_nanos;
    }

    fn prepare_playback(&mut self, sequence: u64, playback: &mut [i32], channels: usize) -> usize {
        let base_sequence = *self.base_sequence.get_or_insert(sequence);
        let period_index = sequence.wrapping_sub(base_sequence);
        if period_index >= (1_u64 << 63) {
            return 0;
        }
        let block_start = period_index.saturating_mul(self.period_frames);
        let block_end = block_start.saturating_add(self.period_frames);
        let mut added = 0;

        while self.next_pulse_frame < block_end {
            self.scheduled_pulses = self.scheduled_pulses.saturating_add(1);
            if self.next_pulse_frame >= block_start {
                let frame = (self.next_pulse_frame - block_start) as usize;
                playback[frame * channels + self.output_channel] = PULSE_AMPLITUDE;
                self.pending.push_back(self.next_pulse_frame);
                added += 1;
            } else {
                self.lost_pulses = self.lost_pulses.saturating_add(1);
            }
            self.next_pulse_frame = self.next_pulse_frame.saturating_add(self.interval_frames);
        }
        added
    }

    fn record_unpublished(&mut self, count: usize) {
        self.pending
            .truncate(self.pending.len().saturating_sub(count));
        self.lost_pulses = self
            .lost_pulses
            .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
    }

    fn stop_emitting(&mut self) {
        self.next_pulse_frame = u64::MAX;
    }

    fn relative_capture_block_start(&self, sequence: u64) -> Option<u64> {
        let base_sequence = self.base_sequence?;
        let physical_sequence = sequence.wrapping_sub(self.capture_sequence_lead);
        let period_index = physical_sequence.wrapping_sub(base_sequence);
        (period_index < (1_u64 << 63)).then(|| period_index.saturating_mul(self.period_frames))
    }

    fn print_summary(&self) {
        for observation in &self.phase_observations[..self.phase_observations_count] {
            println!(
                "loopback_phase_sequence={} previous_observation_nanos={} observed_nanos={} delay_frames={}",
                observation.sequence,
                observation.previous_observation_nanos,
                observation.observed_nanos,
                observation.delay_frames,
            );
        }
        println!(
            "loopback_phase_observations_dropped={}",
            self.phase_observations_dropped
        );
        println!("loopback_scheduled_pulses={}", self.scheduled_pulses);
        println!("loopback_measurements={}", self.measurements);
        println!("loopback_lost_pulses={}", self.lost_pulses);
        if self.measurements > 0 {
            println!("loopback_min_frames={}", self.minimum_frames);
            println!("loopback_max_frames={}", self.maximum_frames);
            println!(
                "loopback_mean_frames={:.3}",
                self.total_frames as f64 / self.measurements as f64
            );
            println!(
                "loopback_min_ms={:.3}",
                self.minimum_frames as f64 * 1_000.0 / f64::from(self.rate)
            );
            println!(
                "loopback_max_ms={:.3}",
                self.maximum_frames as f64 * 1_000.0 / f64::from(self.rate)
            );
        }
    }
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = run(args) {
        eprintln!("PRO loopback test failed: {error}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let mut client = SideAlsaClient::connect(&args.socket)?;
    let device = client.get_info()?;
    let mut stream = client.open_pro()?;
    let info = stream.info();
    let period_frames = usize::try_from(info.period_frames)?;
    let playback_channels = usize::try_from(info.playback_channels)?;
    let capture_channels = usize::try_from(info.capture_channels)?;

    if args.output_channel >= playback_channels {
        return Err(format!(
            "output channel {} is outside 0..{playback_channels}",
            args.output_channel
        )
        .into());
    }
    if args.input_channel >= capture_channels {
        return Err(format!(
            "input channel {} is outside 0..{capture_channels}",
            args.input_channel
        )
        .into());
    }
    if args.pulse_interval_frames <= u64::from(info.period_frames) {
        return Err("pulse interval must exceed one period".into());
    }

    let mut tracker = LoopbackTracker::new(
        &args,
        info.period_frames,
        device.pro_latency_periods,
        device.rate,
    );
    let mut playback = vec![0; period_frames * playback_channels];
    let mut capture = vec![0; period_frames * capture_channels];
    let initial_stats = stream.get_stats()?;

    stream.start()?;
    for _ in 0..args.periods {
        let sequence = stream.wait_period(Duration::from_secs(1))?;
        let capture_sequence = stream.capture_buffer(&mut capture)?;
        if let Some(capture_sequence) = capture_sequence {
            tracker.observe_capture(capture_sequence, &capture, capture_channels);
        }
        maybe_delay(&args, sequence);

        playback.fill(0);
        let added = capture_sequence.map_or(0, |playback_sequence| {
            tracker.prepare_playback(playback_sequence, &mut playback, playback_channels)
        });
        if !stream.playback_buffer(&playback)? {
            tracker.record_unpublished(added);
        }
    }
    tracker.stop_emitting();
    let drain_periods = args
        .pulse_interval_frames
        .div_ceil(u64::from(info.period_frames))
        .saturating_add(1);
    for _ in 0..drain_periods {
        stream.wait_period(Duration::from_secs(1))?;
        if let Some(capture_sequence) = stream.capture_buffer(&mut capture)? {
            tracker.observe_capture(capture_sequence, &capture, capture_channels);
        }
        playback.fill(0);
        if !stream.playback_buffer(&playback)? {
            return Err("could not publish drain silence".into());
        }
    }
    stream.stop()?;
    let stats = stream.get_stats()?;
    stream.close()?;

    tracker.print_summary();
    let pro_deadline_misses = stats
        .pro_deadline_misses
        .saturating_sub(initial_stats.pro_deadline_misses);
    println!("pro_deadline_misses_delta={pro_deadline_misses}");
    let playback_xruns = stats
        .hw_playback_xruns
        .saturating_sub(initial_stats.hw_playback_xruns);
    let capture_xruns = stats
        .hw_capture_xruns
        .saturating_sub(initial_stats.hw_capture_xruns);
    let timeline_resets = stats
        .timeline_resets
        .saturating_sub(initial_stats.timeline_resets);
    println!("hw_playback_xruns_delta={playback_xruns}");
    println!("hw_capture_xruns_delta={capture_xruns}");
    println!("timeline_resets_delta={timeline_resets}");
    if tracker.measurements < 2 {
        return Err(format!(
            "only {} loopback pulses were detected",
            tracker.measurements
        )
        .into());
    }
    if tracker.measurements.saturating_add(tracker.lost_pulses) != tracker.scheduled_pulses {
        return Err("scheduled loopback pulses were not fully accounted".into());
    }
    if !tracker.pending.is_empty() {
        return Err(format!("{} loopback pulses remain pending", tracker.pending.len()).into());
    }
    if tracker.minimum_frames != tracker.maximum_frames {
        return Err(format!(
            "loopback latency varied from {} to {} frames",
            tracker.minimum_frames, tracker.maximum_frames
        )
        .into());
    }
    if !args.expect_pro_misses && tracker.lost_pulses > 0 {
        return Err(format!("{} loopback pulses were lost", tracker.lost_pulses).into());
    }
    if args.expect_pro_misses {
        if pro_deadline_misses == 0 {
            return Err("expected a PRO deadline miss".into());
        }
    } else if pro_deadline_misses > 0 {
        return Err(format!("observed {pro_deadline_misses} PRO deadline misses").into());
    }
    if playback_xruns > 0
        || capture_xruns > 0
        || timeline_resets > 0
        || stats.generation != initial_stats.generation
    {
        return Err("hardware timeline changed during loopback measurement".into());
    }
    Ok(())
}

fn maybe_delay(args: &Args, sequence: u64) {
    if args.delay_ms > 0 && sequence.is_multiple_of(args.delay_every) {
        thread::sleep(Duration::from_millis(args.delay_ms));
    }
}

fn parse_args() -> Result<Args, String> {
    let mut arguments = std::env::args_os().skip(1);
    let mut socket = PathBuf::from("/tmp/sidealsad.sock");
    let mut periods = 30_000;
    let mut output_channel = 0;
    let mut input_channel = 4;
    let mut pulse_interval_frames = 4097;
    let mut delay_ms = 0;
    let mut delay_every = 16;
    let mut expect_pro_misses = false;

    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--socket") => socket = PathBuf::from(next_value(&mut arguments, "--socket")?),
            Some("--periods") => periods = parse_value(&mut arguments, "--periods")?,
            Some("--output-channel") => {
                output_channel = parse_value(&mut arguments, "--output-channel")?
            }
            Some("--input-channel") => {
                input_channel = parse_value(&mut arguments, "--input-channel")?
            }
            Some("--pulse-interval-frames") => {
                pulse_interval_frames = parse_value(&mut arguments, "--pulse-interval-frames")?
            }
            Some("--delay-ms") => delay_ms = parse_value(&mut arguments, "--delay-ms")?,
            Some("--delay-every") => delay_every = parse_value(&mut arguments, "--delay-every")?,
            Some("--expect-pro-misses") => expect_pro_misses = true,
            Some("--help") | Some("-h") => {
                print_help();
                std::process::exit(0);
            }
            Some(value) => return Err(format!("unknown argument: {value}")),
            None => return Err("arguments must be valid UTF-8".into()),
        }
    }
    if periods == 0 {
        return Err("--periods must be non-zero".into());
    }
    if pulse_interval_frames == 0 {
        return Err("--pulse-interval-frames must be non-zero".into());
    }
    if delay_ms > 0 && delay_every == 0 {
        return Err("--delay-every must be non-zero when --delay-ms is used".into());
    }
    if expect_pro_misses && delay_ms == 0 {
        return Err("--expect-pro-misses requires --delay-ms".into());
    }
    Ok(Args {
        socket,
        periods,
        output_channel,
        input_channel,
        pulse_interval_frames,
        delay_ms,
        delay_every,
        expect_pro_misses,
    })
}

fn next_value(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> Result<std::ffi::OsString, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{name} requires a value"))
}

fn parse_value<T>(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> Result<T, String>
where
    T: std::str::FromStr,
{
    let value = next_value(arguments, name)?;
    let value = value
        .to_str()
        .ok_or_else(|| format!("{name} must be valid UTF-8"))?;
    value
        .parse()
        .map_err(|_| format!("{name} must be an integer"))
}

fn print_help() {
    println!(
        "sidealsa-loopback-test [--socket PATH] [--periods COUNT] [--output-channel INDEX] [--input-channel INDEX] [--pulse-interval-frames COUNT] [--delay-ms MS] [--delay-every COUNT] [--expect-pro-misses]"
    );
    println!("default path: output channel 0 to input channel 4");
    println!("default periods: 30000");
    println!("default first pulse: frame {FIRST_PULSE_FRAME}");
    println!("default pulse interval: 4097 frames");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_timeline_removes_process_ahead_lead() {
        let args = Args {
            socket: PathBuf::new(),
            periods: 1,
            output_channel: 0,
            input_channel: 0,
            pulse_interval_frames: 4097,
            delay_ms: 0,
            delay_every: 1,
            expect_pro_misses: false,
        };
        let mut tracker = LoopbackTracker::new(&args, 64, 1, 48_000);
        tracker.base_sequence = Some(10);

        assert_eq!(tracker.relative_capture_block_start(11), Some(0));
        assert_eq!(tracker.relative_capture_block_start(12), Some(64));
    }

    #[test]
    fn first_pulse_exercises_second_physical_half() {
        let args = Args {
            socket: PathBuf::new(),
            periods: 1,
            output_channel: 0,
            input_channel: 0,
            pulse_interval_frames: 4097,
            delay_ms: 0,
            delay_every: 1,
            expect_pro_misses: false,
        };
        let mut tracker = LoopbackTracker::new(&args, 64, 1, 48_000);
        let mut playback = [0; 128];

        assert_eq!(tracker.prepare_playback(0, &mut playback, 2), 0);
        assert_eq!(tracker.prepare_playback(1, &mut playback, 2), 1);
        assert_eq!(playback[2], PULSE_AMPLITUDE);
    }

    #[test]
    fn pending_pulse_expires_without_an_input_edge() {
        let args = Args {
            socket: PathBuf::new(),
            periods: 1,
            output_channel: 0,
            input_channel: 0,
            pulse_interval_frames: 4097,
            delay_ms: 0,
            delay_every: 1,
            expect_pro_misses: false,
        };
        let mut tracker = LoopbackTracker::new(&args, 64, 1, 48_000);
        tracker.base_sequence = Some(0);
        tracker.pending.push_back(FIRST_PULSE_FRAME);

        tracker.observe_capture(67, &[0; 64], 1);

        assert!(tracker.pending.is_empty());
        assert_eq!(tracker.lost_pulses, 1);
    }

    #[test]
    fn phase_observations_bound_transitions_without_growing() {
        let args = Args {
            socket: PathBuf::new(),
            periods: 1,
            output_channel: 0,
            input_channel: 0,
            pulse_interval_frames: 4097,
            delay_ms: 0,
            delay_every: 1,
            expect_pro_misses: false,
        };
        let mut tracker = LoopbackTracker::new(&args, 64, 0, 48_000);
        tracker.record_phase(10, 425, 100);
        tracker.record_phase(11, 425, 200);
        tracker.record_phase(12, 401, 300);
        assert_eq!(tracker.phase_observations_count, 2);
        assert_eq!(tracker.phase_observations[1].sequence, 12);
        assert_eq!(
            tracker.phase_observations[1].previous_observation_nanos,
            200
        );
        assert_eq!(tracker.phase_observations[1].observed_nanos, 300);
        assert_eq!(tracker.phase_observations[1].delay_frames, 401);
        for index in 0..MAX_PHASE_OBSERVATIONS {
            tracker.record_phase(index as u64, index as u64, 400 + index as u64);
        }
        assert_eq!(tracker.phase_observations_count, MAX_PHASE_OBSERVATIONS);
        assert_eq!(tracker.phase_observations_dropped, 2);
    }
}
