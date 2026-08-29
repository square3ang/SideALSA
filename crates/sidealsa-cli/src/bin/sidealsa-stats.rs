use std::{error::Error, io, path::PathBuf, thread, time::Duration};

use sidealsa_client::SideAlsaClient;

struct Args {
    socket: PathBuf,
    samples: u64,
    interval_ms: u64,
    expect_peer_pid: Option<u32>,
    expect_peer_uid: Option<u32>,
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
    let credentials = client.peer_credentials()?;
    if let Some(expected) = args.expect_peer_pid
        && expected != credentials.pid
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "daemon peer PID {} does not match expected PID {expected}",
                credentials.pid
            ),
        )
        .into());
    }
    if let Some(expected) = args.expect_peer_uid
        && expected != credentials.uid
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "daemon peer UID {} does not match expected UID {expected}",
                credentials.uid
            ),
        )
        .into());
    }
    let daemon_pid = credentials.pid;
    for _ in 0..args.samples {
        let stats = client.get_stats()?;
        println!(
            "daemon_pid={} periods={} pro={} client={} core={} hw_playback={} hw_capture={}",
            daemon_pid,
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
        println!(
            "sample_position={} playback_position={} capture_position={}",
            stats.sample_position, stats.playback_position, stats.capture_position,
        );
        println!(
            "playback_delay={} capture_delay={} playback_low_watermarks={}",
            stats.playback_delay_frames, stats.capture_delay_frames, stats.playback_low_watermarks,
        );
        println!(
            "playback_delay_min={} playback_delay_max={}",
            stats.playback_delay_min_frames, stats.playback_delay_max_frames,
        );
        println!(
            "playback_ring_delay={} playback_ring_delay_min={} playback_ring_delay_max={}",
            stats.playback_ring_delay_frames,
            stats.playback_ring_delay_min_frames,
            stats.playback_ring_delay_max_frames,
        );
        println!(
            "playback_driver_delay={} playback_driver_delay_min={} playback_driver_delay_max={}",
            stats.playback_driver_delay_frames,
            stats.playback_driver_delay_min_frames,
            stats.playback_driver_delay_max_frames,
        );
        println!(
            "capture_delay_min={} capture_delay_max={}",
            stats.capture_delay_min_frames, stats.capture_delay_max_frames,
        );
        println!(
            "playback_overshoot_max={} capture_clock_wait_max_nanos={}",
            stats.playback_target_overshoot_max_frames, stats.capture_clock_wait_max_nanos,
        );
        println!(
            "pro_wait_budget_nanos_min={} pro_wait_budget_nanos_max={} pro_ready_wait_max_nanos={} playback_write_max_nanos={}",
            stats.pro_wait_budget_min_nanos,
            stats.pro_wait_budget_max_nanos,
            stats.pro_ready_wait_max_nanos,
            stats.playback_write_max_nanos,
        );
        println!(
            "capture_to_playback_write_nanos={} capture_to_playback_write_min_nanos={} capture_to_playback_write_max_nanos={}",
            stats.capture_to_playback_write_nanos,
            stats.capture_to_playback_write_min_nanos,
            stats.capture_to_playback_write_max_nanos,
        );
        println!(
            "duplex_pointer_phase_nanos={} duplex_pointer_phase_min_nanos={} duplex_pointer_phase_max_nanos={} duplex_pointer_phase_samples={}",
            stats.duplex_pointer_phase_nanos,
            stats.duplex_pointer_phase_min_nanos,
            stats.duplex_pointer_phase_max_nanos,
            stats.duplex_pointer_phase_samples,
        );
        println!(
            "linked_phase_attempts={} linked_phase_rebases={} linked_phase_score_nanos={} linked_phase_target_met={}",
            stats.linked_phase_attempts,
            stats.linked_phase_rebases,
            stats.linked_phase_score_nanos,
            stats.linked_phase_target_met,
        );
        println!(
            "pro_playback_blocks={} pro_playback_nonzero_blocks={}",
            stats.pro_playback_blocks, stats.pro_playback_nonzero_blocks,
        );
        println!(
            "pro_capture_overruns={} expired_capture={} submit_failures={} rt_failures={}",
            stats.pro_capture_overruns,
            stats.pro_expired_capture_blocks,
            stats.pro_playback_submit_failures,
            stats.pro_realtime_failures,
        );
        println!(
            "callback_overruns={} callback_max_nanos={}",
            stats.pro_callback_overruns, stats.pro_callback_max_nanos,
        );
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
        thread::sleep(Duration::from_millis(args.interval_ms));
    }
    Ok(())
}

fn parse_args() -> Result<Args, String> {
    let mut arguments = std::env::args_os().skip(1);
    let mut socket = PathBuf::from("/tmp/sidealsad.sock");
    let mut samples = 100;
    let mut interval_ms = 10;
    let mut expect_peer_pid = None;
    let mut expect_peer_uid = None;

    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--socket") => socket = PathBuf::from(next_value(&mut arguments, "--socket")?),
            Some("--samples") => samples = parse_value(&mut arguments, "--samples")?,
            Some("--interval-ms") => interval_ms = parse_value(&mut arguments, "--interval-ms")?,
            Some("--expect-peer-pid") => {
                expect_peer_pid = Some(parse_value(&mut arguments, "--expect-peer-pid")?)
            }
            Some("--expect-peer-uid") => {
                expect_peer_uid = Some(parse_value(&mut arguments, "--expect-peer-uid")?)
            }
            Some("--help") | Some("-h") => {
                println!(
                    "sidealsa-stats [--socket PATH] [--samples COUNT] [--interval-ms MS] [--expect-peer-pid PID] [--expect-peer-uid UID]"
                );
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
        expect_peer_pid,
        expect_peer_uid,
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
