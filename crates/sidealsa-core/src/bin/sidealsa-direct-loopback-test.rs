use std::{error::Error, path::PathBuf};

use alsa::{Direction, ValueOr};
use alsa::{
    pcm::{Access, Format, HwParams, PCM},
    poll::{self, Descriptors, Flags, pollfd},
};
use sidealsa_core::Profile;

const PULSE_AMPLITUDE: i32 = 1 << 29;
const FIRST_PULSE_OFFSET: u64 = 65;
const PULSE_INTERVAL_FRAMES: u64 = 4097;

#[derive(Debug)]
struct Args {
    profile: PathBuf,
    periods: u64,
    buffer_frames: Option<u32>,
    start_frames: Option<u32>,
    output_channel: usize,
    input_channel: usize,
}

#[derive(Default)]
struct Measurements {
    scheduled: u64,
    measured: u64,
    minimum: u64,
    maximum: u64,
    total: u128,
    physical_minimum: u64,
    physical_maximum: u64,
    physical_total: u128,
    pending_output_frame: Option<(u64, u64)>,
}

struct DuplexPoll {
    descriptors: Vec<pollfd>,
    playback_count: usize,
    capture_count: usize,
}

impl DuplexPoll {
    fn new(playback: &PCM, capture: &PCM) -> alsa::Result<Self> {
        let playback_count = playback.count();
        let capture_count = capture.count();
        let mut descriptors = vec![
            pollfd {
                fd: 0,
                events: 0,
                revents: 0,
            };
            playback_count + capture_count
        ];
        playback.fill(&mut descriptors[..playback_count])?;
        capture.fill(&mut descriptors[playback_count..])?;
        Ok(Self {
            descriptors,
            playback_count,
            capture_count,
        })
    }

    fn wait(&mut self, playback: &PCM, capture: &PCM) -> Result<usize, Box<dyn Error>> {
        let mut playback_ready = false;
        let mut capture_ready = false;
        while !playback_ready || !capture_ready {
            let mut used = 0;
            let playback_range = if playback_ready {
                None
            } else {
                let range = used..used + self.playback_count;
                playback.fill(&mut self.descriptors[range.clone()])?;
                used = range.end;
                Some(range)
            };
            let capture_range = if capture_ready {
                None
            } else {
                let range = used..used + self.capture_count;
                capture.fill(&mut self.descriptors[range.clone()])?;
                used = range.end;
                Some(range)
            };
            for descriptor in &mut self.descriptors[..used] {
                descriptor.events |= libc::POLLERR;
                descriptor.revents = 0;
            }
            if poll::poll(&mut self.descriptors[..used], 1_000)? == 0 {
                return Err("direct ALSA duplex poll timed out".into());
            }
            if let Some(range) = playback_range {
                let events = playback.revents(&self.descriptors[range])?;
                if events.intersects(Flags::ERR | Flags::HUP | Flags::NVAL) {
                    return Err(format!(
                        "playback poll failed with {events:?}, state={:?}",
                        playback.state()
                    )
                    .into());
                }
                playback_ready = events.contains(Flags::OUT);
            }
            if let Some(range) = capture_range {
                let events = capture.revents(&self.descriptors[range])?;
                if events.intersects(Flags::ERR | Flags::HUP | Flags::NVAL) {
                    return Err(format!(
                        "capture poll failed with {events:?}, state={:?}",
                        capture.state()
                    )
                    .into());
                }
                capture_ready = events.contains(Flags::IN);
            }
        }

        let playback_available = usize::try_from(playback.avail_update()?)?;
        let capture_available = usize::try_from(capture.avail_update()?)?;
        Ok(playback_available.min(capture_available))
    }
}

