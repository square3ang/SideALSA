use std::{
    collections::HashMap,
    io::{self, Write},
    os::fd::{AsRawFd, RawFd},
    os::linux::fs::MetadataExt,
    os::unix::fs::{DirBuilderExt, FileTypeExt},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    ptr,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use sidealsa_protocol::{
    ErrorCode, FEATURE_PRO, FEATURE_PRO_DIRECTIONS, FEATURE_SHARED, PROTOCOL_VERSION,
    ProtocolError, Request, Response, write_response,
};
use thiserror::Error;

use crate::{DaemonState, OpenSharedError};

const MAX_CONTROL_CLIENTS: usize = 64;
// One uid cannot take every slot; the rest stay free for other users and root.
const MAX_CONTROL_CLIENTS_PER_UID: usize = 48;
const MAX_REQUESTS_BEFORE_HELLO: usize = 3;
const CONTROL_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
// handle_client keeps frames on the heap; mlockall pins every thread stack.
const CONTROL_THREAD_STACK: usize = 128 * 1024;

#[derive(Default)]
struct ClientSlots(Mutex<HashMap<libc::uid_t, usize>>);

struct ClientSlot {
    slots: Arc<ClientSlots>,
    uid: libc::uid_t,
}

impl ClientSlots {
    fn acquire(self: &Arc<Self>, uid: libc::uid_t) -> Option<ClientSlot> {
        let mut counts = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let total: usize = counts.values().sum();
        let mine = counts.get(&uid).copied().unwrap_or(0);
        if total >= MAX_CONTROL_CLIENTS || mine >= MAX_CONTROL_CLIENTS_PER_UID {
            return None;
        }
        counts.insert(uid, mine + 1);
        Some(ClientSlot {
            slots: Arc::clone(self),
            uid,
        })
    }
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        let mut counts = self.slots.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = counts.get_mut(&self.uid) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.uid);
            }
        }
    }
}

/// Closes the connection's session even when the control thread unwinds.
struct SessionGuard<'a> {
    state: &'a DaemonState,
    id: Option<u64>,
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        if let Some(session_id) = self.id {
            self.state.close(session_id);
        }
    }
}

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
    if let Some(parent) = path.parent() {
        prepare_socket_directory(parent)?;
    }
    remove_stale_socket(path)?;
    let listener = UnixListener::bind(path)?;
    let listener_metadata = std::fs::metadata(path)?;
    listener.set_nonblocking(true)?;
    let slots = Arc::new(ClientSlots::default());

    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let Some(slot) = peer_credentials(&stream)
                    .ok()
                    .and_then(|peer| slots.acquire(peer.uid))
                else {
                    continue;
                };
                let state = Arc::clone(&state);
                // A failed spawn drops the closure, which releases the slot.
                let _ = thread::Builder::new()
                    .name("sidealsa-control".into())
                    .stack_size(CONTROL_THREAD_STACK)
                    .spawn(move || {
                        let _slot = slot;
                        handle_client(stream, state);
                    });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if is_transient_accept_error(&error) => {
                // Descriptor/memory pressure must not take the hardware engine down.
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error.into()),
        }
    }

    if std::fs::metadata(path).is_ok_and(|metadata| {
        metadata.st_dev() == listener_metadata.st_dev()
            && metadata.st_ino() == listener_metadata.st_ino()
    }) {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// A root daemon serves microphones, so nobody else may be able to replace its
/// socket: the directory must be root-owned and writable only by root.
fn prepare_socket_directory(directory: &Path) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return std::fs::create_dir_all(directory);
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o750)
        .create(directory)?;
    let metadata = std::fs::symlink_metadata(directory)?;
    if !socket_directory_is_safe(metadata.is_dir(), metadata.st_uid(), metadata.st_mode()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "socket directory {} must be root-owned and writable only by root",
                directory.display()
            ),
        ));
    }
    Ok(())
}

fn socket_directory_is_safe(is_dir: bool, uid: u32, mode: u32) -> bool {
    is_dir && uid == 0 && mode & 0o022 == 0
}

