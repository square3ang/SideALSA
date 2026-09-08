// Included in the library's unit tests so the probe exercises the current C/Rust
// plugin entry, not a previously built or installed cdylib.
use super::*;
use crate::capture_tests::{send_open_response, test_device_info};
use sidealsa_client::SharedRegion;
use sidealsa_protocol::{
    PROTOCOL_VERSION, Request, Response, read_request, write_request, write_response,
};
use std::{
    ffi::CString,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::{UnixListener, UnixStream},
    },
    sync::{Arc, Mutex},
    thread,
};

type PluginOpen = unsafe extern "C" fn(
    *mut *mut c_void,
    *const c_char,
    *mut c_void,
    *mut c_void,
    c_int,
    c_int,
) -> c_int;
type Probe = unsafe extern "C" fn(
    PluginOpen,
    *const c_char,
    c_int,
    extern "C" fn(*const c_char, c_int),
) -> c_int;

extern "C" fn other_owner(socket: *const c_char, direction: c_int) {
    // A distinct capability models an unrelated owner even though the transport
    // lives in this test process. Also check that classic PRO cannot join it.
    let path = unsafe { CStr::from_ptr(socket) }.to_str().unwrap();
    for request in [
        Request::OpenProDirection {
            direction: if direction == 0 {
                PortDirection::Playback
            } else {
                PortDirection::Capture
            },
            group_token: [0; 2],
        },
        Request::OpenPro,
    ] {
        let mut peer = UnixStream::connect(path).unwrap();
        write_request(
            &mut peer,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        sidealsa_protocol::read_response(&mut peer).unwrap();
        write_request(&mut peer, &request).unwrap();
        assert_eq!(
            sidealsa_protocol::read_response(&mut peer).unwrap(),
            Response::Busy
        );
    }
}

#[test]
fn real_libasound_separate_pro_handles_both_orders() {
    let library_path =
        std::env::temp_dir().join(format!("sidealsa-split-probe-{}.so", std::process::id()));
    let output = std::process::Command::new("cc")
        .args([
            "-Wall",
            "-Wextra",
            "-Werror",
            "-shared",
            "-fPIC",
            "-DSIDEALSA_PROBE_LIBRARY",
        ])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/split_probe.c"))
        .args(["-lasound", "-ldl", "-o"])
        .arg(&library_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let library_name = CString::new(library_path.to_str().unwrap()).unwrap();
    let library = unsafe { libc::dlopen(library_name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    assert!(!library.is_null());
    let symbol = unsafe { libc::dlsym(library, c"sidealsa_split_probe".as_ptr()) };
    assert!(!symbol.is_null());
    let probe: Probe = unsafe { std::mem::transmute(symbol) };

    for capture_first in [0, 1] {
        let path = std::env::temp_dir().join(format!(
            "sidealsa-split-{}-{capture_first}.sock",
            std::process::id()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let owner = Arc::new(Mutex::new(None::<[u64; 2]>));
        let opened = Arc::new(Mutex::new([false; 2]));
        let logs = Arc::new(Mutex::new([Vec::new(), Vec::new()]));
        let server_logs = Arc::clone(&logs);
        let server = thread::spawn(move || {
            let mut workers = vec![];
            // Two successful handles, two duplicates, unrelated token, classic.
            for _ in 0..6 {
                let (mut peer, _) = listener.accept().unwrap();
                peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let owner = Arc::clone(&owner);
                let opened = Arc::clone(&opened);
                let logs = Arc::clone(&server_logs);
                workers.push(thread::spawn(move || {
                    assert!(matches!(
                        read_request(&mut peer).unwrap(),
                        Request::Hello { .. }
                    ));
                    write_response(
                        &mut peer,
                        &Response::Hello {
                            version: PROTOCOL_VERSION,
                            features: FEATURE_PRO_DIRECTIONS,
                        },
                    )
                    .unwrap();
                    let mut request = read_request(&mut peer).unwrap();
                    if request == Request::GetInfo {
                        write_response(&mut peer, &Response::Info(test_device_info())).unwrap();
                        request = read_request(&mut peer).unwrap();
                    }
                    let Request::OpenProDirection {
                        direction,
                        group_token,
                    } = request
                    else {
                        assert_eq!(request, Request::OpenPro);
                        write_response(&mut peer, &Response::Busy).unwrap();
                        return;
                    };
                    let index = usize::from(direction == PortDirection::Capture);
                    {
                        let mut owner = owner.lock().unwrap();
                        let mut opened = opened.lock().unwrap();
                        if owner.is_some_and(|token| token != group_token) || opened[index] {
                            write_response(&mut peer, &Response::Busy).unwrap();
                            return;
                        }
                        assert_ne!(group_token, [0; 2]);
                        *owner = Some(group_token);
                        opened[index] = true;
                    }
                    let region =
                        SharedRegion::create(64, u32::from(index == 0), u32::from(index == 1))
                            .unwrap();
                    let events: [OwnedFd; 3] = std::array::from_fn(|_| unsafe {
                        let fd = libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC);
                        assert!(fd >= 0);
                        OwnedFd::from_raw_fd(fd)
                    });
                    let session_id = index as u64 + 17;
                    send_open_response(
                        &mut peer,
                        &Response::OpenProDirection {
                            session_id,
                            direction,
                            shared: region.info(),
                        },
                        &[
                            region.fd(),
                            events[0].as_raw_fd(),
                            events[1].as_raw_fd(),
                            events[2].as_raw_fd(),
                        ],
                    );
                    let mut starts = 0;
                    loop {
                        let request = read_request(&mut peer).unwrap();
                        logs.lock().unwrap()[index].push(request.clone());
                        match request {
                            Request::Start { session_id: id } => {
                                assert_eq!(id, session_id);
                                region.set_lifecycle_generation(starts + 1);
                                region.reset_activation();
                                region.establish_activation(0);
                                region.set_cycle_sequence(1);
                                region.set_playback_sequence(1);
                                if index == 1 {
                                    let samples = std::array::from_fn::<_, 64, _>(|i| {
                                        starts as i32 * 100 + i as i32
                                    });
                                    assert!(region.try_publish_capture(&mut 0, 1, &samples));
                                }
                                // Descriptor 1 is capture; descriptor 2 is playback.
                                let event = events[index ^ 1].as_raw_fd();
                                let one = 1_u64;
                                assert_eq!(
                                    unsafe { libc::write(event, (&one as *const u64).cast(), 8) },
                                    8
                                );
                                starts += 1;
                            }
                            Request::Stop { session_id: id } => {
                                assert_eq!(id, session_id);
                                if index == 0 {
                                    let mut output = [0; 64];
                                    assert!(region.try_consume_playback(1, &mut output));
                                    assert_eq!(output[0], 42);
                                    assert!(output[1..].iter().all(|sample| *sample == 0));
                                }
                            }
                            Request::Close { session_id: id } => {
                                assert_eq!(id, session_id);
                                write_response(&mut peer, &Response::Ack).unwrap();
                                break;
                            }
                            _ => panic!("unexpected session request {request:?}"),
                        }
                        write_response(&mut peer, &Response::Ack).unwrap();
                    }
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        let socket = CString::new(path.to_str().unwrap()).unwrap();
        let result = unsafe {
            probe(
                _snd_pcm_sidealsa_open,
                socket.as_ptr(),
                capture_first,
                other_owner,
            )
        };
        assert_eq!(result, 0, "C probe failed at line {result}");
        server.join().unwrap();
        for (index, log) in logs.lock().unwrap().iter().enumerate() {
            let id = index as u64 + 17;
            assert_eq!(
                log,
                &[
                    Request::Start { session_id: id },
                    Request::Stop { session_id: id },
                    Request::Start { session_id: id },
                    Request::Stop { session_id: id },
                    Request::Close { session_id: id },
                ]
            );
        }
        std::fs::remove_file(path).unwrap();
    }
    unsafe {
        libc::dlclose(library);
    }
    std::fs::remove_file(library_path).unwrap();
}
