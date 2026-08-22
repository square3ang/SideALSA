mod shared;

pub use shared::{SharedError, SharedRegion};

use std::{
    io,
    mem::size_of,
    os::fd::{AsRawFd, RawFd},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

use sidealsa_protocol::{
    DeviceInfo, ErrorCode, PROTOCOL_VERSION, PortDirection, ProtocolError, Request, Response,
    SHARED_CLIENT_IDLE, SHARED_CLIENT_RUNNING, SHARED_CLIENT_STARTING, SharedRegionInfo, Stats,
    write_request,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("client I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("client protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("shared memory error: {0}")]
    Shared(#[from] SharedError),
    #[error("daemon reports PRO or SHARED resource busy")]
    Busy,
    #[error("daemon does not support requested operation")]
    Unsupported,
    #[error("daemon rejected request: {code:?}: {message}")]
    Daemon { code: ErrorCode, message: String },
    #[error("unexpected daemon response: {0:?}")]
    UnexpectedResponse(Response),
    #[error("expected 3 shared descriptors, received {0}")]
    InvalidDescriptorCount(usize),
    #[error("stream is closed")]
    Closed,
    #[error("stream is not started")]
    NotStarted,
    #[error("stream has no {0} direction")]
    MissingDirection(&'static str),
    #[error("audio buffer is not ready")]
    BufferNotReady,
    #[error("timed out waiting for audio period")]
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamMode {
    Pro,
    Shared(PortDirection),
}

pub struct SideAlsaClient {
    control: UnixStream,
    features: u32,
}

impl SideAlsaClient {
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let mut control = UnixStream::connect(path)?;
        write_request(
            &mut control,
            &Request::Hello {
                version: PROTOCOL_VERSION,
            },
        )?;
        let features = match sidealsa_protocol::read_response(&mut control)? {
            Response::Hello { version, features } if version == PROTOCOL_VERSION => features,
            response => return Err(response_error(response)),
        };
        Ok(Self { control, features })
    }

    pub fn connect_default() -> Result<Self, ClientError> {
        Self::connect("/tmp/sidealsad.sock")
    }

    pub fn features(&self) -> u32 {
        self.features
    }

    pub fn get_info(&mut self) -> Result<DeviceInfo, ClientError> {
        match request(&mut self.control, Request::GetInfo)? {
            Response::Info(info) => Ok(info),
            response => Err(response_error(response)),
        }
    }

    pub fn get_stats(&mut self) -> Result<Stats, ClientError> {
        match request(&mut self.control, Request::GetStats)? {
            Response::Stats(stats) => Ok(stats),
            response => Err(response_error(response)),
        }
    }

    pub fn open_pro(self) -> Result<AudioStream, ClientError> {
        let mut control = self.control;
        write_request(&mut control, &Request::OpenPro)?;
        let (response, fds) = receive_with_fds(&control)?;
        let (session_id, shared) = match response {
            Response::OpenPro { session_id, shared } => (session_id, shared),
            response => {
                close_fds(fds);
                return Err(response_error(response));
            }
        };
        AudioStream::from_parts(control, session_id, StreamMode::Pro, shared, fds)
    }

    pub fn open_shared(self, port_id: impl Into<String>) -> Result<AudioStream, ClientError> {
        let mut control = self.control;
        write_request(
            &mut control,
            &Request::OpenShared {
                port_id: port_id.into(),
            },
        )?;
        let (response, fds) = receive_with_fds(&control)?;
        let (session_id, direction, shared) = match response {
            Response::OpenShared {
                session_id,
                direction,
                shared,
            } => (session_id, direction, shared),
            response => {
                close_fds(fds);
                return Err(response_error(response));
            }
        };
        AudioStream::from_parts(
            control,
            session_id,
            StreamMode::Shared(direction),
            shared,
            fds,
        )
    }
}

pub struct AudioStream {
    control: UnixStream,
    session_id: u64,
    mode: StreamMode,
    info: SharedRegionInfo,
    region: SharedRegion,
    capture_event: EventFd,
    playback_event: EventFd,
    capture_index: usize,
    playback_index: usize,
    last_sequence: Option<u64>,
    lifecycle_generation: u64,
    started: bool,
    closed: bool,
}

impl AudioStream {
    fn from_parts(
        control: UnixStream,
        session_id: u64,
        mode: StreamMode,
        info: SharedRegionInfo,
        fds: Vec<RawFd>,
    ) -> Result<Self, ClientError> {
        if fds.len() != 3 {
            let count = fds.len();
            close_fds(fds);
            return Err(ClientError::InvalidDescriptorCount(count));
        }
        let region = match SharedRegion::map_fd(fds[0], info) {
            Ok(region) => region,
            Err(error) => {
                close_fd(fds[1]);
                close_fd(fds[2]);
                return Err(error.into());
            }
        };
        let lifecycle_generation = region.lifecycle_generation();
        Ok(Self {
            control,
            session_id,
            mode,
            info,
            region,
            capture_event: EventFd::from_raw(fds[1]),
            playback_event: EventFd::from_raw(fds[2]),
            capture_index: 0,
            playback_index: 0,
            last_sequence: None,
            lifecycle_generation,
            started: false,
            closed: false,
        })
    }

    pub fn mode(&self) -> StreamMode {
        self.mode
    }

    pub fn info(&self) -> SharedRegionInfo {
        self.info
    }

    pub fn cycle_sequence(&self) -> u64 {
        self.region.cycle_sequence()
    }

    pub fn start(&mut self) -> Result<(), ClientError> {
        self.ensure_open()?;
        if self.started {
            return Ok(());
        }
        self.region.reset_slots();
        self.capture_index = 0;
        self.playback_index = 0;
        self.last_sequence = None;
        self.lifecycle_generation = self.region.lifecycle_generation();
        self.capture_event.drain();
        self.playback_event.drain();
        match request(
            &mut self.control,
            Request::Start {
                session_id: self.session_id,
            },
        )? {
            Response::Ack => {
                self.region
                    .set_client_state(if self.info.playback_channels > 0 {
                        SHARED_CLIENT_STARTING
                    } else {
                        SHARED_CLIENT_RUNNING
                    });
                self.started = true;
                Ok(())
            }
            response => Err(response_error(response)),
        }
    }

    pub fn stop(&mut self) -> Result<(), ClientError> {
        self.ensure_open()?;
        if !self.started {
            return Ok(());
        }
        self.region.set_client_state(SHARED_CLIENT_IDLE);
        match request(
            &mut self.control,
            Request::Stop {
                session_id: self.session_id,
            },
        )? {
            Response::Ack => {
                self.started = false;
                self.region.reset_slots();
                self.capture_index = 0;
                self.playback_index = 0;
                self.last_sequence = None;
                self.capture_event.drain();
                self.playback_event.drain();
                Ok(())
            }
            response => Err(response_error(response)),
        }
    }

    pub fn prepare(&mut self) -> Result<(), ClientError> {
        self.ensure_open()?;
        if self.started {
            self.stop()?;
        }
        self.region.reset_slots();
        self.capture_index = 0;
        self.playback_index = 0;
        self.last_sequence = None;
        self.capture_event.drain();
        self.playback_event.drain();
        Ok(())
    }

    pub fn close(&mut self) -> Result<(), ClientError> {
        self.ensure_open()?;
        self.region.set_client_state(SHARED_CLIENT_IDLE);
        match request(
            &mut self.control,
            Request::Close {
                session_id: self.session_id,
            },
        )? {
            Response::Ack => {
                self.started = false;
                self.closed = true;
                self.region.reset_slots();
                self.capture_event.drain();
                self.playback_event.drain();
                Ok(())
            }
            response => Err(response_error(response)),
        }
    }

    pub fn get_stats(&mut self) -> Result<Stats, ClientError> {
        self.ensure_open()?;
        match request(&mut self.control, Request::GetStats)? {
            Response::Stats(stats) => Ok(stats),
            response => Err(response_error(response)),
        }
    }

    pub fn notification_fd(&self) -> Result<RawFd, ClientError> {
        let fd = if self.info.playback_channels > 0 {
            self.playback_event.fd
        } else if self.info.capture_channels > 0 {
            self.capture_event.fd
        } else {
            return Err(ClientError::MissingDirection("audio"));
        };
        let duplicate = unsafe { libc::dup(fd) };
        if duplicate < 0 {
            Err(io::Error::last_os_error().into())
        } else {
            Ok(duplicate)
        }
    }

    pub fn wait_period(&mut self, timeout: Duration) -> Result<u64, ClientError> {
        self.ensure_started()?;
        if self.info.playback_channels > 0 {
            self.playback_event.wait(timeout)?;
        } else if self.info.capture_channels > 0 {
            self.capture_event.wait(timeout)?;
        } else {
            return Err(ClientError::MissingDirection("audio"));
        }
        self.refresh_generation();
        let sequence = self.region.cycle_sequence();
        self.last_sequence = Some(sequence);
        Ok(sequence)
    }

    pub fn capture_buffer(&mut self, samples: &mut [i32]) -> Result<Option<u64>, ClientError> {
        self.ensure_started()?;
        if self.info.capture_channels == 0 {
            return Err(ClientError::MissingDirection("capture"));
        }
        let sequence = match self.last_sequence {
            Some(expected) => self.region.try_client_read_capture_at_least(
                &mut self.capture_index,
                expected,
                samples,
            ),
            None => self
                .region
                .try_client_read_capture(&mut self.capture_index, samples),
        };
        if sequence.is_some() {
            self.last_sequence = sequence;
        }
        Ok(sequence)
    }

    pub fn capture_buffer_for_sequence(
        &mut self,
        expected_sequence: u64,
        samples: &mut [i32],
    ) -> Result<Option<u64>, ClientError> {
        loop {
            let Some(sequence) = self.capture_buffer(samples)? else {
                return Ok(None);
            };
            if sequence >= expected_sequence {
                return Ok(Some(sequence));
            }
        }
    }

    pub fn playback_buffer(&mut self, samples: &[i32]) -> Result<bool, ClientError> {
        self.ensure_started()?;
        let sequence = self
            .last_sequence
            .ok_or(ClientError::BufferNotReady)?
            .wrapping_add(1);
        self.submit_playback(sequence, samples)
    }

    pub fn submit_playback(&mut self, sequence: u64, samples: &[i32]) -> Result<bool, ClientError> {
        self.ensure_started()?;
        if self.info.playback_channels == 0 {
            return Err(ClientError::MissingDirection("playback"));
        }
        Ok(self
            .region
            .try_client_publish_playback(&mut self.playback_index, sequence, samples))
    }

    fn ensure_open(&self) -> Result<(), ClientError> {
        if self.closed {
            Err(ClientError::Closed)
        } else {
            Ok(())
        }
    }

    fn ensure_started(&self) -> Result<(), ClientError> {
        self.ensure_open()?;
        if self.started {
            Ok(())
        } else {
            Err(ClientError::NotStarted)
        }
    }

    fn refresh_generation(&mut self) {
        let generation = self.region.lifecycle_generation();
        if generation != self.lifecycle_generation {
            self.lifecycle_generation = generation;
            self.capture_index = 0;
            self.playback_index = 0;
            self.last_sequence = None;
        }
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        if !self.closed {
            self.region.set_client_state(SHARED_CLIENT_IDLE);
            self.region.reset_slots();
            let _ = write_request(
                &mut self.control,
                &Request::Close {
                    session_id: self.session_id,
                },
            );
        }
    }
}

struct EventFd {
    fd: RawFd,
}

impl EventFd {
    fn from_raw(fd: RawFd) -> Self {
        Self { fd }
    }

    fn wait(&self, timeout: Duration) -> Result<(), ClientError> {
        let timeout = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut pollfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let result = unsafe { libc::poll(&mut pollfd, 1, timeout) };
            if result > 0 {
                if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(io::Error::from_raw_os_error(libc::EIO).into());
                }
                let mut value = 0_u64;
                loop {
                    let bytes = unsafe {
                        libc::read(self.fd, (&mut value as *mut u64).cast(), size_of::<u64>())
                    };
                    if bytes == size_of::<u64>() as isize {
                        return Ok(());
                    }
                    if bytes < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        if error.raw_os_error() == Some(libc::EAGAIN) {
                            break;
                        }
                        return Err(error.into());
                    }
                    return Err(
                        io::Error::new(io::ErrorKind::UnexpectedEof, "short eventfd read").into(),
                    );
                }
            }
            if result == 0 {
                return Err(ClientError::Timeout);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
    }

    fn drain(&self) {
        loop {
            let mut value = 0_u64;
            let bytes =
                unsafe { libc::read(self.fd, (&mut value as *mut u64).cast(), size_of::<u64>()) };
            if bytes == size_of::<u64>() as isize {
                continue;
            }
            if bytes < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
    }
}

impl Drop for EventFd {
    fn drop(&mut self) {
        close_fd(self.fd);
    }
}

fn request(stream: &mut UnixStream, request: Request) -> Result<Response, ClientError> {
    write_request(stream, &request)?;
    Ok(sidealsa_protocol::read_response(stream)?)
}

fn response_error(response: Response) -> ClientError {
    match response {
        Response::Busy => ClientError::Busy,
        Response::Unsupported => ClientError::Unsupported,
        Response::Error { code, message } => ClientError::Daemon { code, message },
        response => ClientError::UnexpectedResponse(response),
    }
}

fn receive_with_fds(stream: &UnixStream) -> Result<(Response, Vec<RawFd>), ClientError> {
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
    if received < 0 {
        return Err(io::Error::last_os_error().into());
    }

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
    Ok((
        sidealsa_protocol::decode_response(&bytes[..received as usize])?,
        fds,
    ))
}

fn close_fds(fds: Vec<RawFd>) {
    for fd in fds {
        close_fd(fd);
    }
}

fn close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::net::UnixStream,
        sync::mpsc,
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn client_error_maps_busy_and_unsupported() {
        assert!(matches!(response_error(Response::Busy), ClientError::Busy));
        assert!(matches!(
            response_error(Response::Unsupported),
            ClientError::Unsupported
        ));
    }

    #[test]
    fn client_handshakes_and_reads_device_info() {
        let path = std::env::temp_dir().join(format!(
            "sidealsa-client-test-{}-{}.sock",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        ));
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("listener should bind");
        let expected = DeviceInfo {
            name: "Test".into(),
            rate: 48000,
            period_size: 32,
            buffer_size: 64,
            playback_channels: 2,
            capture_channels: 2,
            playback_ports: Vec::new(),
            capture_ports: Vec::new(),
        };
        let server_info = expected.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client should connect");
            assert_eq!(
                sidealsa_protocol::read_request(&mut stream).expect("hello should arrive"),
                Request::Hello {
                    version: PROTOCOL_VERSION
                }
            );
            sidealsa_protocol::write_response(
                &mut stream,
                &Response::Hello {
                    version: PROTOCOL_VERSION,
                    features: 3,
                },
            )
            .expect("hello should write");
            assert_eq!(
                sidealsa_protocol::read_request(&mut stream).expect("info should arrive"),
                Request::GetInfo
            );
            sidealsa_protocol::write_response(&mut stream, &Response::Info(server_info))
                .expect("info should write");
        });

        let mut client = SideAlsaClient::connect(&path).expect("client should connect");
        assert_eq!(client.features(), 3);
        assert_eq!(client.get_info().expect("info should arrive"), expected);
        server.join().expect("server should not panic");
        fs::remove_file(path).expect("socket should remove");
    }

    #[test]
    fn playback_stream_waits_and_submits_next_sequence() {
        let server_region = SharedRegion::create(4, 2, 0).expect("region should create");
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let capture_fd = event_fd();
        let playback_fd = event_fd();
        let info = server_region.info();
        let (mut peer, control) = UnixStream::pair().expect("socket pair should create");
        let (ready_tx, ready_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let request = sidealsa_protocol::read_request(&mut peer).expect("start should arrive");
            assert_eq!(request, Request::Start { session_id: 9 });
            sidealsa_protocol::write_response(&mut peer, &Response::Ack)
                .expect("start ack should write");
            ready_tx.send(()).expect("test should be waiting");
            let request = sidealsa_protocol::read_request(&mut peer).expect("close should arrive");
            assert_eq!(request, Request::Close { session_id: 9 });
            sidealsa_protocol::write_response(&mut peer, &Response::Ack)
                .expect("close ack should write");
        });

        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Shared(PortDirection::Playback),
            info,
            vec![client_fd, capture_fd, playback_fd],
        )
        .expect("stream should create");
        stream.start().expect("stream should start");
        stream.start().expect("repeated start should be harmless");
        ready_rx.recv().expect("server should receive start");
        server_region.set_cycle_sequence(8);
        signal_event(playback_fd);
        signal_event(playback_fd);
        assert_eq!(
            stream
                .wait_period(Duration::from_millis(100))
                .expect("period should arrive"),
            8
        );
        assert!(
            stream
                .playback_buffer(&[1, 2, 3, 4, 5, 6, 7, 8])
                .expect("playback should submit")
        );
        let mut submitted = [0; 8];
        assert!(server_region.try_consume_playback(9, &mut submitted));
        assert_eq!(submitted, [1, 2, 3, 4, 5, 6, 7, 8]);
        stream.close().expect("stream should close");
        server.join().expect("server should not panic");
    }

    fn event_fd() -> RawFd {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(fd >= 0);
        fd
    }

    fn signal_event(fd: RawFd) {
        let value = 1_u64;
        let result = unsafe { libc::write(fd, (&value as *const u64).cast(), size_of::<u64>()) };
        assert_eq!(result, size_of::<u64>() as isize);
    }
}
