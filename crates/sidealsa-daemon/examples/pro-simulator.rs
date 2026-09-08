//! Hardware-free control/protocol fixture, not a realtime engine or installed daemon.
//! Only bridge sequences advance; hardware statistics stay at their default values.
//! This cannot validate ALSA XRUN recovery, hardware latency, or realtime reliability.
use sidealsa_config::Profile;
use sidealsa_core::{HardwareTimeline, ProCaptureSink, ProPlaybackSource};
use sidealsa_daemon::{DaemonState, run_control_listener};
use signal_hook::{
    consts::signal::{SIGINT, SIGTERM},
    flag,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let socket = PathBuf::from(args.next().ok_or("usage: pro-simulator ABSOLUTE_SOCKET")?);
    if args.next().is_some() || !socket.is_absolute() {
        return Err("usage: pro-simulator ABSOLUTE_SOCKET (no default socket)".into());
    }
    match std::fs::symlink_metadata(&socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("refusing an existing socket path".into()),
    }
    let profile = Profile::from_toml(
        r#"
        [device]
        name = "SideALSA hardware-free ASIO split simulator"
        rate = 48000
        period_size = 64
        buffer_size = 256
        duplex_link = true
        pro_latency_periods = 0
        pro_handoff_us = 1000
        realtime = false
        [device.playback]
        device = "hw:synthetic,0"
        channels = 2
        format = "S32_LE"
        [device.capture]
        device = "hw:synthetic,0"
        channels = 2
        format = "S32_LE"
        "#,
    )?;
    let state = Arc::new(DaemonState::new(
        &profile,
        Arc::new(HardwareTimeline::default()),
    )?);
    let stop = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&stop))?;
    flag::register(SIGTERM, Arc::clone(&stop))?;
    state.hardware_ready_handle().store(true, Ordering::Release);
    let control_state = Arc::clone(&state);
    let control_stop = Arc::clone(&stop);
    let control = thread::spawn(move || {
        let result = run_control_listener(&socket, control_state, Arc::clone(&control_stop));
        control_stop.store(true, Ordering::Release);
        result
    });
    let (mut capture, mut playback) = state.bridges();
    let mut rendered = [0_i32; 128];
    let mut sequence = 0_u64;
    let mut nonzero = false;
    let mut clock_error = None;
    eprintln!("pro-simulator: synthetic 2in/2out 48k Q64 B256; no PCM, no realtime scheduling");
    while !stop.load(Ordering::Acquire) {
        let next = Instant::now() + Duration::from_nanos(64 * 1_000_000_000 / 48_000);
        playback.prepare_playback(sequence);
        capture.process_capture_for_playback(sequence, sequence, &[0; 128]);
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // Bridge deadlines use absolute CLOCK_MONOTONIC nanoseconds, not Instant's epoch.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
            clock_error = Some(std::io::Error::last_os_error());
            stop.store(true, Ordering::Release);
            break;
        }
        let cutoff = now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64 + 1_000_000;
        playback.wait_for_playback_before(sequence, cutoff);
        playback.process_playback_before(sequence, cutoff, &mut rendered);
        nonzero |= rendered.iter().any(|&sample| sample != 0);
        sequence += 1;
        // Deliberately ordinary scheduling: this tests lifecycle/protocol, not deadlines.
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    control.join().map_err(|_| "control listener panicked")??;
    if let Some(error) = clock_error {
        return Err(error.into());
    }
    eprintln!("pro-simulator: synthetic_periods={sequence} nonzero_playback={nonzero}");
    if nonzero {
        return Err("expected silence playback".into());
    }
    Ok(())
}
