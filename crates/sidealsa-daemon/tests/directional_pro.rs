//! Real control server/client integration with synthetic periods; no ALSA PCM opens.
use sidealsa_client::{ClientError, SideAlsaClient};
use sidealsa_config::Profile;
use sidealsa_core::{HardwareTimeline, ProCaptureSink, ProPlaybackSource};
use sidealsa_daemon::{DaemonState, run_control_listener};
use sidealsa_protocol::PortDirection;
use std::{
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Server {
    path: PathBuf,
    state: Arc<DaemonState>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let profile = Profile::from_toml(
            r#"
            [device]
            name = "Synthetic duplex"
            rate = 48000
            period_size = 4
            buffer_size = 16
            duplex_link = true
            pro_latency_periods = 0
            pro_handoff_us = 20
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
        )
        .unwrap();
        let state =
            Arc::new(DaemonState::new(&profile, Arc::new(HardwareTimeline::default())).unwrap());
        let path = std::env::temp_dir().join(format!(
            "sidealsa-pair-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_state = Arc::clone(&state);
        let worker_stop = Arc::clone(&stop);
        let worker_path = path.clone();
        let thread = thread::spawn(move || {
            run_control_listener(&worker_path, worker_state, worker_stop).unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "control listener failed to start"
            );
            thread::sleep(Duration::from_millis(1));
        }
        Self {
            path,
            state,
            stop,
            thread: Some(thread),
        }
    }
    fn connect(&self) -> SideAlsaClient {
        SideAlsaClient::connect_with_timeout(&self.path, Duration::from_secs(2)).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn unrelated_process_cannot_join() {
    let Some(path) = std::env::var_os("SIDEALSA_PAIR_TEST_SOCKET") else {
        return;
    };
    let client = SideAlsaClient::connect_with_timeout(path, Duration::from_secs(2)).unwrap();
    assert!(matches!(
        client.open_pro_direction(PortDirection::Playback),
        Err(ClientError::Busy)
    ));
}

#[test]
fn capture_and_playback_keep_their_distinct_sequence_domains() {
    let server = Server::new();
    let mut input = server
        .connect()
        .open_pro_direction(PortDirection::Capture)
        .unwrap();
    let mut output = server
        .connect()
        .open_pro_direction(PortDirection::Playback)
        .unwrap();
    input.start().unwrap();
    output.start().unwrap();
    let (mut capture, mut playback) = server.state.bridges();
    let mut rendered = [0; 8];
    playback.prepare_playback(10);
    capture.process_capture_for_playback(9, 10, &[0; 8]);
    playback.process_playback(10, &mut rendered);
    playback.prepare_playback(11);
    capture.process_capture_for_playback(10, 11, &[23; 8]);
    assert_eq!(input.wait_period(Duration::ZERO).unwrap(), 10);
    assert_eq!(output.wait_pro_playback_period(Duration::ZERO).unwrap(), 11);
    let mut samples = [0; 8];
    assert_eq!(input.capture_buffer(&mut samples).unwrap(), Some(10));
    assert_eq!(samples, [23; 8]);
    assert!(output.submit_playback(11, &[29; 8]).unwrap());
    playback.process_playback_before(11, u64::MAX, &mut rendered);
    assert_eq!(rendered, [29; 8]);
    input.close().unwrap();
    output.close().unwrap();
}

#[test]
fn real_server_pairs_only_complementary_handles_and_preserves_sibling_io() {
    for capture_first in [false, true] {
        let server = Server::new();
        let (mut output, mut input) = if capture_first {
            let input = server
                .connect()
                .open_pro_direction(PortDirection::Capture)
                .unwrap();
            let output = server
                .connect()
                .open_pro_direction(PortDirection::Playback)
                .unwrap();
            (output, input)
        } else {
            let output = server
                .connect()
                .open_pro_direction(PortDirection::Playback)
                .unwrap();
            let input = server
                .connect()
                .open_pro_direction(PortDirection::Capture)
                .unwrap();
            (output, input)
        };
        assert_eq!(output.info().capture_channels, 0);
        assert_eq!(input.info().playback_channels, 0);
        input.record_realtime_failure();
        input.record_callback_timing(12, 10);
        output.record_callback_timing(9, 10);
        let stats = server.state.stats();
        assert_eq!(stats.pro_realtime_failures, 1);
        assert_eq!(stats.pro_callback_overruns, 1);
        assert_eq!(stats.pro_callback_max_nanos, 12);
        assert!(matches!(
            server.connect().open_pro(),
            Err(ClientError::Busy)
        ));
        assert!(matches!(
            server.connect().open_pro_direction(PortDirection::Capture),
            Err(ClientError::Busy)
        ));
        output.start().unwrap();
        input.start().unwrap();
        let (mut capture, mut playback) = server.state.bridges();
        let mut rendered = [0; 8];
        playback.prepare_playback(0);
        capture.process_capture_for_playback(0, 0, &[0; 8]);
        playback.process_playback(0, &mut rendered);
        playback.prepare_playback(1);
        capture.process_capture_for_playback(1, 1, &[11; 8]);
        assert_eq!(output.wait_pro_playback_period(Duration::ZERO).unwrap(), 1);
        assert!(output.submit_playback(1, &[17; 8]).unwrap());
        assert_eq!(input.wait_period(Duration::ZERO).unwrap(), 1);
        let mut captured = [0; 8];
        assert_eq!(input.capture_buffer(&mut captured).unwrap(), Some(1));
        assert_eq!(captured, [11; 8]);
        playback.process_playback_before(1, u64::MAX, &mut rendered);
        assert_eq!(rendered, [17; 8]);

        output.stop().unwrap();
        output.close().unwrap();
        playback.prepare_playback(2);
        capture.process_capture_for_playback(2, 2, &[22; 8]);
        playback.process_playback(2, &mut rendered);
        assert_eq!(rendered, [0; 8]);
        assert_eq!(input.wait_period(Duration::ZERO).unwrap(), 2);
        input.capture_buffer(&mut captured).unwrap();
        assert_eq!(captured, [22; 8]);
        assert!(matches!(
            server.connect().open_pro(),
            Err(ClientError::Busy)
        ));
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "unrelated_process_cannot_join", "--nocapture"])
            .env("SIDEALSA_PAIR_TEST_SOCKET", &server.path)
            .status()
            .unwrap();
        assert!(status.success());

        let mut output = server
            .connect()
            .open_pro_direction(PortDirection::Playback)
            .unwrap();
        output.start().unwrap();
        playback.prepare_playback(3);
        capture.process_capture_for_playback(3, 3, &[33; 8]);
        playback.process_playback(3, &mut rendered);
        input.stop().unwrap();
        input.close().unwrap();
        playback.prepare_playback(4);
        capture.process_capture_for_playback(4, 4, &[44; 8]);
        assert_eq!(output.wait_pro_playback_period(Duration::ZERO).unwrap(), 4);
        assert!(output.submit_playback(4, &[19; 8]).unwrap());
        playback.process_playback_before(4, u64::MAX, &mut rendered);
        assert_eq!(rendered, [19; 8]);
        assert_eq!(server.state.stats().pro_deadline_misses, 0);
        assert_eq!(server.state.stats().generation, 0);
        output.stop().unwrap();
        output.close().unwrap();
        let mut classic = server.connect().open_pro().unwrap();
        assert_eq!(classic.info().capture_channels, 2);
        classic.close().unwrap();
    }
}
