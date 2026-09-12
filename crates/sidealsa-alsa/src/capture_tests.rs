use super::*;
use sidealsa_client::SharedRegion;
use sidealsa_protocol::{PROTOCOL_VERSION, Request, Response, read_request, write_response};
use std::{
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixListener,
    },
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
    thread,
};

struct Fixture {
    adapter: SideAlsaStream,
    region: Arc<SharedRegion>,
    producer: usize,
    server: Option<thread::JoinHandle<()>>,
}

#[test]
fn blocking_pro_start_does_not_wait_for_first_hardware_period() {
    let mut f = Fixture::new_mode(false, MODE_PRO, STREAM_PLAYBACK, true);
    assert_eq!(unsafe { sidealsa_stream_stop(&mut f.adapter) }, 0);
    assert_eq!(
        unsafe { sidealsa_stream_set_buffer_size(&mut f.adapter, 64) },
        0
    );
    let source = [42; 64];
    let area = SideAlsaChannelArea {
        addr: source.as_ptr().cast_mut().cast(),
        first: 0,
        step: 32,
    };
    assert_eq!(f.adapter.transfer_playback(&area, 0, 64).unwrap(), 64);
    f.adapter.nonblock = false;
    // The fake hardware never publishes an activation or period. Start must
    // still return, retaining all prepared data for later pointer/I/O pumping.
    f.adapter.start().unwrap();
    assert!(f.adapter.running);
    assert!(!f.adapter.nonblock);
    assert_eq!(f.adapter.playback_fifo_frames, 64);
    assert_eq!(f.adapter.position(), 0);
}

#[test]
fn pro_pointer_counts_consumption_not_unpublished_clock_cycles() {
    let mut f = Fixture::new_mode(false, MODE_PRO, STREAM_PLAYBACK, true);
    f.region.reset_activation();
    assert!(f.region.establish_activation(10));
    f.region.set_cycle_sequence(11);
    f.region.set_playback_sequence(11);
    assert_eq!(f.adapter.position(), 0);
    let source = [42_i32; 64];
    let area = SideAlsaChannelArea {
        addr: source.as_ptr().cast_mut().cast(),
        first: 0,
        step: 32,
    };
    assert_eq!(f.adapter.transfer_playback(&area, 0, 64).unwrap(), 64);
    assert_eq!(f.adapter.position(), 0);
    f.region.set_cycle_sequence(12);
    assert_eq!(f.adapter.position(), 0);
    let mut output = [0; 64];
    assert!(f.region.try_consume_playback(11, &mut output));
    assert_eq!(output, source);
    f.region.set_playback_sequence(12);
    assert_eq!(f.adapter.position(), 64);
    assert_eq!(unsafe { sidealsa_stream_stop(&mut f.adapter) }, 0);
    f.region.set_playback_sequence(13);
    assert_eq!(f.adapter.position(), 64);
}

#[test]
fn pro_negotiated_capacity_bounds_prepared_writes_and_cannot_change_live() {
    let mut f = Fixture::new_mode(false, MODE_PRO, STREAM_PLAYBACK, true);
    assert_eq!(
        unsafe { sidealsa_stream_set_buffer_size(&mut f.adapter, 64) },
        -libc::EBUSY
    );
    assert_eq!(unsafe { sidealsa_stream_stop(&mut f.adapter) }, 0);
    for invalid in [0, 32, 65, 1024] {
        assert_eq!(
            unsafe { sidealsa_stream_set_buffer_size(&mut f.adapter, invalid) },
            -libc::EINVAL
        );
    }
    assert_eq!(
        unsafe { sidealsa_stream_set_buffer_size(&mut f.adapter, 64) },
        0
    );
    let source: [i32; 128] = std::array::from_fn(|i| i as i32);
    let area = SideAlsaChannelArea {
        addr: source.as_ptr().cast_mut().cast(),
        first: 0,
        step: 32,
    };
    assert_eq!(f.adapter.transfer_playback(&area, 0, 128).unwrap(), 64);
    assert_eq!(f.adapter.playback_fifo_frames, 64);
    assert_eq!(&f.adapter.playback_fifo[..64], &source[..64]);
    assert_eq!(f.adapter.transfer_playback(&area, 64, 1), Err(libc::EAGAIN));
    assert_eq!(
        unsafe { sidealsa_stream_set_buffer_size(&mut f.adapter, 128) },
        -libc::EBUSY
    );
    assert_eq!(unsafe { sidealsa_stream_prepare(&mut f.adapter) }, 0);
    assert_eq!(
        unsafe { sidealsa_stream_set_buffer_size(&mut f.adapter, 128) },
        0
    );
    assert_eq!(f.adapter.playback_fifo_frames, 0);
}

