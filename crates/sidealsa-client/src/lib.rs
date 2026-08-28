mod shared;

pub use shared::{PlaybackConsume, SharedError, SharedRegion};

use std::{
    io::{self, Read},
    mem::size_of,
    net::Shutdown,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

use sidealsa_protocol::{
    DeviceInfo, ErrorCode, MAX_FRAME_PAYLOAD, PROTOCOL_MAGIC, PROTOCOL_VERSION, PortDirection,
    ProtocolError, Request, Response, SHARED_CLIENT_IDLE, SharedRegionInfo, Stats, write_request,
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
    UnexpectedResponse(Box<Response>),
    #[error("expected 4 shared descriptors, received {0}")]
    InvalidDescriptorCount(usize),
    #[error("stream is closed")]
    Closed,
    #[error("stream is not started")]
    NotStarted,
    #[error("stream has no {0} direction")]
    MissingDirection(&'static str),
    #[error("audio buffer is not ready")]
    BufferNotReady,
    #[error("shared capture stream lost buffered periods")]
    CaptureDiscontinuity,
    #[error("timed out waiting for audio period")]
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamMode {
    Pro,
    Shared(PortDirection),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerCredentials {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

pub struct SideAlsaClient {
    control: UnixStream,
    features: u32,
}

impl SideAlsaClient {
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        Self::connect_inner(path.as_ref(), None)
    }

    pub fn connect_with_timeout(
        path: impl AsRef<Path>,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        Self::connect_inner(path.as_ref(), Some(timeout))
    }

    fn connect_inner(path: &Path, timeout: Option<Duration>) -> Result<Self, ClientError> {
        let mut control = UnixStream::connect(path)?;
        control.set_read_timeout(timeout)?;
        control.set_write_timeout(timeout)?;
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

    pub fn peer_credentials(&self) -> Result<PeerCredentials, ClientError> {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = size_of::<libc::ucred>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                self.control.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(PeerCredentials {
            pid: u32::try_from(credentials.pid)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid daemon PID"))?,
            uid: credentials.uid,
            gid: credentials.gid,
        })
    }

    pub fn peer_pid(&self) -> Result<u32, ClientError> {
        Ok(self.peer_credentials()?.pid)
    }

    pub fn get_info(&mut self) -> Result<DeviceInfo, ClientError> {
        match request(&mut self.control, Request::GetInfo)? {
            Response::Info(info) => Ok(info),
            response => Err(response_error(response)),
        }
    }

    pub fn get_stats(&mut self) -> Result<Stats, ClientError> {
        match request(&mut self.control, Request::GetStats)? {
            Response::Stats(stats) => Ok(*stats),
            response => Err(response_error(response)),
        }
    }

    pub fn open_pro(self) -> Result<AudioStream, ClientError> {
        let mut control = self.control;
        write_request(&mut control, &Request::OpenPro)?;
        let (response, fds) = receive_with_fds(&control)?;
        let (session_id, shared) = match response {
            Response::OpenPro { session_id, shared } => (session_id, shared),
            response => return Err(response_error(response)),
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
            response => return Err(response_error(response)),
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
    playback_ready_event: EventFd,
    capture_index: usize,
    playback_index: usize,
    last_sequence: Option<u64>,
    lifecycle_generation: u64,
    hardware_generation: u64,
    capture_discontinuities: u64,
    started: bool,
    closed: bool,
}

impl AudioStream {
    fn from_parts(
        control: UnixStream,
        session_id: u64,
        mode: StreamMode,
        info: SharedRegionInfo,
        fds: Vec<OwnedFd>,
    ) -> Result<Self, ClientError> {
        let [
            region_fd,
            capture_event,
            playback_event,
            playback_ready_event,
        ]: [OwnedFd; 4] = fds
            .try_into()
            .map_err(|fds: Vec<OwnedFd>| ClientError::InvalidDescriptorCount(fds.len()))?;
        let region = SharedRegion::map_fd(region_fd.into_raw_fd(), info)?;
        let lifecycle_generation = region.lifecycle_generation();
        let hardware_generation = region.hardware_generation();
        let capture_discontinuities = region.capture_discontinuities();
        Ok(Self {
            control,
            session_id,
            mode,
            info,
            region,
            capture_event: EventFd::from_owned(capture_event),
            playback_event: EventFd::from_owned(playback_event),
            playback_ready_event: EventFd::from_owned(playback_ready_event),
            capture_index: 0,
            playback_index: 0,
            last_sequence: None,
            lifecycle_generation,
            hardware_generation,
            capture_discontinuities,
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

    pub fn playback_sequence(&self) -> u64 {
        self.region.playback_sequence()
    }

    pub fn hardware_generation(&self) -> u64 {
        self.region.hardware_generation()
    }

    pub fn activation_sequence(&self) -> Option<u64> {
        self.region
            .activation_ready()
            .then(|| self.region.start_sequence())
    }

    pub fn record_realtime_failure(&self) {
        self.region.record_client_realtime_failure();
    }

    pub fn record_callback_timing(&self, duration_nanos: u64, period_nanos: u64) {
        self.region
            .record_client_callback_timing(duration_nanos, period_nanos);
    }

    pub fn record_expired_playback_periods(&self, periods: u64) {
        self.region.record_client_expired_playback_periods(periods);
    }

    pub fn record_playback_xrun(&self) {
        self.region.record_client_playback_xrun();
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
        self.hardware_generation = self.region.hardware_generation();
        self.capture_discontinuities = self.region.capture_discontinuities();
        self.capture_event.drain();
        self.playback_event.drain();
        self.playback_ready_event.drain();
        match self.control_request(Request::Start {
            session_id: self.session_id,
        })? {
            Response::Ack => {
                self.started = true;
                Ok(())
            }
            response => Err(self.control_response_error(response)),
        }
    }

    pub fn stop(&mut self) -> Result<(), ClientError> {
        self.ensure_open()?;
        if !self.started {
            return Ok(());
        }
        self.region.set_client_state(SHARED_CLIENT_IDLE);
        match self.control_request(Request::Stop {
            session_id: self.session_id,
        })? {
            Response::Ack => {
                self.started = false;
                self.region.reset_slots();
                self.capture_index = 0;
                self.playback_index = 0;
                self.last_sequence = None;
                self.capture_discontinuities = self.region.capture_discontinuities();
                self.capture_event.drain();
                self.playback_event.drain();
                self.playback_ready_event.drain();
                Ok(())
            }
            response => Err(self.control_response_error(response)),
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
        self.capture_discontinuities = self.region.capture_discontinuities();
        self.capture_event.drain();
        self.playback_event.drain();
        self.playback_ready_event.drain();
        Ok(())
    }

    pub fn close(&mut self) -> Result<(), ClientError> {
        self.ensure_open()?;
        self.region.set_client_state(SHARED_CLIENT_IDLE);
        match self.control_request(Request::Close {
            session_id: self.session_id,
        })? {
            Response::Ack => {
                self.started = false;
                self.closed = true;
                Ok(())
            }
            response => Err(self.control_response_error(response)),
        }
    }

    pub fn get_stats(&mut self) -> Result<Stats, ClientError> {
        self.ensure_open()?;
        match self.control_request(Request::GetStats)? {
            Response::Stats(stats) => Ok(*stats),
            response => Err(self.control_response_error(response)),
        }
    }

    pub fn notification_fd(&self) -> Result<RawFd, ClientError> {
        let fd = if self.info.capture_channels > 0 {
            self.capture_event.as_raw_fd()
        } else if self.info.playback_channels > 0 {
            self.playback_event.as_raw_fd()
        } else {
            return Err(ClientError::MissingDirection("audio"));
        };
        Ok(duplicate_cloexec(fd)?)
    }

    pub fn control_fd(&self) -> Result<RawFd, ClientError> {
        Ok(duplicate_cloexec(self.control.as_raw_fd())?)
    }

    pub fn wait_period(&mut self, timeout: Duration) -> Result<u64, ClientError> {
        self.ensure_started()?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            self.refresh_generation();
            self.check_hardware_generation()?;
            self.check_capture_discontinuity()?;
            if self.info.capture_channels > 0 {
                let ready_sequence = match self.mode {
                    StreamMode::Pro => self
                        .region
                        .oldest_valid_client_capture_sequence(self.region.playback_sequence()),
                    StreamMode::Shared(_) => {
                        self.region.next_client_capture_sequence(self.capture_index)
                    }
                };
                if let Some(sequence) = ready_sequence
                    && self.last_sequence != Some(sequence)
                {
                    self.capture_event.drain();
                    if self.region.has_multiple_ready_capture_slots() {
                        self.capture_event.notify()?;
                    }
                    self.last_sequence = Some(sequence);
                    return Ok(sequence);
                }
                let wait = self
                    .capture_event
                    .wait_until(self.control.as_raw_fd(), deadline);
                if matches!(&wait, Err(ClientError::Closed)) {
                    self.poison_control();
                }
                if wait? == EventWait::Spurious {
                    continue;
                }
            } else if self.info.playback_channels > 0 {
                let wait = self
                    .playback_event
                    .wait_until(self.control.as_raw_fd(), deadline);
                if matches!(&wait, Err(ClientError::Closed)) {
                    self.poison_control();
                }
                if wait? == EventWait::Spurious {
                    continue;
                }
            } else {
                return Err(ClientError::MissingDirection("audio"));
            }
            self.refresh_generation();
            self.check_hardware_generation()?;
            self.check_capture_discontinuity()?;
            if self.info.capture_channels > 0 {
                continue;
            }
            let sequence = self.region.cycle_sequence();
            if self.last_sequence == Some(sequence) {
                continue;
            }
            self.last_sequence = Some(sequence);
            return Ok(sequence);
        }
    }

    pub fn capture_buffer(&mut self, samples: &mut [i32]) -> Result<Option<u64>, ClientError> {
        self.ensure_started()?;
        self.refresh_generation();
        self.check_hardware_generation()?;
        self.check_capture_discontinuity()?;
        if self.info.capture_channels == 0 {
            return Err(ClientError::MissingDirection("capture"));
        }
        let sequence = match self.mode {
            StreamMode::Pro => loop {
                let minimum_sequence = self.region.playback_sequence();
                let sequence = self.region.try_client_read_valid_capture(
                    &mut self.capture_index,
                    minimum_sequence,
                    samples,
                );
                let Some(sequence) = sequence else {
                    break None;
                };
                if sequence_is_before(sequence, self.region.playback_sequence()) {
                    self.region.record_client_expired_capture_block();
                    continue;
                }
                break Some(sequence);
            },
            _ => self
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
            if !sequence_is_before(sequence, expected_sequence) {
                return Ok(Some(sequence));
            }
        }
    }

    pub fn playback_buffer(&mut self, samples: &[i32]) -> Result<bool, ClientError> {
        self.ensure_started()?;
        let sequence = self.last_sequence.ok_or(ClientError::BufferNotReady)?;
        let sequence = match self.mode {
            StreamMode::Pro => sequence,
            StreamMode::Shared(_) => sequence.wrapping_add(1),
        };
        self.submit_playback(sequence, samples)
    }

    pub fn submit_playback(&mut self, sequence: u64, samples: &[i32]) -> Result<bool, ClientError> {
        self.ensure_started()?;
        if self.info.playback_channels == 0 {
            return Err(ClientError::MissingDirection("playback"));
        }
        let published =
            self.region
                .try_client_publish_playback(&mut self.playback_index, sequence, samples);
        if published && matches!(self.mode, StreamMode::Pro) {
            self.playback_ready_event.notify()?;
        }
        Ok(published)
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

    fn control_request(&mut self, control_request: Request) -> Result<Response, ClientError> {
        match request(&mut self.control, control_request) {
            Ok(response) => Ok(response),
            Err(error) => {
                self.poison_control();
                Err(error)
            }
        }
    }

    fn control_response_error(&mut self, response: Response) -> ClientError {
        let error = response_error(response);
        if matches!(error, ClientError::UnexpectedResponse(_)) {
            self.poison_control();
        }
        error
    }

    fn poison_control(&mut self) {
        self.started = false;
        self.closed = true;
        let _ = self.control.shutdown(Shutdown::Both);
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

    fn check_capture_discontinuity(&mut self) -> Result<(), ClientError> {
        if self.info.capture_channels == 0 {
            return Ok(());
        }
        let current = self.region.capture_discontinuities();
        if current == self.capture_discontinuities {
            return Ok(());
        }
        self.capture_discontinuities = current;
        self.capture_index = 0;
        self.last_sequence = None;
        self.capture_event.drain();
        Err(ClientError::CaptureDiscontinuity)
    }

    fn check_hardware_generation(&mut self) -> Result<(), ClientError> {
        let current = self.region.hardware_generation();
        if current == self.hardware_generation {
            return Ok(());
        }
        self.hardware_generation = current;
        self.capture_index = 0;
        self.playback_index = 0;
        self.last_sequence = None;
        self.capture_event.drain();
        self.playback_event.drain();
        self.playback_ready_event.drain();
        Err(ClientError::CaptureDiscontinuity)
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        if !self.closed {
            self.region.set_client_state(SHARED_CLIENT_IDLE);
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
    fd: OwnedFd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventWait {
    Notified,
    Spurious,
}

impl EventFd {
    fn from_owned(fd: OwnedFd) -> Self {
        Self { fd }
    }

    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    fn wait_until(&self, control_fd: RawFd, deadline: Instant) -> Result<EventWait, ClientError> {
        let mut pollfds = [
            libc::pollfd {
                fd: self.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: control_fd,
                events: libc::POLLIN | libc::POLLRDHUP,
                revents: 0,
            },
        ];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = if remaining.is_zero() {
                0
            } else {
                remaining
                    .as_millis()
                    .saturating_add(u128::from(
                        !remaining.subsec_nanos().is_multiple_of(1_000_000),
                    ))
                    .min(i32::MAX as u128) as i32
            };
            pollfds[0].revents = 0;
            pollfds[1].revents = 0;
            let result =
                unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, timeout) };
            if result > 0 {
                if control_is_closed(control_fd, pollfds[1].revents)? {
                    return Err(ClientError::Closed);
                }
                if pollfds[0].revents != 0 {
                    return self.consume_ready(pollfds[0].revents);
                }
                if pollfds[1].revents & libc::POLLIN != 0 {
                    // No control response is expected while waiting for audio. Keep watching
                    // hangup/error without spinning on unexpected readable data.
                    pollfds[1].events = libc::POLLRDHUP;
                }
                continue;
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

    fn consume_ready(&self, revents: libc::c_short) -> Result<EventWait, ClientError> {
        if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::from_raw_os_error(libc::EIO).into());
        }
        if revents & libc::POLLIN == 0 {
            return Ok(EventWait::Spurious);
        }
        let mut value = 0_u64;
        loop {
            let bytes = unsafe {
                libc::read(
                    self.as_raw_fd(),
                    (&mut value as *mut u64).cast(),
                    size_of::<u64>(),
                )
            };
            if bytes == size_of::<u64>() as isize {
                return Ok(EventWait::Notified);
            }
            if bytes < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.raw_os_error() == Some(libc::EAGAIN) {
                    return Ok(EventWait::Spurious);
                }
                return Err(error.into());
            }
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short eventfd read").into());
        }
    }

    fn notify(&self) -> Result<(), ClientError> {
        let value = 1_u64;
        loop {
            let bytes = unsafe {
                libc::write(
                    self.as_raw_fd(),
                    (&value as *const u64).cast(),
                    size_of::<u64>(),
                )
            };
            if bytes == size_of::<u64>() as isize {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
    }

    fn drain(&self) {
        loop {
            let mut value = 0_u64;
            let bytes = unsafe {
                libc::read(
                    self.as_raw_fd(),
                    (&mut value as *mut u64).cast(),
                    size_of::<u64>(),
                )
            };
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

fn control_is_closed(fd: RawFd, revents: libc::c_short) -> Result<bool, ClientError> {
    if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL | libc::POLLRDHUP) != 0 {
        return Ok(true);
    }
    if revents & libc::POLLIN == 0 {
        return Ok(false);
    }
    let mut byte = 0_u8;
    loop {
        let received = unsafe {
            libc::recv(
                fd,
                (&mut byte as *mut u8).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if received == 0 {
            return Ok(true);
        }
        if received > 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.raw_os_error() == Some(libc::EAGAIN) {
            return Ok(false);
        }
        if matches!(
            error.raw_os_error(),
            Some(libc::ECONNRESET) | Some(libc::ENOTCONN)
        ) {
            return Ok(true);
        }
        return Err(error.into());
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
        response => ClientError::UnexpectedResponse(Box::new(response)),
    }
}

fn sequence_is_before(candidate: u64, reference: u64) -> bool {
    let distance = reference.wrapping_sub(candidate);
    distance != 0 && distance < (1_u64 << 63)
}

fn receive_with_fds(stream: &UnixStream) -> Result<(Response, Vec<OwnedFd>), ClientError> {
    const FRAME_HEADER_SIZE: usize = 12;
    const MAX_DESCRIPTORS: usize = 4;

    let mut first_byte = [0_u8; 1];
    let control_len =
        unsafe { libc::CMSG_SPACE((MAX_DESCRIPTORS * size_of::<RawFd>()) as u32) as usize };
    let mut control = vec![0_usize; control_len.div_ceil(size_of::<usize>())];
    let mut fds = Vec::with_capacity(MAX_DESCRIPTORS);
    let mut iov = libc::iovec {
        iov_base: first_byte.as_mut_ptr().cast(),
        iov_len: first_byte.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    let received = loop {
        message.msg_controllen = control_len;
        message.msg_flags = 0;
        let received =
            unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
        if received >= 0 {
            break received;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    };

    let mut malformed_control = false;
    unsafe {
        let control_start = message.msg_control as usize;
        let control_end = control_start.saturating_add(message.msg_controllen);
        let mut cmsg = libc::CMSG_FIRSTHDR(&message);
        while !cmsg.is_null() {
            let cmsg_start = cmsg as usize;
            let cmsg_len = (*cmsg).cmsg_len as usize;
            let header_len = libc::CMSG_LEN(0) as usize;
            let Some(cmsg_end) = cmsg_start.checked_add(cmsg_len) else {
                malformed_control = true;
                break;
            };
            if cmsg_len < header_len || cmsg_start < control_start || cmsg_end > control_end {
                malformed_control = true;
                break;
            }
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let payload_bytes = cmsg_len - header_len;
                malformed_control |= !payload_bytes.is_multiple_of(size_of::<RawFd>());
                let count = payload_bytes / size_of::<RawFd>();
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for index in 0..count {
                    fds.push(OwnedFd::from_raw_fd(*data.add(index)));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&message, cmsg);
        }
    }

    if malformed_control
        || message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        || received != 1
    {
        return Err(ProtocolError::MalformedPayload.into());
    }

    let mut header = [0_u8; FRAME_HEADER_SIZE];
    header[0] = first_byte[0];
    let mut reader = stream;
    reader
        .read_exact(&mut header[1..])
        .map_err(ProtocolError::from)?;
    if header[..4] != PROTOCOL_MAGIC {
        return Err(ProtocolError::InvalidMagic.into());
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version).into());
    }
    let payload_len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    if payload_len > MAX_FRAME_PAYLOAD {
        return Err(ProtocolError::FrameTooLarge(payload_len).into());
    }

    let mut frame = Vec::with_capacity(FRAME_HEADER_SIZE + payload_len);
    frame.extend_from_slice(&header);
    frame.resize(FRAME_HEADER_SIZE + payload_len, 0);
    reader
        .read_exact(&mut frame[FRAME_HEADER_SIZE..])
        .map_err(ProtocolError::from)?;
    Ok((sidealsa_protocol::decode_response(&frame)?, fds))
}

fn duplicate_cloexec(fd: RawFd) -> io::Result<RawFd> {
    loop {
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate >= 0 {
            return Ok(duplicate);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
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
        io::Write,
        os::unix::net::UnixStream,
        sync::{Arc, mpsc},
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
            profile_fingerprint: 0x1234,
            rate: 48000,
            period_size: 32,
            hardware_period_size: 16,
            buffer_size: 64,
            shared_buffer_size: 256,
            pro_latency_periods: 1,
            pro_output_latency_frames: 48,
            pro_realtime_priority: 15,
            shared_latency_periods: 3,
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
    fn fragmented_fd_response_is_read_exactly_and_descriptors_are_cloexec() {
        let (mut sender, receiver) = UnixStream::pair().expect("socket pair should create");
        let response = Response::OpenPro {
            session_id: 41,
            shared: SharedRegionInfo {
                size: 4096,
                period_frames: 64,
                playback_channels: 2,
                capture_channels: 1,
                slot_count: 8,
                slot_stride: 512,
                capture_offset: 128,
                playback_offset: 2048,
            },
        };
        let frame = sidealsa_protocol::encode_response(&response).expect("response should encode");
        let sent_fds = owned_fds([event_fd(), event_fd(), event_fd(), event_fd()]);
        let raw_fds: Vec<_> = sent_fds.iter().map(AsRawFd::as_raw_fd).collect();

        send_with_fds(&sender, &frame[..1], &raw_fds);
        sender
            .write_all(&frame[1..5])
            .expect("partial header should write");
        sender
            .write_all(&frame[5..12])
            .expect("remaining header should write");
        sender
            .write_all(&frame[12..])
            .expect("payload should write");

        let (received, received_fds) = receive_with_fds(&receiver).expect("response should arrive");
        assert_eq!(received, response);
        assert_eq!(received_fds.len(), 4);
        for fd in &received_fds {
            assert_cloexec(fd.as_raw_fd());
        }
    }

    #[test]
    fn malformed_fd_response_closes_received_descriptors() {
        let (mut sender, receiver) = UnixStream::pair().expect("socket pair should create");
        let mut pipe_fds = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK,) },
            0
        );
        let read_fd = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
        let write_fd = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };
        let mut frame =
            sidealsa_protocol::encode_response(&Response::Ack).expect("response should encode");
        frame[6..8].copy_from_slice(&u16::MAX.to_le_bytes());

        send_with_fds(&sender, &frame[..1], &[write_fd.as_raw_fd()]);
        sender
            .write_all(&frame[1..])
            .expect("malformed response should write");
        drop(write_fd);
        assert!(matches!(
            receive_with_fds(&receiver),
            Err(ClientError::Protocol(ProtocolError::UnknownResponse(
                u16::MAX
            )))
        ));

        let mut byte = 0_u8;
        assert_eq!(
            unsafe { libc::read(read_fd.as_raw_fd(), (&mut byte as *mut u8).cast(), 1,) },
            0,
            "the received pipe writer should be closed on decode failure"
        );
    }

    #[test]
    fn oversized_fd_response_is_rejected_before_payload_read() {
        let (mut sender, receiver) = UnixStream::pair().expect("socket pair should create");
        let mut header = [0_u8; 12];
        header[..4].copy_from_slice(&PROTOCOL_MAGIC);
        header[4..6].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        header[6..8].copy_from_slice(&4_u16.to_le_bytes());
        header[8..12].copy_from_slice(&((MAX_FRAME_PAYLOAD + 1) as u32).to_le_bytes());
        sender
            .write_all(&header)
            .expect("oversized header should write");

        assert!(matches!(
            receive_with_fds(&receiver),
            Err(ClientError::Protocol(ProtocolError::FrameTooLarge(size)))
                if size == MAX_FRAME_PAYLOAD + 1
        ));
    }

    #[test]
    fn truncated_descriptor_control_is_rejected() {
        let (mut sender, receiver) = UnixStream::pair().expect("socket pair should create");
        let frame =
            sidealsa_protocol::encode_response(&Response::Ack).expect("response should encode");
        let sent_fds = [event_fd(), event_fd(), event_fd(), event_fd(), event_fd()]
            .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
        let raw_fds: [RawFd; 5] = std::array::from_fn(|index| sent_fds[index].as_raw_fd());

        send_with_fds(&sender, &frame[..1], &raw_fds);
        sender
            .write_all(&frame[1..])
            .expect("response should write");
        assert!(matches!(
            receive_with_fds(&receiver),
            Err(ClientError::Protocol(ProtocolError::MalformedPayload))
        ));
    }

    #[test]
    fn duplicated_notification_and_control_descriptors_are_cloexec() {
        let server_region = SharedRegion::create(1, 1, 0).expect("region should create");
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let (peer, control) = UnixStream::pair().expect("socket pair should create");
        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Shared(PortDirection::Playback),
            server_region.info(),
            owned_fds([client_fd, event_fd(), event_fd(), event_fd()]),
        )
        .expect("stream should create");

        let notification = unsafe {
            OwnedFd::from_raw_fd(
                stream
                    .notification_fd()
                    .expect("notification should duplicate"),
            )
        };
        let control =
            unsafe { OwnedFd::from_raw_fd(stream.control_fd().expect("control should duplicate")) };
        assert_cloexec(notification.as_raw_fd());
        assert_cloexec(control.as_raw_fd());

        stream.closed = true;
        drop(peer);
    }

    #[test]
    fn eventfd_drain_after_poll_is_a_spurious_wake() {
        let event = EventFd::from_owned(unsafe { OwnedFd::from_raw_fd(event_fd()) });
        signal_event(event.as_raw_fd());
        let mut descriptor = libc::pollfd {
            fd: event.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 0) }, 1);
        assert_ne!(descriptor.revents & libc::POLLIN, 0);

        let mut value = 0_u64;
        assert_eq!(
            unsafe {
                libc::read(
                    event.as_raw_fd(),
                    (&mut value as *mut u64).cast(),
                    size_of::<u64>(),
                )
            },
            size_of::<u64>() as isize
        );
        assert_eq!(
            event
                .consume_ready(descriptor.revents)
                .expect("a raced drain should not fail"),
            EventWait::Spurious
        );
    }

    #[test]
    fn daemon_disconnect_wakes_wait_period_as_closed() {
        let server_region = SharedRegion::create(1, 1, 0).expect("region should create");
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let (peer, control) = UnixStream::pair().expect("socket pair should create");
        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Shared(PortDirection::Playback),
            server_region.info(),
            owned_fds([client_fd, event_fd(), event_fd(), event_fd()]),
        )
        .expect("stream should create");
        stream.started = true;
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            waiting_tx.send(()).expect("wait should signal entry");
            let result = stream.wait_period(Duration::from_secs(2));
            result_tx
                .send(matches!(result, Err(ClientError::Closed)))
                .expect("wait result should send");
        });

        waiting_rx.recv().expect("waiter should start");
        drop(peer);
        assert!(
            result_rx
                .recv_timeout(Duration::from_millis(500))
                .expect("disconnect should wake wait_period")
        );
        waiter.join().expect("waiter should not panic");
    }

    #[test]
    fn playback_stream_waits_and_submits_next_sequence() {
        let server_region = SharedRegion::create(4, 2, 0).expect("region should create");
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let capture_fd = event_fd();
        let playback_fd = event_fd();
        let playback_ready_fd = event_fd();
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
            StreamMode::Pro,
            info,
            owned_fds([client_fd, capture_fd, playback_fd, playback_ready_fd]),
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
        assert!(server_region.try_consume_playback(8, &mut submitted));
        assert_eq!(submitted, [1, 2, 3, 4, 5, 6, 7, 8]);
        let mut ready = 0_u64;
        assert_eq!(
            unsafe {
                libc::read(
                    playback_ready_fd,
                    (&mut ready as *mut u64).cast(),
                    size_of::<u64>(),
                )
            },
            size_of::<u64>() as isize
        );
        assert_eq!(ready, 1);
        signal_event(playback_fd);
        assert!(matches!(
            stream.wait_period(Duration::ZERO),
            Err(ClientError::Timeout)
        ));
        server_region.set_cycle_sequence(9);
        signal_event(playback_fd);
        assert_eq!(
            stream
                .wait_period(Duration::from_millis(100))
                .expect("next period should arrive"),
            9
        );
        stream.close().expect("stream should close");
        server.join().expect("server should not panic");
    }

    #[test]
    fn close_ack_does_not_touch_resources_reused_by_a_new_owner() {
        let server_region = Arc::new(SharedRegion::create(1, 0, 1).expect("region should create"));
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let capture_fd = event_fd();
        let server_capture_fd = unsafe { libc::dup(capture_fd) };
        let observed_capture_fd = unsafe { libc::dup(capture_fd) };
        assert!(server_capture_fd >= 0);
        assert!(observed_capture_fd >= 0);
        let (mut peer, control) = UnixStream::pair().expect("socket pair should create");
        let server_region_for_peer = Arc::clone(&server_region);
        let server = thread::spawn(move || {
            assert_eq!(
                sidealsa_protocol::read_request(&mut peer).expect("close should arrive"),
                Request::Close { session_id: 9 }
            );

            // Model a new owner publishing after daemon-side close, but before the old ACK arrives.
            server_region_for_peer.reset_slots();
            let mut producer_index = 0;
            assert!(server_region_for_peer.try_publish_capture(&mut producer_index, 77, &[42]));
            signal_event(server_capture_fd);
            close_fd(server_capture_fd);
            sidealsa_protocol::write_response(&mut peer, &Response::Ack)
                .expect("close ack should write");
        });

        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Shared(PortDirection::Capture),
            server_region.info(),
            owned_fds([client_fd, capture_fd, event_fd(), event_fd()]),
        )
        .expect("stream should create");
        stream.close().expect("stream should close");
        drop(stream);
        server.join().expect("server should not panic");

        let mut consumer_index = 0;
        let mut samples = [0];
        assert_eq!(
            server_region.try_client_read_capture(&mut consumer_index, &mut samples),
            Some(77)
        );
        assert_eq!(samples, [42]);
        let mut notification = 0_u64;
        assert_eq!(
            unsafe {
                libc::read(
                    observed_capture_fd,
                    (&mut notification as *mut u64).cast(),
                    size_of::<u64>(),
                )
            },
            size_of::<u64>() as isize
        );
        assert_eq!(notification, 1);
        close_fd(observed_capture_fd);
    }

    #[test]
    fn partial_close_ack_poisons_stream_and_drop_sends_nothing() {
        let server_region = SharedRegion::create(1, 0, 1).expect("region should create");
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let info = server_region.info();
        let (mut peer, control) = UnixStream::pair().expect("socket pair should create");
        control
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("timeout should set");
        let (partial_tx, partial_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            assert_eq!(
                sidealsa_protocol::read_request(&mut peer).expect("close should arrive"),
                Request::Close { session_id: 9 }
            );
            let ack =
                sidealsa_protocol::encode_response(&Response::Ack).expect("ack should encode");
            peer.write_all(&ack[..6]).expect("partial ack should write");
            partial_tx.send(()).expect("partial ack should signal");
            release_rx.recv().expect("test should release server");
            peer.set_read_timeout(Some(Duration::from_millis(250)))
                .expect("timeout should set");
            match sidealsa_protocol::read_request(&mut peer) {
                Err(ProtocolError::Io(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                    ) => {}
                result => {
                    panic!("poisoned stream should close without another request: {result:?}")
                }
            }
        });

        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Shared(PortDirection::Capture),
            info,
            owned_fds([client_fd, event_fd(), event_fd(), event_fd()]),
        )
        .expect("stream should create");
        let client = thread::spawn(move || {
            assert!(matches!(
                stream.close(),
                Err(ClientError::Protocol(ProtocolError::Io(_)))
            ));
            stream
        });
        partial_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("partial ack should arrive");
        let mut stream = client.join().expect("client should not panic");
        assert!(matches!(stream.get_stats(), Err(ClientError::Closed)));
        assert!(matches!(stream.start(), Err(ClientError::Closed)));
        assert!(matches!(stream.close(), Err(ClientError::Closed)));
        drop(stream);
        release_tx.send(()).expect("server should be released");
        server.join().expect("server should not panic");
    }

    #[test]
    fn daemon_error_response_does_not_poison_stream() {
        let server_region = SharedRegion::create(1, 0, 1).expect("region should create");
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let (mut peer, control) = UnixStream::pair().expect("socket pair should create");
        let server = thread::spawn(move || {
            assert_eq!(
                sidealsa_protocol::read_request(&mut peer).expect("start should arrive"),
                Request::Start { session_id: 9 }
            );
            sidealsa_protocol::write_response(
                &mut peer,
                &Response::Error {
                    code: ErrorCode::BadState,
                    message: "not ready".into(),
                },
            )
            .expect("daemon error should write");
            assert_eq!(
                sidealsa_protocol::read_request(&mut peer).expect("stats should arrive"),
                Request::GetStats
            );
            sidealsa_protocol::write_response(&mut peer, &Response::Stats(Box::default()))
                .expect("stats should write");
            assert_eq!(
                sidealsa_protocol::read_request(&mut peer).expect("close should arrive"),
                Request::Close { session_id: 9 }
            );
            sidealsa_protocol::write_response(&mut peer, &Response::Ack)
                .expect("close ack should write");
        });

        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Shared(PortDirection::Capture),
            server_region.info(),
            owned_fds([client_fd, event_fd(), event_fd(), event_fd()]),
        )
        .expect("stream should create");
        assert!(matches!(
            stream.start(),
            Err(ClientError::Daemon {
                code: ErrorCode::BadState,
                ..
            })
        ));
        assert_eq!(
            stream.get_stats().expect("stream should remain usable"),
            Stats::default()
        );
        stream.close().expect("stream should close");
        server.join().expect("server should not panic");
    }

    #[test]
    fn pro_keeps_playback_sequence_for_daemon_expiry_check() {
        let server_region = SharedRegion::create(4, 2, 0).expect("region should create");
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let (peer, control) = UnixStream::pair().expect("socket pair should create");
        drop(peer);
        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Pro,
            server_region.info(),
            owned_fds([client_fd, event_fd(), event_fd(), event_fd()]),
        )
        .expect("stream should create");
        stream.started = true;
        server_region.set_cycle_sequence(9);

        assert!(
            stream
                .submit_playback(8, &[1, 2, 3, 4, 5, 6, 7, 8])
                .expect("playback should remain publishable")
        );
        let mut submitted = [0; 8];
        assert!(server_region.try_consume_playback(8, &mut submitted));
        assert_eq!(submitted, [1, 2, 3, 4, 5, 6, 7, 8]);
        stream.closed = true;
    }

    #[test]
    fn pro_wait_preserves_oldest_still_valid_capture_sequence() {
        let server_region = SharedRegion::create(1, 1, 1).expect("region should create");
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let (peer, control) = UnixStream::pair().expect("socket pair should create");
        drop(peer);
        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Pro,
            server_region.info(),
            owned_fds([client_fd, event_fd(), event_fd(), event_fd()]),
        )
        .expect("stream should create");
        stream.started = true;

        let mut producer_index = 0;
        for sequence in 20..=24 {
            assert!(server_region.try_publish_capture(
                &mut producer_index,
                sequence,
                &[sequence as i32]
            ));
        }
        server_region.set_playback_sequence(20);

        assert_eq!(
            stream
                .wait_period(Duration::ZERO)
                .expect("ready capture should not need an event"),
            20
        );
        assert!(server_region.try_publish_capture(&mut producer_index, 25, &[25]));
        let mut capture = [0];
        assert_eq!(
            stream
                .capture_buffer_for_sequence(20, &mut capture)
                .expect("capture should read"),
            Some(20)
        );
        assert_eq!(capture, [20]);

        server_region.set_playback_sequence(24);
        assert_eq!(
            stream
                .wait_period(Duration::ZERO)
                .expect("valid capture should remain ready"),
            24
        );
        server_region.set_playback_sequence(25);
        assert_eq!(
            stream
                .capture_buffer_for_sequence(24, &mut capture)
                .expect("capture should read"),
            Some(25)
        );
        assert_eq!(capture, [25]);
        assert_eq!(server_region.client_expired_capture_blocks(), 4);
        assert_eq!(server_region.oldest_valid_client_capture_sequence(25), None);
        stream.closed = true;
    }

    #[test]
    fn ready_capture_fast_path_drains_notification() {
        let server_region = SharedRegion::create(1, 0, 1).expect("region should create");
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let capture_fd = event_fd();
        let observed_capture_fd = unsafe { libc::dup(capture_fd) };
        assert!(observed_capture_fd >= 0);
        let (peer, control) = UnixStream::pair().expect("socket pair should create");
        drop(peer);
        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Shared(PortDirection::Capture),
            server_region.info(),
            owned_fds([client_fd, capture_fd, event_fd(), event_fd()]),
        )
        .expect("stream should create");
        stream.started = true;
        let mut producer_index = 0;
        assert!(server_region.try_publish_capture(&mut producer_index, 7, &[7]));
        signal_event(observed_capture_fd);

        assert_eq!(
            stream
                .wait_period(Duration::ZERO)
                .expect("ready capture should return"),
            7
        );
        let mut value = 0_u64;
        assert_eq!(
            unsafe {
                libc::read(
                    observed_capture_fd,
                    (&mut value as *mut u64).cast(),
                    size_of::<u64>(),
                )
            },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN)
        );

        unsafe { libc::close(observed_capture_fd) };
        stream.closed = true;
    }

    #[test]
    fn ready_capture_fast_path_rearms_when_another_slot_is_ready() {
        let server_region = SharedRegion::create(1, 0, 1).expect("region should create");
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let capture_fd = event_fd();
        let observed_capture_fd = unsafe { libc::dup(capture_fd) };
        assert!(observed_capture_fd >= 0);
        let (peer, control) = UnixStream::pair().expect("socket pair should create");
        drop(peer);
        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Shared(PortDirection::Capture),
            server_region.info(),
            owned_fds([client_fd, capture_fd, event_fd(), event_fd()]),
        )
        .expect("stream should create");
        stream.started = true;
        let mut producer_index = 0;
        assert!(server_region.try_publish_capture(&mut producer_index, 7, &[7]));
        assert!(server_region.try_publish_capture(&mut producer_index, 8, &[8]));
        signal_event(observed_capture_fd);
        signal_event(observed_capture_fd);

        assert_eq!(
            stream
                .wait_period(Duration::ZERO)
                .expect("first capture should return"),
            7
        );
        let mut descriptor = libc::pollfd {
            fd: observed_capture_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 0) }, 1);
        assert_ne!(descriptor.revents & libc::POLLIN, 0);

        let mut samples = [0];
        assert_eq!(
            stream
                .capture_buffer(&mut samples)
                .expect("capture should read"),
            Some(7)
        );
        assert_eq!(
            stream
                .wait_period(Duration::ZERO)
                .expect("second capture should return"),
            8
        );

        unsafe { libc::close(observed_capture_fd) };
        stream.closed = true;
    }

    #[test]
    fn shared_capture_discontinuity_requests_pcm_recovery() {
        let server_region = SharedRegion::create(1, 0, 1).expect("region should create");
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let (peer, control) = UnixStream::pair().expect("socket pair should create");
        drop(peer);
        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Shared(PortDirection::Capture),
            server_region.info(),
            owned_fds([client_fd, event_fd(), event_fd(), event_fd()]),
        )
        .expect("stream should create");
        stream.started = true;
        server_region.record_capture_discontinuity();

        assert!(matches!(
            stream.wait_period(Duration::ZERO),
            Err(ClientError::CaptureDiscontinuity)
        ));
        stream.closed = true;
    }

    #[test]
    fn hardware_generation_change_requests_stream_recovery() {
        let server_region = SharedRegion::create(1, 0, 1).expect("region should create");
        server_region.set_hardware_generation(4);
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let (_peer, control) = UnixStream::pair().expect("socket pair should create");
        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Shared(PortDirection::Capture),
            server_region.info(),
            owned_fds([client_fd, event_fd(), event_fd(), event_fd()]),
        )
        .expect("stream should create");
        stream.started = true;
        server_region.set_hardware_generation(5);

        assert!(matches!(
            stream.wait_period(Duration::ZERO),
            Err(ClientError::CaptureDiscontinuity)
        ));
        stream.closed = true;
    }

    #[test]
    fn stream_can_stop_and_restart_after_hardware_generation_change() {
        let server_region = SharedRegion::create(1, 0, 1).expect("region should create");
        server_region.set_hardware_generation(4);
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let capture_fd = event_fd();
        let signal_fd = unsafe { libc::dup(capture_fd) };
        assert!(signal_fd >= 0);
        let info = server_region.info();
        let (mut peer, control) = UnixStream::pair().expect("socket pair should create");
        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Shared(PortDirection::Capture),
            info,
            owned_fds([client_fd, capture_fd, event_fd(), event_fd()]),
        )
        .expect("stream should create");
        stream.started = true;
        server_region.set_hardware_generation(5);
        let server = thread::spawn(move || {
            assert_eq!(
                sidealsa_protocol::read_request(&mut peer).expect("stop should arrive"),
                Request::Stop { session_id: 9 }
            );
            sidealsa_protocol::write_response(&mut peer, &Response::Ack)
                .expect("stop ack should write");
            assert_eq!(
                sidealsa_protocol::read_request(&mut peer).expect("restart should arrive"),
                Request::Start { session_id: 9 }
            );
            server_region.set_lifecycle_generation(1);
            sidealsa_protocol::write_response(&mut peer, &Response::Ack)
                .expect("start ack should write");
            let mut producer_index = 0;
            assert!(server_region.try_publish_capture(&mut producer_index, 17, &[17]));
            signal_event(signal_fd);
            unsafe { libc::close(signal_fd) };
            assert_eq!(
                sidealsa_protocol::read_request(&mut peer).expect("final stop should arrive"),
                Request::Stop { session_id: 9 }
            );
            sidealsa_protocol::write_response(&mut peer, &Response::Ack)
                .expect("final stop ack should write");
        });

        assert!(matches!(
            stream.wait_period(Duration::ZERO),
            Err(ClientError::CaptureDiscontinuity)
        ));
        stream.prepare().expect("stream should prepare again");
        stream.start().expect("stream should restart");
        assert_eq!(
            stream
                .wait_period(Duration::from_millis(100))
                .expect("capture should resume"),
            17
        );
        let mut capture = [0];
        assert_eq!(
            stream
                .capture_buffer_for_sequence(17, &mut capture)
                .expect("capture should read after restart"),
            Some(17)
        );
        assert_eq!(capture, [17]);
        stream.stop().expect("stream should stop");
        stream.closed = true;
        server.join().expect("server should not panic");
    }

    #[test]
    fn shared_capture_detects_loss_while_start_waits_for_ack() {
        let server_region = SharedRegion::create(1, 0, 1).expect("region should create");
        let client_fd = unsafe { libc::dup(server_region.fd()) };
        assert!(client_fd >= 0);
        let info = server_region.info();
        let (mut peer, control) = UnixStream::pair().expect("socket pair should create");
        let server = thread::spawn(move || {
            assert_eq!(
                sidealsa_protocol::read_request(&mut peer).expect("start should arrive"),
                Request::Start { session_id: 9 }
            );
            server_region.set_lifecycle_generation(1);
            server_region.record_capture_discontinuity();
            sidealsa_protocol::write_response(&mut peer, &Response::Ack)
                .expect("start ack should write");
        });
        let mut stream = AudioStream::from_parts(
            control,
            9,
            StreamMode::Shared(PortDirection::Capture),
            info,
            owned_fds([client_fd, event_fd(), event_fd(), event_fd()]),
        )
        .expect("stream should create");

        stream.start().expect("stream should start");
        assert!(matches!(
            stream.wait_period(Duration::ZERO),
            Err(ClientError::CaptureDiscontinuity)
        ));

        stream.closed = true;
        server.join().expect("server should not panic");
    }

    fn owned_fds(fds: [RawFd; 4]) -> Vec<OwnedFd> {
        fds.into_iter()
            .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
            .collect()
    }

    fn assert_cloexec(fd: RawFd) {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "descriptor flags should be readable");
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    fn send_with_fds(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) {
        assert!(!bytes.is_empty());
        assert!(!fds.is_empty());
        let fd_bytes = std::mem::size_of_val(fds);
        let control_len = unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize };
        let mut control = vec![0_usize; control_len.div_ceil(size_of::<usize>())];
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut libc::c_void,
            iov_len: bytes.len(),
        };
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control_len;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&message);
            assert!(!cmsg.is_null());
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as usize;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr(),
                libc::CMSG_DATA(cmsg).cast::<RawFd>(),
                fds.len(),
            );
        }
        assert_eq!(
            unsafe { libc::sendmsg(stream.as_raw_fd(), &message, 0) },
            bytes.len() as isize
        );
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
