use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    thread,
};

use sidealsa_config::Profile;
use sidealsa_core::DuplexEngine;
use sidealsa_daemon::{DaemonState, run_control_listener};
use signal_hook::{
    consts::signal::{SIGINT, SIGTERM},
    flag,
};

#[derive(Debug)]
struct Args {
    profile: PathBuf,
    socket: PathBuf,
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
    let engine = match DuplexEngine::open(profile.clone()) {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let timeline = engine.timeline_handle();
    let state = match DaemonState::new(&profile, timeline) {
        Ok(state) => Arc::new(state),
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

    let control_state = Arc::clone(&state);
    let control_stop = Arc::clone(&stop);
    let socket = args.socket.clone();
    let control_handle = thread::spawn(move || {
        let result = run_control_listener(&socket, control_state, Arc::clone(&control_stop));
        if result.is_err() {
            control_stop.store(true, std::sync::atomic::Ordering::Release);
        }
        result
    });

    let hardware_stop = Arc::clone(&stop);
    let (capture_bridge, playback_bridge) = state.bridges();
    let hardware_handle = thread::spawn(move || {
        let mut engine = engine;
        let run_result = engine.run_pro(&hardware_stop, None, capture_bridge, playback_bridge);
        let stop_result = engine.stop();
        (run_result, stop_result)
    });

    let (run_result, stop_result) = match hardware_handle.join() {
        Ok(result) => result,
        Err(_) => {
            stop.store(true, std::sync::atomic::Ordering::Release);
            eprintln!("sidealsad hardware thread panicked");
            std::process::exit(1);
        }
    };
    stop.store(true, std::sync::atomic::Ordering::Release);
    let control_result = control_handle.join();

    if let Err(error) = run_result {
        eprintln!("hardware stopped: {error}");
        std::process::exit(1);
    }
    if let Err(error) = stop_result {
        eprintln!("hardware cleanup failed: {error}");
        std::process::exit(1);
    }
    if let Ok(Err(error)) = control_result {
        eprintln!("control listener stopped: {error}");
        std::process::exit(1);
    }
    let stats = state.stats();
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
    println!("pro_capture_overruns={}", stats.pro_capture_overruns);
    println!(
        "pro_expired_capture_blocks={}",
        stats.pro_expired_capture_blocks
    );
    println!(
        "pro_playback_submit_failures={}",
        stats.pro_playback_submit_failures
    );
    println!("pro_realtime_failures={}", stats.pro_realtime_failures);
    println!("pro_callback_overruns={}", stats.pro_callback_overruns);
    println!("pro_callback_max_nanos={}", stats.pro_callback_max_nanos);
    println!("pro_playback_blocks={}", stats.pro_playback_blocks);
    println!(
        "pro_playback_nonzero_blocks={}",
        stats.pro_playback_nonzero_blocks
    );
    println!("shared_underruns={}", stats.shared_underruns);
    println!("shared_overruns={}", stats.shared_overruns);
}

fn parse_args() -> Result<Args, String> {
    let mut arguments = std::env::args_os().skip(1);
    let mut profile = PathBuf::from("profiles/topping-e1x2.toml");
    let mut socket = PathBuf::from("/tmp/sidealsad.sock");

    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--profile") => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--profile requires a path".to_string())?;
                profile = PathBuf::from(value);
            }
            Some("--socket") => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--socket requires a path".to_string())?;
                socket = PathBuf::from(value);
            }
            Some("--help") | Some("-h") => {
                print_help();
                std::process::exit(0);
            }
            Some(value) => return Err(format!("unknown argument: {value}")),
            None => return Err("arguments must be valid UTF-8".into()),
        }
    }
    Ok(Args { profile, socket })
}

fn print_help() {
    println!("sidealsad [--profile PATH] [--socket PATH]");
    println!("default profile: profiles/topping-e1x2.toml");
    println!("default socket: /tmp/sidealsad.sock");
}
