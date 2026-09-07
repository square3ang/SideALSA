use std::{
    io,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::Duration,
};

use sidealsa_config::Profile;
use sidealsa_config::selection::{
    DEFAULT_SOCKET_PATH, InstalledSelection, LEGACY_PROFILE_PATH, installed_selection,
};
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
    pro_diagnostics: bool,
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
    if args.pro_diagnostics && !profile.device.uses_event_driven_linked_pro() {
        eprintln!("--pro-diagnostics currently requires direct event-driven linked PRO");
        std::process::exit(2);
    }
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
    if args.pro_diagnostics {
        state.enable_pro_diagnostics();
    }
    if let Err(error) = flag::register(SIGINT, Arc::clone(&stop)) {
        eprintln!("could not register SIGINT handler: {error}");
        std::process::exit(1);
    }
    if let Err(error) = flag::register(SIGTERM, Arc::clone(&stop)) {
        eprintln!("could not register SIGTERM handler: {error}");
        std::process::exit(1);
    }
    let hardware_stop = Arc::clone(&stop);
    let hardware_ready = state.hardware_ready_handle();
    let (capture_bridge, playback_bridge) = state.bridges();
    if profile.device.realtime
        && let Err(error) = lock_process_memory()
    {
        eprintln!("could not lock realtime memory: {error}");
        std::process::exit(1);
    }
    let hardware_handle = thread::spawn(move || {
        prefault_realtime_stack();
        let mut engine = engine;
        let run_result = engine.run_pro_with_ready(
            &hardware_stop,
            None,
            capture_bridge,
            playback_bridge,
            &hardware_ready,
        );
        let stop_result = engine.stop();
        (run_result, stop_result)
    });

    while !state.hardware_ready()
        && !hardware_handle.is_finished()
        && !stop.load(std::sync::atomic::Ordering::Acquire)
    {
        thread::sleep(Duration::from_millis(1));
    }
    if !state.hardware_ready() || hardware_handle.is_finished() {
        stop.store(true, std::sync::atomic::Ordering::Release);
        let mut exit_code = 1;
        match hardware_handle.join() {
            Ok((Err(error), _)) => {
                exit_code = hardware_error_exit_code(&error);
                eprintln!("hardware stopped before ready: {error}");
            }
            Ok((_, Err(error))) => eprintln!("hardware cleanup failed before ready: {error}"),
            Ok((Ok(()), Ok(()))) => eprintln!("hardware stopped before ready"),
            Err(_) => eprintln!("sidealsad hardware thread panicked before ready"),
        }
        std::process::exit(exit_code);
    }
    if let Some(loopback) = profile.device.startup_loopback {
        eprintln!(
            "startup digital loopback verified: target={} frames, playback_channel={}, capture_channel={}",
            loopback.target_frames, loopback.playback_channel, loopback.capture_channel
        );
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

    if args.pro_diagnostics {
        while !hardware_handle.is_finished() && !stop.load(std::sync::atomic::Ordering::Acquire) {
            if let Some(snapshot) = state.pro_diagnostics() {
                eprintln!("pro_diagnostics {snapshot:?}");
            }
            thread::sleep(Duration::from_secs(1));
        }
    }
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
        std::process::exit(hardware_error_exit_code(&error));
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
    if args.pro_diagnostics
        && let Some(snapshot) = state.pro_diagnostics()
    {
        eprintln!("pro_diagnostics final {snapshot:?}");
    }
    println!("periods_processed={}", stats.periods_processed);
    println!("sample_position={}", stats.sample_position);
    println!("playback_position={}", stats.playback_position);
    println!("capture_position={}", stats.capture_position);
    println!("linked_phase_attempts={}", stats.linked_phase_attempts);
    println!("linked_phase_rebases={}", stats.linked_phase_rebases);
    println!(
        "linked_phase_score_nanos={}",
        stats.linked_phase_score_nanos
    );
    println!("linked_phase_target_met={}", stats.linked_phase_target_met);
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
    for port in &stats.shared_playback_ports {
        println!(
            "shared_playback_port={} underruns={} last_underrun_sequence={} last_underrun_nanos={} last_sequence_lag_periods={} max_sequence_lag_periods={} expired_playback_periods={} submit_failures={} xruns={}",
            port.port_id,
            port.underruns,
            port.last_underrun_sequence,
            port.last_underrun_nanos,
            port.last_sequence_lag_periods,
            port.max_sequence_lag_periods,
            port.expired_playback_periods,
            port.playback_submit_failures,
            port.playback_xruns,
        );
    }
}

fn hardware_error_exit_code(error: &sidealsa_core::EngineError) -> i32 {
    if matches!(error, sidealsa_core::EngineError::StartupLoopback(_)) {
        78 // EX_CONFIG: don't let systemd retry hardware starts until one happens to fit.
    } else {
        1
    }
}

fn lock_process_memory() -> io::Result<()> {
    if unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[inline(never)]
fn prefault_realtime_stack() {
    const PREFAULT_BYTES: usize = 64 * 1024;
    const PAGE_BYTES: usize = 4096;

    let mut stack = [0_u8; PREFAULT_BYTES];
    for offset in (0..PREFAULT_BYTES).step_by(PAGE_BYTES) {
        unsafe {
            std::ptr::write_volatile(stack.as_mut_ptr().add(offset), 0);
        }
    }
    std::hint::black_box(&mut stack);
}

fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args_os().skip(1))
}