#[test]
fn c_shim_tracks_post_callback_cursor_without_opening_pcm() {
    let executable =
        std::env::temp_dir().join(format!("sidealsa-shim-test-{}", std::process::id()));
    let output = std::process::Command::new("cc")
        .args(["-Wall", "-Wextra", "-Werror"])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/capture_shim_test.c"
        ))
        .arg("-lasound")
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = std::process::Command::new(&executable).status().unwrap();
    std::fs::remove_file(executable).unwrap();
    assert!(result.success());
}

impl Fixture {
    fn new(disconnect_on_stats: bool) -> Self {
        Self::new_mode(disconnect_on_stats, MODE_SHARED, STREAM_CAPTURE, false)
    }

    fn new_mode(disconnect_on_stats: bool, mode: c_int, direction: c_int, split: bool) -> Self {
        static ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sidealsa-capture-{}-{}.sock",
            std::process::id(),
            ID.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let playback = direction == STREAM_PLAYBACK;
        let region = Arc::new(
            SharedRegion::create(
                64,
                u32::from(playback || (mode == MODE_PRO && !split)),
                u32::from(!playback || (mode == MODE_PRO && !split)),
            )
            .unwrap(),
        );
        let shared = Arc::clone(&region);
        let server = thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            assert!(matches!(
                read_request(&mut peer).unwrap(),
                Request::Hello { .. }
            ));
            write_response(
                &mut peer,
                &Response::Hello {
                    version: PROTOCOL_VERSION,
                    features: if split { FEATURE_PRO_DIRECTIONS } else { 0 },
                },
            )
            .unwrap();
            assert_eq!(read_request(&mut peer).unwrap(), Request::GetInfo);
            write_response(&mut peer, &Response::Info(test_device_info())).unwrap();
            let port_direction = if playback {
                PortDirection::Playback
            } else {
                PortDirection::Capture
            };
            let request = read_request(&mut peer).unwrap();
            let response = if mode == MODE_SHARED {
                assert!(matches!(request, Request::OpenShared { .. }));
                Response::OpenShared {
                    session_id: 1,
                    direction: port_direction,
                    shared: shared.info(),
                }
            } else if split {
                let Request::OpenProDirection {
                    direction,
                    group_token,
                } = request
                else {
                    panic!("expected directional PRO open, got {request:?}");
                };
                assert_eq!(direction, port_direction);
                assert_ne!(group_token, [0; 2]);
                Response::OpenProDirection {
                    session_id: 1,
                    direction,
                    shared: shared.info(),
                }
            } else {
                assert_eq!(request, Request::OpenPro);
                Response::OpenPro {
                    session_id: 1,
                    shared: shared.info(),
                }
            };
            let events: [OwnedFd; 3] = std::array::from_fn(|_| unsafe {
                let fd = libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC);
                assert!(fd >= 0);
                OwnedFd::from_raw_fd(fd)
            });
            let fds = [
                shared.fd(),
                events[0].as_raw_fd(),
                events[1].as_raw_fd(),
                events[2].as_raw_fd(),
            ];
            send_open_response(&mut peer, &response, &fds);
            while let Ok(request) = read_request(&mut peer) {
                if disconnect_on_stats && matches!(request, Request::GetStats) {
                    write_response(&mut peer, &Response::Stats(Box::default())).unwrap();
                    break;
                }
                let close = matches!(request, Request::Close { .. });
                if matches!(request, Request::Start { .. }) {
                    shared.set_lifecycle_generation(shared.lifecycle_generation().wrapping_add(1));
                }
                assert!(matches!(
                    request,
                    Request::Start { .. } | Request::Stop { .. } | Request::Close { .. }
                ));
                write_response(&mut peer, &Response::Ack).unwrap();
                if close {
                    break;
                }
            }
        });
        let socket = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        let mut handle = ptr::null_mut();
        let (mut rate, mut channels, mut period, mut minimum, mut buffer) = (0, 0, 0, 0, 0);
        let (mut poll_fd, mut control_fd) = (-1, -1);
        assert_eq!(
            unsafe {
                sidealsa_stream_open(
                    socket.as_ptr(),
                    mode,
                    c"capture".as_ptr(),
                    direction,
                    1,
                    &mut handle,
                    &mut rate,
                    &mut channels,
                    &mut period,
                    &mut minimum,
                    &mut buffer,
                    &mut poll_fd,
                    &mut control_fd,
                )
            },
            0
        );
        let mut adapter = unsafe {
            libc::close(poll_fd);
            libc::close(control_fd);
            *Box::from_raw(handle)
        };
        assert_eq!(
            (rate, channels, period),
            (48000, 1, if mode == MODE_SHARED { 256 } else { 64 })
        );
        std::fs::remove_file(path).unwrap();
        assert_eq!(unsafe { sidealsa_stream_prepare(&mut adapter) }, 0);
        adapter.start().unwrap();
        Self {
            adapter,
            region,
            producer: 0,
            server: Some(server),
        }
    }

    fn publish(&mut self, first: i32) -> bool {
        self.region.try_publish_capture(
            &mut self.producer,
            first as u64,
            &std::array::from_fn::<_, 64, _>(|i| first + i as i32),
        )
    }

    fn sync(&mut self, expected: u64, current: u64, boundary: u64) -> c_int {
        unsafe { sidealsa_stream_capture_sync(&mut self.adapter, expected, current, boundary, 512) }
    }

    fn read(&mut self, frames: usize) -> (isize, Vec<i32>) {
        let mut samples = vec![-1; frames];
        let area = SideAlsaChannelArea {
            addr: samples.as_mut_ptr().cast(),
            first: 0,
            step: 32,
        };
        let result = unsafe { sidealsa_stream_transfer(&mut self.adapter, &area, 0, frames) };
        (result, samples)
    }

    fn prepare(&mut self) {
        assert_eq!(unsafe { sidealsa_stream_prepare(&mut self.adapter) }, 0);
        self.producer = 0;
        assert_eq!(self.adapter.position(), 0);
        self.adapter.start().unwrap();
    }
}

