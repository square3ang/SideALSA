use std::{error::Error, path::PathBuf, thread, time::Duration};

use sidealsa_client::{SideAlsaClient, StreamMode};

#[derive(Debug)]
struct Args {
    socket: PathBuf,
    port: String,
    periods: u64,
    delay_ms: u64,
    delay_every: u64,
    tone_hz: u32,
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
        eprintln!("shared test failed: {error}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let client = SideAlsaClient::connect(&args.socket)?;
    let mut stream = client.open_shared(args.port.clone())?;
    let direction = stream.mode();
    stream.start()?;

    let info = stream.info();
    let samples = sample_count(info)?;
    let mut audio = vec![0; samples];
    let mut phase = 0.0_f64;
    let mut published = 0_u64;
    let mut publish_failures = 0_u64;
    let mut captured = 0_u64;

    for _ in 0..args.periods {
        let sequence = stream.wait_period(Duration::from_secs(1))?;
        maybe_delay(&args, sequence);
        match direction {
            StreamMode::Shared(sidealsa_protocol::PortDirection::Playback) => {
                audio.fill(0);
                if args.tone_hz > 0 {
                    fill_tone(
                        &mut audio,
                        info.period_frames,
                        info.playback_channels,
                        args.tone_hz,
                        &mut phase,
                    );
                }
                if stream.playback_buffer(&audio)? {
                    published += 1;
                } else {
                    publish_failures += 1;
                }
            }
            StreamMode::Shared(sidealsa_protocol::PortDirection::Capture) => {
                if stream.capture_buffer(&mut audio)?.is_some() {
                    captured += 1;
                }
            }
            StreamMode::Pro => return Err("unexpected PRO stream".into()),
        }
    }

    stream.stop()?;
    let stats = stream.get_stats()?;
    stream.close()?;

    println!("direction={direction:?}");
    println!("periods_requested={}", args.periods);
    println!("playback_blocks_published={published}");
    println!("playback_publish_failures={publish_failures}");
    println!("capture_blocks_read={captured}");
    println!("tone_hz={}", args.tone_hz);
    println!("periods_processed={}", stats.periods_processed);
    println!("generation={}", stats.generation);
    println!("timeline_resets={}", stats.timeline_resets);
    println!("hw_playback_xruns={}", stats.hw_playback_xruns);
    println!("hw_capture_xruns={}", stats.hw_capture_xruns);
    println!("pro_deadline_misses={}", stats.pro_deadline_misses);
    println!(
        "pro_client_deadline_misses={}",
        stats.pro_client_deadline_misses
    );
    println!(
        "pro_core_deadline_misses={}",
        stats.pro_core_deadline_misses
    );
    println!("shared_underruns={}", stats.shared_underruns);
    println!("shared_overruns={}", stats.shared_overruns);
    Ok(())
}

fn sample_count(info: sidealsa_protocol::SharedRegionInfo) -> Result<usize, Box<dyn Error>> {
    usize::try_from(
        u64::from(info.period_frames)
            * u64::from(info.playback_channels.max(info.capture_channels)),
    )
    .map_err(|_| "shared sample count does not fit usize".into())
}

fn fill_tone(samples: &mut [i32], frames: u32, channels: u32, frequency: u32, phase: &mut f64) {
    let frames = usize::try_from(frames).unwrap_or(0);
    let channels = usize::try_from(channels).unwrap_or(0);
    let increment = std::f64::consts::TAU * f64::from(frequency) / 48_000.0;
    let amplitude = f64::from(i32::MAX) * 0.1;
    for frame in 0..frames {
        let value = (*phase).sin() * amplitude;
        for channel in 0..channels {
            samples[frame * channels + channel] = value as i32;
        }
        *phase += increment;
        if *phase >= std::f64::consts::TAU {
            *phase -= std::f64::consts::TAU;
        }
    }
}

fn maybe_delay(args: &Args, sequence: u64) {
    if args.delay_ms > 0 && sequence.is_multiple_of(args.delay_every) {
        thread::sleep(Duration::from_millis(args.delay_ms));
    }
}

fn parse_args() -> Result<Args, String> {
    let mut arguments = std::env::args_os().skip(1);
    let mut socket = PathBuf::from("/tmp/sidealsad.sock");
    let mut port = String::from("line1");
    let mut periods = 3000;
    let mut delay_ms = 0;
    let mut delay_every = 16;
    let mut tone_hz = 0;

    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--socket") => socket = PathBuf::from(next_value(&mut arguments, "--socket")?),
            Some("--port") => port = next_string(&mut arguments, "--port")?,
            Some("--periods") => periods = parse_value(&mut arguments, "--periods")?,
            Some("--delay-ms") => delay_ms = parse_value(&mut arguments, "--delay-ms")?,
            Some("--delay-every") => delay_every = parse_value(&mut arguments, "--delay-every")?,
            Some("--tone-hz") => tone_hz = parse_value(&mut arguments, "--tone-hz")?,
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
    if delay_ms > 0 && delay_every == 0 {
        return Err("--delay-every must be non-zero when --delay-ms is used".into());
    }
    Ok(Args {
        socket,
        port,
        periods,
        delay_ms,
        delay_every,
        tone_hz,
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

fn next_string(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> Result<String, String> {
    next_value(arguments, name)?
        .into_string()
        .map_err(|_| format!("{name} must be valid UTF-8"))
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
        "sidealsa-shared-test [--socket PATH] [--port ID] [--periods COUNT] [--delay-ms MS] [--delay-every COUNT] [--tone-hz HZ]"
    );
    println!("default socket: /tmp/sidealsad.sock");
    println!("default port: line1");
    println!("default periods: 3000");
}
