use std::{error::Error, path::PathBuf, thread, time::Duration};

use sidealsa_client::SideAlsaClient;

struct Args {
    socket: PathBuf,
    samples: u64,
    interval_ms: u64,
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
        eprintln!("stats failed: {error}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let mut client = SideAlsaClient::connect(&args.socket)?;
    for _ in 0..args.samples {
        let stats = client.get_stats()?;
        println!(
            "periods={} pro={} client={} core={} hw_playback={} hw_capture={}",
            stats.periods_processed,
            stats.pro_deadline_misses,
            stats.pro_client_deadline_misses,
            stats.pro_core_deadline_misses,
            stats.hw_playback_xruns,
            stats.hw_capture_xruns,
        );
        println!(
            "shared_underruns={} shared_overruns={} timeline_resets={} generation={}",
            stats.shared_underruns, stats.shared_overruns, stats.timeline_resets, stats.generation,
        );
        thread::sleep(Duration::from_millis(args.interval_ms));
    }
    Ok(())
}

fn parse_args() -> Result<Args, String> {
    let mut arguments = std::env::args_os().skip(1);
    let mut socket = PathBuf::from("/tmp/sidealsad.sock");
    let mut samples = 100;
    let mut interval_ms = 10;

    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--socket") => socket = PathBuf::from(next_value(&mut arguments, "--socket")?),
            Some("--samples") => samples = parse_value(&mut arguments, "--samples")?,
            Some("--interval-ms") => interval_ms = parse_value(&mut arguments, "--interval-ms")?,
            Some("--help") | Some("-h") => {
                println!("sidealsa-stats [--socket PATH] [--samples COUNT] [--interval-ms MS]");
                std::process::exit(0);
            }
            Some(value) => return Err(format!("unknown argument: {value}")),
            None => return Err("arguments must be valid UTF-8".into()),
        }
    }
    if samples == 0 {
        return Err("--samples must be non-zero".into());
    }
    Ok(Args {
        socket,
        samples,
        interval_ms,
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