pub(super) fn test_device_info() -> sidealsa_protocol::DeviceInfo {
    sidealsa_protocol::DeviceInfo {
        name: "Fake duplex".into(),
        profile_fingerprint: 0,
        rate: 48000,
        period_size: 64,
        hardware_period_size: 64,
        buffer_size: 64,
        shared_buffer_size: 512,
        pro_latency_periods: 0,
        pro_output_latency_frames: 0,
        pro_realtime_priority: 0,
        shared_latency_periods: 6,
        playback_channels: 1,
        capture_channels: 1,
        playback_ports: vec![],
        capture_ports: vec![],
    }
}

pub(super) fn send_open_response(
    peer: &mut std::os::unix::net::UnixStream,
    response: &Response,
    fds: &[c_int; 4],
) {
    let frame = sidealsa_protocol::encode_response(response).unwrap();
    let len = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) as usize };
    let mut control = vec![0_usize; len.div_ceil(size_of::<usize>())];
    let mut iov = libc::iovec {
        iov_base: frame.as_ptr().cast_mut().cast(),
        iov_len: 1,
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = len;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&message);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as usize;
        ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast(), fds.len());
        assert_eq!(libc::sendmsg(peer.as_raw_fd(), &message, 0), 1);
    }
    peer.write_all(&frame[1..]).unwrap();
}

