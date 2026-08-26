use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::Duration,
};

use sidealsa_core::{DuplexEngine, ProCaptureSink, ProPlaybackSource, Profile};
use signal_hook::{
    consts::signal::{SIGINT, SIGTERM},
    flag,
};

#[derive(Debug)]
struct Args {
    profile: PathBuf,
    periods: Option<u64>,
    delay_ms: u64,
    delay_every: u64,
}

struct FakeCapture {
    delay: Duration,
    delay_every: u64,
}

impl ProCaptureSink for FakeCapture {
    fn process_capture(&mut self, sequence: u64, _capture: &[i32]) {
        if !self.delay.is_zero() && sequence.is_multiple_of(self.delay_every) {
            thread::sleep(self.delay);
        }
    }
}

struct FakePlayback;

impl ProPlaybackSource for FakePlayback {
    fn process_playback(&mut self, _sequence: u64, playback: &mut [i32]) {
        playback.fill(0);
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

    if args.delay_ms > 0 && args.delay_every == 0 {
        eprintln!("--delay-every must be non-zero when --delay-ms is used");
        std::process::exit(2);
    }

    let profile = match Profile::from_path(&args.profile) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let mut engine = match DuplexEngine::open(profile) {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    if let Err(error) = flag::register(SIGINT, Arc::clone(&stop)) {
        eprintln!("could not register SIGINT handler: {error}");
        std::process::exit(1);
    }
    if let Err(error) = flag::register(SIGTERM, Arc::clone(&stop)) {
        eprintln!("could not register SIGTERM handler: {error}");
        std::process::exit(1);
    }

    let capture = FakeCapture {
        delay: Duration::from_millis(args.delay_ms),
        delay_every: args.delay_every,
    };
    let run_result = engine.run_pro(&stop, args.periods, capture, FakePlayback);
    let stop_result = engine.stop();
    let stats = engine.stats();

    if let Err(error) = run_result {
        eprintln!("stream stopped: {error}");
        std::process::exit(1);
    }
    if let Err(error) = stop_result {
        eprintln!("could not stop stream: {error}");
        std::process::exit(1);
    }

    println!("periods_processed={}", stats.periods_processed);
    println!("sample_position={}", stats.sample_position);
    println!("playback_position={}", stats.playback_position);
    println!("capture_position={}", stats.capture_position);
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
}

fn parse_args() -> Result<Args, String> {
    let mut arguments = std::env::args_os().skip(1);
    let mut profile = PathBuf::from("profiles/topping-e1x2.toml");
    let mut periods = None;
    let mut delay_ms = 0;
    let mut delay_every = 16;

    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--profile") => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--profile requires a path".to_string())?;
                profile = PathBuf::from(value);
            }
            Some("--periods") => {
                periods = Some(parse_value(&mut arguments, "--periods")?);
            }
            Some("--delay-ms") => {
                delay_ms = parse_value(&mut arguments, "--delay-ms")?;
            }
            Some("--delay-every") => {
                delay_every = parse_value(&mut arguments, "--delay-every")?;
            }
            Some("--help") | Some("-h") => {
                print_help();
                std::process::exit(0);
            }
            Some(value) => return Err(format!("unknown argument: {value}")),
            None => return Err("arguments must be valid UTF-8".into()),
        }
    }

    Ok(Args {
        profile,
        periods,
        delay_ms,
        delay_every,
    })
}

fn parse_value<T>(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> Result<T, String>
where
    T: std::str::FromStr,
{
    let value = arguments
        .next()
        .ok_or_else(|| format!("{name} requires a value"))?;
    let value = value
        .to_str()
        .ok_or_else(|| format!("{name} must be valid UTF-8"))?;
    value
        .parse()
        .map_err(|_| format!("{name} must be an integer"))
}

fn print_help() {
    println!(
        "sidealsa-pro-test [--profile PATH] [--periods COUNT] [--delay-ms MS] [--delay-every PERIODS]"
    );
    println!("default profile: profiles/topping-e1x2.toml");
    println!("delay defaults to zero; delay-every defaults to 16");
    println!("without --periods, run until SIGINT or SIGTERM");
}
