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
        static ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sidealsa-capture-{}-{}.sock",
            std::process::id(),
            ID.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let region = Arc::new(SharedRegion::create(64, 0, 1).unwrap());
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
                    features: 0,
                },
            )
            .unwrap();
            assert!(matches!(
                read_request(&mut peer).unwrap(),
                Request::OpenShared { .. }
            ));
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
            let frame = sidealsa_protocol::encode_response(&Response::OpenShared {
                session_id: 1,
                direction: PortDirection::Capture,
                shared: shared.info(),
            })
            .unwrap();
            let len = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(&fds) as u32) as usize };
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
                (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(&fds) as u32) as usize;
                ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast(), fds.len());
                assert_eq!(libc::sendmsg(peer.as_raw_fd(), &message, 0), 1);
            }
            peer.write_all(&frame[1..]).unwrap();
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
        let stream = SideAlsaClient::connect(&path)
            .unwrap()
            .open_shared("capture")
            .unwrap();
        std::fs::remove_file(path).unwrap();
        let mut adapter = SideAlsaStream {
            stream,
            pro: false,
            playback: false,
            channels: 1,
            period_frames: 64,
            buffer_frames: 512,
            scratch: vec![0; 64],
            playback_fifo: Vec::new(),
            playback_fifo_frames: 0,
            capture_frames: 0,
            capture_offset: 0,
            capture_retired_frames: 0,
            capture_error: None,
            nonblock: true,
            playback_latency_periods: 0,
            next_playback_sequence: None,
            playback_cycle_sequence: None,
            last_observed_playback_sequence: None,
            last_playback_sequence: None,
            start_sequence: None,
            position: 0,
            running: false,
        };
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