fn parse_args_from(arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<Args, String> {
    parse_args_with_selection(
        arguments,
        || installed_selection().map_err(|e| e.to_string()),
        || {
            // Compatibility only for the concrete profile installed before active.toml existed.
            match std::fs::symlink_metadata(LEGACY_PROFILE_PATH) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e.to_string()),
                Ok(_) => sidealsa_config::selection::validate_installed_profile(
                    std::path::Path::new(LEGACY_PROFILE_PATH),
                )
                .map(|()| true)
                .map_err(|e| e.to_string()),
            }
        },
    )
}

fn parse_args_with_selection(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
    selection: impl FnOnce() -> Result<Option<InstalledSelection>, String>,
    legacy_exists: impl FnOnce() -> Result<bool, String>,
) -> Result<Args, String> {
    let mut profile = None;
    let mut socket = None;
    let mut pro_diagnostics = false;

    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--pro-diagnostics") => pro_diagnostics = true,
            Some("--profile") => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--profile requires a path".to_string())?;
                profile = Some(PathBuf::from(value));
            }
            Some("--socket") => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--socket requires a path".to_string())?;
                socket = Some(PathBuf::from(value));
            }
            Some("--help") | Some("-h") => {
                print_help();
                std::process::exit(0);
            }
            Some(value) => return Err(format!("unknown argument: {value}")),
            None => return Err("arguments must be valid UTF-8".into()),
        }
    }
    if (profile.is_none() || socket.is_none())
        && let Some(selected) = selection()?
    {
        profile.get_or_insert(selected.profile);
        socket.get_or_insert(selected.socket);
    }
    let profile = match profile {
        Some(profile) => profile,
        None if legacy_exists()? => PathBuf::from(LEGACY_PROFILE_PATH),
        None => return Err("no installed selection or legacy profile; use --profile PATH (including development profiles)".into()),
    };
    let socket = socket.unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH));
    Ok(Args {
        profile,
        socket,
        pro_diagnostics,
    })
}

fn print_help() {
    println!("sidealsad [--profile PATH] [--socket PATH] [--pro-diagnostics]");
    println!("--pro-diagnostics: log direct-PRO last-miss timing once per second, outside RT");
    println!(
        "Defaults: trusted /etc/sidealsa/active.toml; legacy profile only if installed; otherwise use --profile PATH."
    );
    println!("Socket without selection: {DEFAULT_SOCKET_PATH}");
}

#[cfg(test)]
mod tests {
    #[test]
    fn selection_precedence_and_absence() {
        use super::*;
        for (flags, selected, legacy, expected) in [
            (
                vec![],
                true,
                false,
                Some(("/selected.toml", "/selected.sock")),
            ),
            (
                vec!["--profile", "dev.toml"],
                true,
                false,
                Some(("dev.toml", "/selected.sock")),
            ),
            (
                vec!["--socket", "/explicit.sock"],
                true,
                false,
                Some(("/selected.toml", "/explicit.sock")),
            ),
            (
                vec!["--profile", "dev.toml"],
                false,
                false,
                Some(("dev.toml", DEFAULT_SOCKET_PATH)),
            ),
            (
                vec![],
                false,
                true,
                Some((LEGACY_PROFILE_PATH, DEFAULT_SOCKET_PATH)),
            ),
            (vec![], false, false, None),
        ] {
            let result = parse_args_with_selection(
                flags.into_iter().map(std::ffi::OsString::from),
                || {
                    Ok(selected.then(|| InstalledSelection {
                        profile: "/selected.toml".into(),
                        socket: "/selected.sock".into(),
                    }))
                },
                || Ok(legacy),
            );
            if let Some((profile, socket)) = expected {
                let args = result.unwrap();
                assert_eq!(args.profile, PathBuf::from(profile));
                assert_eq!(args.socket, PathBuf::from(socket));
            } else {
                assert!(result.unwrap_err().contains("use --profile"));
            }
        }
        parse_args_with_selection(
            ["--profile", "dev.toml", "--socket", "/explicit.sock"]
                .into_iter()
                .map(std::ffi::OsString::from),
            || panic!("selection must not be read"),
            || panic!("legacy must not be checked"),
        )
        .unwrap();
        let error = parse_args_with_selection(
            ["--profile", "dev.toml"]
                .into_iter()
                .map(std::ffi::OsString::from),
            || Err("untrusted selection".into()),
            || panic!("must not fall back"),
        );
        assert_eq!(error.unwrap_err(), "untrusted selection");
    }

    #[test]
    fn pro_diagnostics_flag_is_opt_in() {
        let defaults = super::parse_args_from(
            ["--profile", "dev.toml", "--socket", "/tmp/test.sock"]
                .into_iter()
                .map(std::ffi::OsString::from),
        )
        .unwrap();
        assert!(!defaults.pro_diagnostics);
        let enabled = super::parse_args_from(
            [
                "--pro-diagnostics",
                "--profile",
                "dev.toml",
                "--socket",
                "/tmp/diagnostic-test.sock",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        )
        .unwrap();
        assert!(enabled.pro_diagnostics);
        assert_eq!(
            enabled.socket,
            std::path::PathBuf::from("/tmp/diagnostic-test.sock")
        );
    }

    #[test]
    fn unqualified_loopback_prevents_service_restart() {
        assert_eq!(
            super::hardware_error_exit_code(&sidealsa_core::EngineError::StartupLoopback(
                "missing digital route"
            )),
            78
        );
        assert_eq!(
            super::hardware_error_exit_code(&sidealsa_core::EngineError::Stopped),
            1
        );
        assert!(
            include_str!("../../../packaging/sidealsad.service.in")
                .contains("RestartPreventExitStatus=78")
        );
    }
}
