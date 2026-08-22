use std::{
    io::{self, Write},
    os::fd::{AsRawFd, RawFd},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use sidealsa_protocol::{
    ErrorCode, FEATURE_PRO, FEATURE_SHARED, PROTOCOL_VERSION, ProtocolError, Request, Response,
    write_response,
};
use thiserror::Error;

use crate::DaemonState;

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("control socket I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("control protocol error: {0}")]
    Protocol(#[from] ProtocolError),
}

pub fn run_control_listener(
    path: &Path,
    state: Arc<DaemonState>,
    stop: Arc<AtomicBool>,
) -> Result<(), ControlError> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;

    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let state = Arc::clone(&state);
                thread::spawn(move || handle_client(stream, state));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }

    let _ = std::fs::remove_file(path);
    Ok(())
}

fn handle_client(mut stream: UnixStream, state: Arc<DaemonState>) {
    let mut hello = false;
    let mut session = None;

    loop {
        let request = match sidealsa_protocol::read_request(&mut stream) {
            Ok(request) => request,
            Err(ProtocolError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                break;
            }
            Err(_) => break,
        };

        if !hello && !matches!(request, Request::Hello { .. }) {
            if send_response(
                &mut stream,
                &Response::Error {
                    code: ErrorCode::BadState,
                    message: "HELLO required".into(),
                },
                &[],
            )
            .is_err()
            {
                break;
            }
            continue;
        }

        let (response, fds) = match request {
            Request::Hello { version } => {
                if version != PROTOCOL_VERSION {
                    (
                        Response::Error {
                            code: ErrorCode::InvalidRequest,
                            message: format!("unsupported client version {version}"),
                        },
                        Vec::new(),
                    )
                } else {
                    hello = true;
                    (
                        Response::Hello {
                            version: PROTOCOL_VERSION,
                            features: FEATURE_PRO | FEATURE_SHARED,
                        },
                        Vec::new(),
                    )
                }
            }
            Request::GetInfo => (Response::Info(state.info()), Vec::new()),
            Request::OpenPro => {
                if session.is_some() {
                    (
                        Response::Error {
                            code: ErrorCode::BadState,
                            message: "client already owns a session".into(),
                        },
                        Vec::new(),
                    )
                } else if let Some((session_id, shared, fds)) = state.open_pro() {
                    session = Some(session_id);
                    (Response::OpenPro { session_id, shared }, fds.to_vec())
                } else {
                    (Response::Busy, Vec::new())
                }
            }
            Request::OpenShared { port_id } => match state.open_shared(&port_id) {
                Ok(Some(open)) => {
                    session = Some(open.session_id);
                    (
                        Response::OpenShared {
                            session_id: open.session_id,
                            direction: open.direction,
                            shared: open.shared,
                        },
                        open.fds.to_vec(),
                    )
                }
                Ok(None) => (Response::Busy, Vec::new()),
                Err(error) => (
                    Response::Error {
                        code: ErrorCode::InvalidRequest,
                        message: error.to_string(),
                    },
                    Vec::new(),
                ),
            },
            Request::Start { session_id } => {
                if session == Some(session_id) && state.start(session_id) {
                    (Response::Ack, Vec::new())
                } else {
                    (not_owner(), Vec::new())
                }
            }
            Request::Stop { session_id } => {
                if session == Some(session_id) && state.stop(session_id) {
                    (Response::Ack, Vec::new())
                } else {
                    (not_owner(), Vec::new())
                }
            }
            Request::Close { session_id } => {
                if session == Some(session_id) && state.close(session_id) {
                    session = None;
                    (Response::Ack, Vec::new())
                } else {
                    (not_owner(), Vec::new())
                }
            }
            Request::GetStats => (Response::Stats(state.stats()), Vec::new()),
        };

        if send_response(&mut stream, &response, &fds).is_err() {
            break;
        }
    }

    if let Some(session_id) = session {
        state.close(session_id);
    }
}

fn not_owner() -> Response {
    Response::Error {
        code: ErrorCode::NotOwner,
        message: "session is not PRO owner".into(),
    }
}

fn send_response(
    stream: &mut UnixStream,
    response: &Response,
    fds: &[RawFd],
) -> Result<(), ControlError> {
    if fds.is_empty() {
        write_response(stream, response)?;
        return Ok(());
    }
    let bytes = sidealsa_protocol::encode_response(response)?;
    send_with_fds(stream, &bytes, fds)?;
    Ok(())
}