impl Measurements {
    fn observe_capture(
        &mut self,
        block_start: u64,
        capture: &[i32],
        channels: usize,
        input_channel: usize,
    ) {
        let Some((logical_output_frame, physical_output_frame)) = self.pending_output_frame else {
            return;
        };
        for (frame, samples) in capture.chunks_exact(channels).enumerate() {
            if i64::from(samples[input_channel]).abs() < i64::from(PULSE_AMPLITUDE / 2) {
                continue;
            }
            let capture_frame = block_start.saturating_add(frame as u64);
            if capture_frame < physical_output_frame {
                continue;
            }
            let latency = capture_frame.saturating_sub(logical_output_frame);
            let physical_latency = capture_frame - physical_output_frame;
            self.pending_output_frame = None;
            self.measured = self.measured.saturating_add(1);
            self.minimum = if self.measured == 1 {
                latency
            } else {
                self.minimum.min(latency)
            };
            self.maximum = self.maximum.max(latency);
            self.total = self.total.saturating_add(u128::from(latency));
            self.physical_minimum = if self.measured == 1 {
                physical_latency
            } else {
                self.physical_minimum.min(physical_latency)
            };
            self.physical_maximum = self.physical_maximum.max(physical_latency);
            self.physical_total = self
                .physical_total
                .saturating_add(u128::from(physical_latency));
            break;
        }
    }