fn is_transient_accept_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
    ) || matches!(
        error.raw_os_error(),
        Some(
            libc::EMFILE
                | libc::ENFILE
                | libc::ENOBUFS
                | libc::ENOMEM
                | libc::EPROTO
                | libc::EPERM
                | libc::ECONNABORTED
        )
    )
}

fn remove_stale_socket(path: &Path) -> Result<(), io::Error> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "control path exists and is not a Unix socket",
        ));
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another daemon is listening on the control socket",
        )),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            std::fs::remove_file(path)
        }
        Err(error) => Err(error),
    }
}

fn handle_client(mut stream: UnixStream, state: Arc<DaemonState>) {
    let Ok(peer) = peer_credentials(&stream) else {
        return;
    };
    if stream
        .set_read_timeout(Some(CONTROL_HANDSHAKE_TIMEOUT))
        .is_err()
        || stream
            .set_write_timeout(Some(CONTROL_HANDSHAKE_TIMEOUT))
            .is_err()
    {
        return;
    }
    let mut hello = false;
    let mut requests_before_hello = 0;
    let mut session = SessionGuard {
        state: &state,
        id: None,
    };

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
            Err(ProtocolError::UnsupportedVersion(version)) => {
                let _ = send_response(
                    &mut stream,
                    &Response::Error {
                        code: ErrorCode::InvalidRequest,
                        message: format!("unsupported client protocol version {version}"),
                    },
                    &[],
                );
                break;
            }
            // The frame was fully read, so the stream stays in sync.
            Err(ProtocolError::UnknownRequest(_)) => {
                if !hello {
                    requests_before_hello += 1;
                }
                if send_response(&mut stream, &Response::Unsupported, &[]).is_err()
                    || requests_before_hello >= MAX_REQUESTS_BEFORE_HELLO
                {
                    break;
                }
                continue;
            }
            Err(_) => break,
        };

        if !hello && !matches!(request, Request::Hello { .. }) {
            requests_before_hello += 1;
            // Each reply re-arms the read timeout, so cap the replies instead.
            if send_response(
                &mut stream,
                &Response::Error {
                    code: ErrorCode::BadState,
                    message: "HELLO required".into(),
                },
                &[],
            )
            .is_err()
                || requests_before_hello >= MAX_REQUESTS_BEFORE_HELLO
            {
                break;
            }
            continue;
        }

        let (response, fds) = match request {
            Request::Hello { version } => {
                if version != PROTOCOL_VERSION {
                    requests_before_hello += 1;
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
                            features: FEATURE_PRO
                                | FEATURE_SHARED
                                | FEATURE_PRO_DIRECTIONS
                                | sidealsa_protocol::FEATURE_PRO_ALIGNED_START,
                        },
                        Vec::new(),
                    )
                }
            }
            Request::GetInfo => (Response::Info(state.info()), Vec::new()),
            Request::OpenProDirection {
                direction,
                group_token,
            } => {
                if session.id.is_some() {
                    (
                        Response::Error {
                            code: ErrorCode::BadState,
                            message: "client already owns a session".into(),
                        },
                        Vec::new(),
                    )
                } else {
                    match state.open_pro_direction(
                        peer.pid as u32,
                        peer.uid,
                        group_token,
                        direction,
                    ) {
                        Ok(Some((session_id, shared, fds))) => {
                            session.id = Some(session_id);
                            (
                                Response::OpenProDirection {
                                    session_id,
                                    direction,
                                    shared,
                                },
                                fds.to_vec(),
                            )
                        }
                        Ok(None) => (Response::Busy, Vec::new()),
                        Err(error) => (
                            Response::Error {
                                code: ErrorCode::Internal,
                                message: error.to_string(),
                            },
                            Vec::new(),
                        ),
                    }
                }
            }
            Request::OpenPro => {
                if session.id.is_some() {
                    (
                        Response::Error {
                            code: ErrorCode::BadState,
                            message: "client already owns a session".into(),
                        },
                        Vec::new(),
                    )
                } else {
                    match state.open_pro() {
                        Ok(Some((session_id, shared, fds))) => {
                            session.id = Some(session_id);
                            (Response::OpenPro { session_id, shared }, fds.to_vec())
                        }
                        Ok(None) => (Response::Busy, Vec::new()),
                        Err(error) => (
                            Response::Error {
                                code: ErrorCode::Internal,
                                message: error.to_string(),
                            },
                            Vec::new(),
                        ),
                    }
                }
            }
            Request::OpenShared { port_id } => {
                if session.id.is_some() {
                    (
                        Response::Error {
                            code: ErrorCode::BadState,
                            message: "client already owns a session".into(),
                        },
                        Vec::new(),
                    )
                } else {
                    match state.open_shared(&port_id) {
                        Ok(Some(open)) => {
                            session.id = Some(open.session_id);
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
                        Err(error @ OpenSharedError::UnknownPort(_)) => (
                            Response::Error {
                                code: ErrorCode::InvalidRequest,
                                message: error.to_string(),
                            },
                            Vec::new(),
                        ),
                        Err(error @ OpenSharedError::Resources(_)) => (
                            Response::Error {
                                code: ErrorCode::Internal,
                                message: error.to_string(),
                            },
                            Vec::new(),
                        ),
                    }
                }
            }
            Request::Start { session_id } => {
                if session.id == Some(session_id) && state.start(session_id) {
                    (Response::Ack, Vec::new())
                } else {
                    (not_owner(), Vec::new())
                }
            }
            Request::StartProAligned { session_id } => {
                if session.id == Some(session_id) && state.start_pro_aligned(session_id) {
                    (Response::Ack, Vec::new())
                } else {
                    (not_owner(), Vec::new())
                }
            }
            Request::Stop { session_id } => {
                if session.id == Some(session_id) && state.stop(session_id) {
                    (Response::Ack, Vec::new())
                } else {
                    (not_owner(), Vec::new())
                }
            }
            Request::Close { session_id } => {
                if session.id == Some(session_id) && state.close(session_id) {
                    session.id = None;
                    (Response::Ack, Vec::new())
                } else {
                    (not_owner(), Vec::new())
                }
            }
            Request::GetStats => (Response::Stats(Box::new(state.stats())), Vec::new()),
        };

        // Clients without a session must not hold a slot forever.
        let read_timeout = if session.id.is_some() {
            None
        } else if hello {
            Some(CONTROL_IDLE_TIMEOUT)
        } else {
            Some(CONTROL_HANDSHAKE_TIMEOUT)
        };
        if requests_before_hello >= MAX_REQUESTS_BEFORE_HELLO
            || stream.set_read_timeout(read_timeout).is_err()
        {
            let _ = send_response(&mut stream, &response, &fds);
            break;
        }
        match send_response(&mut stream, &response, &fds) {
            Ok(()) => {}
            // Encoding failed before any byte was sent; report it and keep the session.
            Err(ControlError::Protocol(error)) if !matches!(error, ProtocolError::Io(_)) => {
                let failure = Response::Error {
                    code: ErrorCode::Internal,
                    message: format!("response could not be encoded: {error}"),
                };
                if send_response(&mut stream, &failure, &[]).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn peer_credentials(stream: &UnixStream) -> io::Result<libc::ucred> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the output buffer and its length describe a writable ucred.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<libc::ucred>() || credentials.pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid peer credentials",
        ));
    }
    Ok(credentials)
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
    #[test]
    fn socket_directory_must_be_root_only_writable() {
        use super::socket_directory_is_safe as safe;
        assert!(safe(true, 0, 0o750));
        assert!(!safe(true, 1000, 0o750));
        assert!(!safe(true, 0, 0o770));
        assert!(!safe(true, 0, 0o1777));
        assert!(!safe(false, 0, 0o750));
    }

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
            vec![0_u8; unsafe { libc::CMSG_SPACE((4 * size_of::<RawFd>()) as u32) } as usize];
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

    fn test_state() -> Arc<DaemonState> {
        Arc::new(
            DaemonState::new(
                &Profile::from_toml(PROFILE).unwrap(),
                Arc::new(HardwareTimeline::default()),
            )
            .unwrap(),
        )
    }

    #[test]
    fn unknown_request_keeps_the_open_session() {
        let state = test_state();
        let (mut client, server) = UnixStream::pair().unwrap();
        let server_state = Arc::clone(&state);
        let worker = thread::spawn(move || handle_client(server, server_state));
        assert!(matches!(
            request(
                &mut client,
                Request::Hello {
                    version: PROTOCOL_VERSION
                }
            ),
            Response::Hello { .. }
        ));
        let (response, fds) = request_with_fds(&mut client, Request::OpenPro);
        for fd in fds {
            unsafe { libc::close(fd) };
        }
        let Response::OpenPro { session_id, .. } = response else {
            panic!("unexpected response {response:?}");
        };

        // Bytes 6..8 of the frame header carry the request code.
        let mut frame = sidealsa_protocol::encode_request(&Request::GetInfo).unwrap();
        frame[6..8].copy_from_slice(&u16::MAX.to_le_bytes());
        client.write_all(&frame).unwrap();
        assert_eq!(read_response(&mut client).unwrap(), Response::Unsupported);
        assert!(state.owns(session_id));
        assert!(matches!(
            request(&mut client, Request::GetStats),
            Response::Stats(_)
        ));

        drop(client);
        worker.join().unwrap();
        assert!(!state.owns(session_id));
    }

    #[test]
    fn requests_before_hello_are_capped() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || handle_client(server, test_state()));
        for _ in 0..MAX_REQUESTS_BEFORE_HELLO {
            assert!(matches!(
                request(&mut client, Request::GetInfo),
                Response::Error {
                    code: ErrorCode::BadState,
                    ..
                }
            ));
        }
        worker.join().unwrap();
        let _ = write_request(&mut client, &Request::GetInfo);
        assert!(read_response(&mut client).is_err());
    }

    #[test]
    fn client_slots_cap_each_uid_and_release_on_drop() {
        let slots = Arc::new(ClientSlots::default());
        let held: Vec<_> = (0..MAX_CONTROL_CLIENTS_PER_UID)
            .map(|_| slots.acquire(1000).unwrap())
            .collect();
        assert!(slots.acquire(1000).is_none());
        let others: Vec<_> = (MAX_CONTROL_CLIENTS_PER_UID..MAX_CONTROL_CLIENTS)
            .map(|_| slots.acquire(1001).unwrap())
            .collect();
        assert!(slots.acquire(0).is_none());
        drop(held);
        assert!(slots.acquire(1000).is_some());
        drop(others);
        assert!(slots.0.lock().unwrap().is_empty());
    }

    #[test]
    fn directional_control_uses_kernel_credentials_and_connection_owned_ids() {
        use sidealsa_protocol::PortDirection;
        let state = Arc::new(
            DaemonState::new(
                &Profile::from_toml(PROFILE).unwrap(),
                Arc::new(HardwareTimeline::default()),
            )
            .unwrap(),
        );
        let (mut p, server_p) = UnixStream::pair().unwrap();
        let (mut c, server_c) = UnixStream::pair().unwrap();
        let credentials = peer_credentials(&server_p).unwrap();
        assert_eq!(credentials.pid as u32, std::process::id());
        assert_eq!(credentials.uid, unsafe { libc::getuid() });
        let p_state = Arc::clone(&state);
        let c_state = Arc::clone(&state);
        let p_thread = thread::spawn(move || handle_client(server_p, p_state));
        let c_thread = thread::spawn(move || handle_client(server_c, c_state));
        for stream in [&mut p, &mut c] {
            assert!(
                matches!(request(stream, Request::Hello { version: PROTOCOL_VERSION }), Response::Hello { version: PROTOCOL_VERSION, features } if features & FEATURE_PRO_DIRECTIONS != 0)
            );
        }
        let (response, fds) = request_with_fds(
            &mut p,
            Request::OpenProDirection {
                direction: PortDirection::Playback,
                group_token: [1, 2],
            },
        );
        let p_id = match response {
            Response::OpenProDirection {
                session_id,
                direction: PortDirection::Playback,
                shared,
            } => {
                assert_eq!((shared.playback_channels, shared.capture_channels), (2, 0));
                session_id
            }
            other => panic!("unexpected response {other:?}"),
        };
        assert_eq!(fds.len(), 4);
        for fd in fds {
            unsafe { libc::close(fd) };
        }
        assert_eq!(
            request(
                &mut c,
                Request::OpenProDirection {
                    direction: PortDirection::Capture,
                    group_token: [1, 3]
                }
            ),
            Response::Busy
        );
        let (response, fds) = request_with_fds(
            &mut c,
            Request::OpenProDirection {
                direction: PortDirection::Capture,
                group_token: [1, 2],
            },
        );
        let c_id = match response {
            Response::OpenProDirection {
                session_id,
                direction: PortDirection::Capture,
                shared,
            } => {
                assert_eq!((shared.playback_channels, shared.capture_channels), (0, 2));
                session_id
            }
            other => panic!("unexpected response {other:?}"),
        };
        assert_eq!(fds.len(), 4);
        for fd in fds {
            unsafe { libc::close(fd) };
        }
        for forbidden in [
            Request::Start { session_id: c_id },
            Request::StartProAligned { session_id: c_id },
            Request::Stop { session_id: c_id },
            Request::Close { session_id: c_id },
        ] {
            assert!(matches!(
                request(&mut p, forbidden),
                Response::Error {
                    code: ErrorCode::NotOwner,
                    ..
                }
            ));
        }
        assert_eq!(
            request(&mut p, Request::Start { session_id: p_id }),
            Response::Ack
        );
        assert_eq!(
            request(&mut c, Request::Start { session_id: c_id }),
            Response::Ack
        );
        drop(p);
        p_thread.join().unwrap();
        assert!(!state.owns(p_id));
        assert!(state.owns(c_id));
        assert!(state.open_pro().unwrap().is_none());
        assert_eq!(
            request(&mut c, Request::Stop { session_id: c_id }),
            Response::Ack
        );
        assert_eq!(
            request(&mut c, Request::Start { session_id: c_id }),
            Response::Ack
        );
        drop(c);
        c_thread.join().unwrap();
        assert!(!state.owns(c_id));
        assert!(state.open_pro().unwrap().is_some());
    }

    #[test]
    fn stale_socket_cleanup_preserves_non_socket_paths() {
        let path = unique_socket_path();
        fs::write(&path, b"not a socket").expect("test file should create");
        let error = remove_stale_socket(&path).expect_err("regular file must be preserved");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&path).expect("test file should remain"),
            b"not a socket"
        );
        fs::remove_file(&path).expect("test file should remove");

        let listener = UnixListener::bind(&path).expect("test socket should bind");
        drop(listener);
        remove_stale_socket(&path).expect("stale socket should remove");
        assert!(!path.exists());
    }

    #[test]
    fn stale_socket_cleanup_preserves_live_listener() {
        let path = unique_socket_path();
        let listener = UnixListener::bind(&path).expect("test socket should bind");
        let error = remove_stale_socket(&path).expect_err("live listener must be preserved");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(path.exists());
        drop(listener);
        fs::remove_file(path).expect("test socket should remove");
    }

    #[test]
    fn control_rejects_a_second_session_and_releases_pro_on_disconnect() {
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
        assert_eq!(fds.len(), 4);
        for fd in fds {
            unsafe { libc::close(fd) };
        }
        let session_id = match response {
            Response::OpenPro { session_id, .. } => session_id,
            response => panic!("unexpected response: {response:?}"),
        };
        assert!(matches!(
            request(
                &mut first,
                Request::OpenShared {
                    port_id: "line1".into()
                }
            ),
            Response::Error {
                code: ErrorCode::BadState,
                ..
            }
        ));

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
        assert_eq!(fds.len(), 4);
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

        drop(first);
        for _ in 0..100 {
            if !state.owns(session_id) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!state.owns(session_id), "disconnect should release PRO");

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
        assert_eq!(fds.len(), 4);
        for fd in fds {
            unsafe { libc::close(fd) };
        }
        drop(third);

        stop.store(true, Ordering::Release);
        listener.join().expect("listener should not panic");
        let _ = fs::remove_file(path);
    }
}