fn send_with_fds(stream: &mut UnixStream, bytes: &[u8], fds: &[RawFd]) -> Result<(), io::Error> {
    let fd_bytes = fds
        .len()
        .checked_mul(std::mem::size_of::<RawFd>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "too many file descriptors"))?;
    let control_len = unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize };
    let cmsg_len = unsafe { libc::CMSG_LEN(fd_bytes as u32) as usize };
    let mut control = vec![0_u8; control_len];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&message);
        if cmsg.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "could not create control message",
            ));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = cmsg_len;
        ptr::copy_nonoverlapping(
            fds.as_ptr(),
            libc::CMSG_DATA(cmsg).cast::<RawFd>(),
            fds.len(),
        );
    }

    let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    let sent = sent as usize;
    if sent < bytes.len() {
        stream.write_all(&bytes[sent..])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        mem::size_of,
        os::fd::{AsRawFd, RawFd},
        sync::atomic::AtomicBool,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use sidealsa_config::Profile;
    use sidealsa_core::HardwareTimeline;
    use sidealsa_protocol::{Request, decode_response, read_response, write_request};

    const PROFILE: &str = r#"
        [device]
        name = "Test"
        rate = 48000
        period_size = 32
        buffer_size = 64

        [device.playback]
        device = "hw:Test,0"
        channels = 2
        format = "S32_LE"

        [device.capture]
        device = "hw:Test,0"
        channels = 2
        format = "S32_LE"

        [[ports.playback]]
        id = "line1"
        name = "Line 1"
        channels = [0, 1]

        [[ports.capture]]
        id = "mic1"
        name = "Mic 1"
        channels = [0]
    "#;

    fn unique_socket_path() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "sidealsad-test-{}-{nanos}.sock",
            std::process::id()
        ))
    }

    fn connect(path: &Path) -> UnixStream {
        for _ in 0..100 {
            if let Ok(stream) = UnixStream::connect(path) {
                return stream;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("control socket did not appear");
    }

    fn request(stream: &mut UnixStream, request: Request) -> Response {
        write_request(stream, &request).expect("request should write");
        read_response(stream).expect("response should read")
    }

    fn request_with_fds(stream: &mut UnixStream, request: Request) -> (Response, Vec<RawFd>) {
        write_request(stream, &request).expect("request should write");
        let mut bytes = vec![0_u8; 4096];
        let mut control =
            vec![0_u8; unsafe { libc::CMSG_SPACE((3 * size_of::<RawFd>()) as u32) } as usize];
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len();
        let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) };
        assert!(received > 0, "response should arrive");

        let mut fds = Vec::new();
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&message);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let payload_bytes = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    let count = payload_bytes / size_of::<RawFd>();
                    let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                    for index in 0..count {
                        fds.push(*data.add(index));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&message, cmsg);
            }
        }
        let response =
            decode_response(&bytes[..received as usize]).expect("response should decode");
        (response, fds)
    }

    #[test]
    fn control_enforces_exclusive_pro_owner() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let state = Arc::new(
            DaemonState::new(&profile, Arc::new(HardwareTimeline::default()))
                .expect("state should create"),
        );
        let path = unique_socket_path();
        let stop = Arc::new(AtomicBool::new(false));
        let listener_state = Arc::clone(&state);
        let listener_stop = Arc::clone(&stop);
        let listener_path = path.clone();
        let listener = thread::spawn(move || {
            run_control_listener(&listener_path, listener_state, listener_stop)
                .expect("listener should run")
        });

        let mut first = connect(&path);
        let hello = request(
            &mut first,
            Request::Hello {
                version: PROTOCOL_VERSION,
            },
        );
        assert!(
            matches!(hello, Response::Hello { features, .. } if features & FEATURE_SHARED != 0)
        );
        let (response, fds) = request_with_fds(&mut first, Request::OpenPro);
        assert_eq!(fds.len(), 3);
        for fd in fds {
            unsafe { libc::close(fd) };
        }
        let session_id = match response {
            Response::OpenPro { session_id, .. } => session_id,
            response => panic!("unexpected response: {response:?}"),
        };

        let mut second = connect(&path);
        assert!(matches!(
            request(
                &mut second,
                Request::Hello {
                    version: PROTOCOL_VERSION
                }
            ),
            Response::Hello { .. }
        ));
        assert!(matches!(
            request(&mut second, Request::OpenPro),
            Response::Busy
        ));
        drop(second);

        let mut shared = connect(&path);
        assert!(matches!(
            request(
                &mut shared,
                Request::Hello {
                    version: PROTOCOL_VERSION
                }
            ),
            Response::Hello { .. }
        ));
        let (response, fds) = request_with_fds(
            &mut shared,
            Request::OpenShared {
                port_id: "line1".into(),
            },
        );
        let shared_session_id = match response {
            Response::OpenShared {
                session_id,
                direction: sidealsa_protocol::PortDirection::Playback,
                ..
            } => session_id,
            response => panic!("unexpected response: {response:?}"),
        };
        assert_eq!(fds.len(), 3);
        for fd in fds {
            unsafe { libc::close(fd) };
        }
        assert!(matches!(
            request(
                &mut shared,
                Request::Start {
                    session_id: shared_session_id
                }
            ),
            Response::Ack
        ));
        assert!(matches!(
            request(
                &mut shared,
                Request::Close {
                    session_id: shared_session_id
                }
            ),
            Response::Ack
        ));
        drop(shared);

        assert!(matches!(
            request(&mut first, Request::Start { session_id }),
            Response::Ack
        ));
        assert!(matches!(
            request(&mut first, Request::Stop { session_id }),
            Response::Ack
        ));
        assert!(matches!(
            request(&mut first, Request::Close { session_id }),
            Response::Ack
        ));
        drop(first);

        let mut third = connect(&path);
        assert!(matches!(
            request(
                &mut third,
                Request::Hello {
                    version: PROTOCOL_VERSION
                }
            ),
            Response::Hello { .. }
        ));
        let (response, fds) = request_with_fds(&mut third, Request::OpenPro);
        assert!(matches!(response, Response::OpenPro { .. }));
        assert_eq!(fds.len(), 3);
        for fd in fds {
            unsafe { libc::close(fd) };
        }
        drop(third);

        stop.store(true, Ordering::Release);
        listener.join().expect("listener should not panic");
        let _ = fs::remove_file(path);
    }
}