    fn emit_playback(
        &mut self,
        block_start: u64,
        playback: &mut [i32],
        channels: usize,
        output_channel: usize,
        physical_frame_offset: u64,
        next_pulse_frame: &mut u64,
    ) {
        let block_frames = playback.len() / channels;
        let block_end = block_start.saturating_add(block_frames as u64);
        if *next_pulse_frame < block_start || *next_pulse_frame >= block_end {
            return;
        }
        let frame = (*next_pulse_frame - block_start) as usize;
        playback[frame * channels + output_channel] = PULSE_AMPLITUDE;
        self.pending_output_frame = Some((
            *next_pulse_frame,
            next_pulse_frame.saturating_add(physical_frame_offset),
        ));
        self.scheduled = self.scheduled.saturating_add(1);
        *next_pulse_frame = next_pulse_frame.saturating_add(PULSE_INTERVAL_FRAMES);
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
        eprintln!("direct ALSA loopback failed: {error}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let profile = Profile::from_path(&args.profile)?;
    let config = &profile.device;
    let period_frames = config.period_size;
    let hardware_period_frames = config.effective_hardware_period_size();
    if period_frames != hardware_period_frames {
        return Err("direct baseline requires equal logical and hardware periods".into());
    }
    let buffer_frames = args.buffer_frames.unwrap_or(config.buffer_size);
    if buffer_frames < period_frames.saturating_mul(2)
        || !buffer_frames.is_multiple_of(period_frames)
    {
        return Err("buffer frames must contain at least two complete periods".into());
    }
    let start_frames = args.start_frames.unwrap_or_else(|| {
        config
            .playback_queue_periods
            .map_or(buffer_frames, |periods| {
                period_frames.saturating_mul(periods)
            })
    });
    if start_frames < period_frames || start_frames > buffer_frames {
        return Err("start frames must fit between one period and the hardware buffer".into());
    }

    let playback_channels = usize::try_from(config.playback.channels)?;
    let capture_channels = usize::try_from(config.capture.channels)?;
    if args.output_channel >= playback_channels || args.input_channel >= capture_channels {
        return Err("loopback channel is outside the configured stream".into());
    }

    let playback = PCM::new(&config.playback.device, Direction::Playback, false)?;
    let capture = PCM::new(&config.capture.device, Direction::Capture, false)?;
    configure_pcm(
        &playback,
        config.rate,
        period_frames,
        buffer_frames,
        config.playback.channels,
    )?;
    configure_pcm(
        &capture,
        config.rate,
        period_frames,
        buffer_frames,
        config.capture.channels,
    )?;
    configure_sw(&playback, 0, i64::from(period_frames))?;
    configure_sw(&capture, 0, i64::from(period_frames))?;

    let period = usize::try_from(period_frames)?;
    let mut playback_period = vec![0_i32; period * playback_channels];
    let mut capture_period = vec![0_i32; period * capture_channels];
    let startup = vec![0_i32; usize::try_from(start_frames)? * playback_channels];

    playback.link(&capture)?;
    playback.prepare()?;
    write_frames(&playback, &startup, playback_channels)?;
    playback.start()?;
    let mut duplex_poll = DuplexPoll::new(&playback, &capture)?;

    let mut measurements = Measurements::default();
    let mut playback_position = 0_u64;
    let mut capture_position = 0_u64;
    let mut next_pulse_frame = playback_position.saturating_add(FIRST_PULSE_OFFSET);
    let drain_periods = PULSE_INTERVAL_FRAMES.div_ceil(u64::from(period_frames)) + 2;
    let total_periods = args.periods.saturating_add(drain_periods);
    let mut processed_periods = 0_u64;

    while processed_periods < total_periods {
        let mut available = duplex_poll.wait(&playback, &capture)?;
        while available >= period && processed_periods < total_periods {
            read_frames(&capture, &mut capture_period, capture_channels).map_err(|error| {
                format!("capture transfer at period {processed_periods} failed: {error}")
            })?;
            measurements.observe_capture(
                capture_position,
                &capture_period,
                capture_channels,
                args.input_channel,
            );
            capture_position = capture_position.saturating_add(u64::from(period_frames));

            playback_period.fill(0);
            if processed_periods < args.periods {
                measurements.emit_playback(
                    playback_position,
                    &mut playback_period,
                    playback_channels,
                    args.output_channel,
                    u64::from(start_frames),
                    &mut next_pulse_frame,
                );
            }
            write_frames(&playback, &playback_period, playback_channels).map_err(|error| {
                format!("playback transfer at period {processed_periods} failed: {error}")
            })?;
            playback_position = playback_position.saturating_add(u64::from(period_frames));
            processed_periods = processed_periods.saturating_add(1);
            available -= period;
        }
    }

    playback.unlink()?;
    capture.drop()?;
    playback.drop()?;

    println!("direct_buffer_frames={buffer_frames}");
    println!("direct_start_frames={start_frames}");
    println!("direct_scheduled_pulses={}", measurements.scheduled);
    println!("direct_measurements={}", measurements.measured);
    if measurements.pending_output_frame.is_some() {
        return Err("a direct loopback pulse remained pending".into());
    }
    if measurements.measured != measurements.scheduled || measurements.measured < 2 {
        return Err(format!(
            "measured {} of {} direct loopback pulses",
            measurements.measured, measurements.scheduled
        )
        .into());
    }
    println!("direct_min_frames={}", measurements.minimum);
    println!("direct_max_frames={}", measurements.maximum);
    println!(
        "direct_mean_frames={:.3}",
        measurements.total as f64 / measurements.measured as f64
    );
    println!(
        "direct_min_ms={:.3}",
        measurements.minimum as f64 * 1_000.0 / f64::from(config.rate)
    );
    println!(
        "direct_max_ms={:.3}",
        measurements.maximum as f64 * 1_000.0 / f64::from(config.rate)
    );
    println!(
        "direct_physical_min_frames={}",
        measurements.physical_minimum
    );
    println!(
        "direct_physical_max_frames={}",
        measurements.physical_maximum
    );
    println!(
        "direct_physical_mean_frames={:.3}",
        measurements.physical_total as f64 / measurements.measured as f64
    );
    Ok(())
}

fn configure_pcm(
    pcm: &PCM,
    rate: u32,
    period: u32,
    buffer: u32,
    channels: u32,
) -> alsa::Result<()> {
    let params = HwParams::any(pcm)?;
    params.set_access(Access::MMapInterleaved)?;
    params.set_format(Format::S32LE)?;
    params.set_channels(channels)?;
    params.set_rate(rate, ValueOr::Nearest)?;
    params.set_period_size(i64::from(period), ValueOr::Nearest)?;
    params.set_periods(buffer / period, ValueOr::Nearest)?;
    params.set_buffer_size(i64::from(buffer))?;
    pcm.hw_params(&params)
}

fn configure_sw(pcm: &PCM, start_threshold: i64, avail_min: i64) -> alsa::Result<()> {
    let params = pcm.sw_params_current()?;
    params.set_avail_min(avail_min)?;
    params.set_start_threshold(start_threshold)?;
    params.set_tstamp_mode(true)?;
    params.set_tstamp_type(alsa::pcm::TstampType::Monotonic)?;
    pcm.sw_params(&params)
}

fn read_frames(pcm: &PCM, output: &mut [i32], channels: usize) -> alsa::Result<()> {
    let io = unsafe { pcm.io_unchecked::<i32>() };
    let mut offset = 0;
    while offset < output.len() {
        let available = pcm.avail_update()?;
        if available <= 0 {
            pcm.wait(Some(1000))?;
            continue;
        }
        let remaining_frames = (output.len() - offset) / channels;
        let requested = remaining_frames.min(available as usize);
        let processed = io.mmap(requested, |mapped| {
            let samples = mapped.len().min(requested * channels);
            output[offset..offset + samples].copy_from_slice(&mapped[..samples]);
            samples / channels
        })?;
        if processed == 0 {
            pcm.wait(Some(1000))?;
            continue;
        }
        offset += processed * channels;
    }
    Ok(())
}

fn write_frames(pcm: &PCM, input: &[i32], channels: usize) -> alsa::Result<()> {
    let io = unsafe { pcm.io_unchecked::<i32>() };
    let mut offset = 0;
    while offset < input.len() {
        let available = pcm.avail_update()?;
        if available <= 0 {
            pcm.wait(Some(1000))?;
            continue;
        }
        let remaining_frames = (input.len() - offset) / channels;
        let requested = remaining_frames.min(available as usize);
        let processed = io.mmap(requested, |mapped| {
            let samples = mapped.len().min(requested * channels);
            mapped[..samples].copy_from_slice(&input[offset..offset + samples]);
            samples / channels
        })?;
        if processed == 0 {
            pcm.wait(Some(1000))?;
            continue;
        }
        offset += processed * channels;
    }
    Ok(())
}

fn parse_args() -> Result<Args, String> {
    let mut arguments = std::env::args_os().skip(1);
    let mut profile = PathBuf::from("profiles/topping-e1x2.toml");
    let mut periods = 5_000;
    let mut buffer_frames = None;
    let mut start_frames = None;
    let mut output_channel = 0;
    let mut input_channel = 4;
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--profile") => profile = PathBuf::from(next_value(&mut arguments, "--profile")?),
            Some("--periods") => periods = parse_value(&mut arguments, "--periods")?,
            Some("--buffer-frames") => {
                buffer_frames = Some(parse_value(&mut arguments, "--buffer-frames")?)
            }
            Some("--start-frames") => {
                start_frames = Some(parse_value(&mut arguments, "--start-frames")?)
            }
            Some("--output-channel") => {
                output_channel = parse_value(&mut arguments, "--output-channel")?
            }
            Some("--input-channel") => {
                input_channel = parse_value(&mut arguments, "--input-channel")?
            }
            Some("--help") | Some("-h") => {
                println!(
                    "sidealsa-direct-loopback-test [--profile PATH] [--periods COUNT] [--buffer-frames COUNT] [--start-frames COUNT] [--output-channel INDEX] [--input-channel INDEX]"
                );
                std::process::exit(0);
            }
            Some(value) => return Err(format!("unknown argument: {value}")),
            None => return Err("arguments must be valid UTF-8".into()),
        }
    }
    if periods == 0 {
        return Err("--periods must be non-zero".into());
    }
    Ok(Args {
        profile,
        periods,
        buffer_frames,
        start_frames,
        output_channel,
        input_channel,
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
    next_value(arguments, name)?
        .to_str()
        .ok_or_else(|| format!("{name} must be valid UTF-8"))?
        .parse()
        .map_err(|_| format!("{name} has an invalid value"))
}
