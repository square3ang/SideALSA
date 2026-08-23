use std::{error::Error, path::PathBuf, thread, time::Duration};

use sidealsa_client::SideAlsaClient;

#[derive(Debug)]
struct Args {
    socket: PathBuf,
    periods: u64,
    delay_ms: u64,
    delay_every: u64,
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
        eprintln!("PRO client test failed: {error}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let client = SideAlsaClient::connect(&args.socket)?;
    let mut stream = client.open_pro()?;
    let info = stream.info();
    let playback_samples = sample_count(info.period_frames, info.playback_channels)?;
    let capture_samples = sample_count(info.period_frames, info.capture_channels)?;
    let mut playback = vec![0; playback_samples];
    let mut capture = vec![0; capture_samples];
    let mut captured = 0_u64;
    let mut published = 0_u64;
    let mut publish_failures = 0_u64;
    let mut wait_offsets = [0_u64; 9];
    let mut capture_offsets = [0_u64; 9];
    let mut submit_offsets = [0_u64; 9];

    let initial_stats = stream.get_stats()?;
    stream.start()?;
    for _ in 0..args.periods {
        let sequence = stream.wait_period(Duration::from_secs(1))?;
        record_sequence_offset(&mut wait_offsets, sequence, stream.playback_sequence());
        maybe_delay(&args, sequence);
        let capture_sequence =
            if let Some(capture_sequence) = stream.capture_buffer(&mut capture)? {
                captured += 1;
                record_sequence_offset(
                    &mut capture_offsets,
                    capture_sequence,
                    stream.playback_sequence(),
                );
                Some(capture_sequence)
            } else {
                None
            };
        playback.fill(0);
        if stream.playback_buffer(&playback)? {
            published += 1;
        } else {
            publish_failures += 1;
        }
        if let Some(capture_sequence) = capture_sequence {
            record_sequence_offset(
                &mut submit_offsets,
                capture_sequence,
                stream.playback_sequence(),
            );
        }
    }
    stream.stop()?;
    let stats = stream.get_stats()?;
    stream.close()?;

    println!("periods_requested={}", args.periods);
    println!("capture_blocks_read={captured}");
    println!("playback_blocks_published={published}");
    println!("playback_publish_failures={publish_failures}");
    println!("wait_offsets_minus4_to_plus4={wait_offsets:?}");
    println!("capture_offsets_minus4_to_plus4={capture_offsets:?}");
    println!("submit_offsets_minus4_to_plus4={submit_offsets:?}");
    println!("periods_processed={}", stats.periods_processed);
    println!(
        "periods_processed_delta={}",
        stats
            .periods_processed
            .saturating_sub(initial_stats.periods_processed)
    );
    println!("generation={}", stats.generation);
    println!("timeline_resets={}", stats.timeline_resets);
    println!("hw_playback_xruns={}", stats.hw_playback_xruns);
    println!("hw_capture_xruns={}", stats.hw_capture_xruns);
    println!("pro_deadline_misses={}", stats.pro_deadline_misses);
    println!(
        "pro_deadline_misses_delta={}",
        stats
            .pro_deadline_misses
            .saturating_sub(initial_stats.pro_deadline_misses)
    );
    println!(
        "pro_playback_blocks_delta={}",
        stats
            .pro_playback_blocks
            .saturating_sub(initial_stats.pro_playback_blocks)
    );
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

fn record_sequence_offset(buckets: &mut [u64; 9], sequence: u64, playback_sequence: u64) {
    let offset = sequence.wrapping_sub(playback_sequence) as i64;
    if (-4..=4).contains(&offset) {
        buckets[(offset + 4) as usize] += 1;
    }
}

fn sample_count(frames: u32, channels: u32) -> Result<usize, Box<dyn Error>> {
    usize::try_from(u64::from(frames) * u64::from(channels))
        .map_err(|_| "audio sample count does not fit usize".into())
}

fn maybe_delay(args: &Args, sequence: u64) {
    if args.delay_ms > 0 && sequence.is_multiple_of(args.delay_every) {
        thread::sleep(Duration::from_millis(args.delay_ms));
    }
}

fn parse_args() -> Result<Args, String> {
    let mut arguments = std::env::args_os().skip(1);
    let mut socket = PathBuf::from("/tmp/sidealsad.sock");
    let mut periods = 3000;
    let mut delay_ms = 0;
    let mut delay_every = 16;

    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--socket") => socket = PathBuf::from(next_value(&mut arguments, "--socket")?),
            Some("--periods") => periods = parse_value(&mut arguments, "--periods")?,
            Some("--delay-ms") => delay_ms = parse_value(&mut arguments, "--delay-ms")?,
            Some("--delay-every") => delay_every = parse_value(&mut arguments, "--delay-every")?,
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
        periods,
        delay_ms,
        delay_every,
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
        "sidealsa-pro-client-test [--socket PATH] [--periods COUNT] [--delay-ms MS] [--delay-every COUNT]"
    );
    println!("default socket: /tmp/sidealsad.sock");
    println!("default periods: 3000");
}