#[test]
fn pro_open_selects_feature_and_capture_query_excludes_classic_and_playback() {
    for split in [false, true] {
        for direction in [STREAM_CAPTURE, STREAM_PLAYBACK] {
            let mut f = Fixture::new_mode(false, MODE_PRO, direction, split);
            assert_eq!(
                unsafe { sidealsa_stream_is_buffered_capture(&f.adapter) },
                c_int::from(split && direction == STREAM_CAPTURE)
            );
            if split && direction == STREAM_CAPTURE {
                f.region.set_cycle_sequence(1000);
                assert_eq!(f.adapter.position(), 0);
                assert!(f.publish(0));
                assert!(f.publish(64));
                assert_eq!(f.adapter.position(), 128);
                assert_eq!(f.read(16), (16, (0..16).collect()));
                assert_eq!(f.sync(16, 80, 4096), 0);
                assert_eq!(f.read(48), (48, (80..128).collect()));
                f.region.record_capture_discontinuity();
                assert_eq!(f.sync(128, 128, 4096), -libc::EPIPE);
                assert_eq!(f.read(1).0, -(libc::EPIPE as isize));
                f.prepare();
                assert!(f.publish(200));
                assert_eq!(f.read(1), (1, vec![200]));
            } else {
                assert_eq!(f.sync(0, 0, 4096), -libc::EINVAL);
            }
        }
    }
    assert_eq!(client_error_code(ClientError::ForkedProcess), libc::ECHILD);
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.adapter.stream.close();
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

#[test]
fn publication_partial_reads_and_forward_retire_real_samples() {
    let mut f = Fixture::new(false);
    f.region.set_cycle_sequence(1000);
    assert_eq!(f.sync(0, 0, 4096), 0);
    assert_eq!(f.adapter.position(), 0);
    assert!(f.publish(0));
    assert!(f.publish(64));
    assert_eq!(f.adapter.position(), 128);
    let (n, samples) = f.read(16);
    assert_eq!(n, 16);
    assert_eq!(samples, (0..16).collect::<Vec<_>>());
    assert_eq!(f.adapter.position(), 128);
    assert_eq!(f.sync(16, 16, 4096), 0);
    assert_eq!(f.sync(16, 80, 4096), 0);
    assert_eq!(f.adapter.position(), 128);
    let (n, samples) = f.read(64);
    assert_eq!(n, 48);
    assert_eq!(&samples[..48], &(80..128).collect::<Vec<_>>());
    assert_eq!(f.adapter.position(), 128);
    assert_eq!(f.sync(128, 128, 4096), 0);
    assert_eq!(f.read(1).0, -(libc::EAGAIN as isize));
    f.prepare();
    assert_eq!(f.adapter.capture_retired_frames, 0);
    assert!(f.publish(200));
    assert_eq!(f.read(1), (1, vec![200]));
}

#[test]
fn cursor_wrap_bounds_backward_and_reset_are_checked_and_latched() {
    let mut f = Fixture::new(false);
    assert!(f.publish(0));
    assert_eq!(f.sync(4090, 10, 4096), 0);
    assert_eq!(f.read(1), (1, vec![16]));
    assert_eq!(f.adapter.position(), 64);
    for (expected, current, boundary) in [
        (17, 16, 4096),
        (17, 0, 4096),
        (0, 513, 4096),
        (0, 65, 4096),
        (4096, 0, 4096),
        (0, 0, 0),
    ] {
        f.prepare();
        assert!(f.publish(0));
        assert_eq!(f.sync(expected, current, boundary), -libc::EPIPE);
        assert_eq!(f.sync(0, 0, 4096), -libc::EPIPE);
        assert_eq!(f.read(1).0, -(libc::EPIPE as isize));
    }
}

#[test]
fn full_ring_and_generation_faults_are_visible_without_skip() {
    let mut f = Fixture::new(false);
    for i in 0..f.region.info().slot_count {
        assert!(f.publish((i * 64) as i32));
    }
    assert!(!f.publish(999));
    // The daemon records a discontinuity after a failed publication.
    f.region.record_capture_discontinuity();
    assert_eq!(f.sync(0, 0, 4096), -libc::EPIPE);
    assert_eq!(f.sync(0, 0, 4096), -libc::EPIPE);
    f.prepare();
    assert_eq!(f.sync(0, 0, 4096), 0);
    assert!(f.publish(100));
    assert_eq!(f.read(1), (1, vec![100]));
    f.region.set_hardware_generation(1);
    assert_eq!(f.sync(1, 1, 4096), -libc::EPIPE);
    f.prepare();
    assert_eq!(f.sync(0, 0, 4096), 0);
    f.region
        .set_lifecycle_generation(f.region.lifecycle_generation().wrapping_add(1));
    assert_eq!(f.sync(0, 0, 4096), -libc::EPIPE);
    f.prepare();
    assert_eq!(f.sync(0, 0, 4096), 0);
}

#[test]
fn partial_transfer_returns_prefix_and_latches_later_error() {
    let mut f = Fixture::new(true);
    assert!(f.publish(0));
    assert_eq!(f.sync(0, 1, 4096), 0);
    f.adapter.stream.get_stats().unwrap();
    f.server.take().unwrap().join().unwrap(); // Fake daemon has disconnected.
    f.adapter.nonblock = false;
    let (n, samples) = f.read(128);
    assert_eq!(n, 63);
    assert_eq!(&samples[..63], &(1..64).collect::<Vec<_>>());
    assert_eq!(f.adapter.capture_retired_frames, 64);
    assert_eq!(f.sync(64, 64, 4096), -libc::ENODEV);
    assert_eq!(f.read(1).0, -(libc::ENODEV as isize));
}
