use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

use sidealsa_core::{DuplexEngine, Profile, RoutingTable};
use signal_hook::{
    consts::signal::{SIGINT, SIGTERM},
    flag,
};

#[derive(Debug)]
struct Args {
    profile: PathBuf,
    periods: Option<u64>,
    list_ports: bool,
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };

    let profile = match Profile::from_path(&args.profile) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };

    if args.list_ports {
        let routing = match RoutingTable::compile(&profile) {
            Ok(routing) => routing,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        };
        print_ports("playback", routing.playback_ports());
        print_ports("capture", routing.capture_ports());
        return;
    }

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

    let run_result = engine.run(&stop, args.periods);
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
    let mut list_ports = false;

    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--profile") => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--profile requires a path".to_string())?;
                profile = PathBuf::from(value);
            }
            Some("--periods") => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--periods requires a number".to_string())?;
                let value = value
                    .to_str()
                    .ok_or_else(|| "--periods must be valid UTF-8".to_string())?;
                periods = Some(
                    value
                        .parse()
                        .map_err(|_| "--periods must be an integer".to_string())?,
                );
            }
            Some("--list-ports") => list_ports = true,
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
        list_ports,
    })
}

fn print_help() {
    println!("sidealsa-hw-test [--profile PATH] [--periods COUNT] [--list-ports]");
    println!("default profile: profiles/topping-e1x2.toml");
    println!("without --periods, run until SIGINT or SIGTERM");
    println!("--list-ports validates and prints compiled logical ports without opening ALSA");
}

fn print_ports(direction: &str, ports: &[sidealsa_core::CompiledPort]) {
    for port in ports {
        println!(
            "{direction} id={} name={} channels={:?}",
            port.id(),
            port.name(),
            port.channels()
        );
    }
}
