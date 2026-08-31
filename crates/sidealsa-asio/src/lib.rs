// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    ffi::{CStr, c_char, c_int, c_void},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    ptr, slice,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use sidealsa_client::{AudioStream, ClientError, SideAlsaClient};
use sidealsa_protocol::{DeviceInfo, SharedRegionInfo};
use thiserror::Error;

pub const ASIO_SUCCESS: c_int = 0;
pub const ASIO_INVALID_PARAMETER: c_int = -998;
pub const ASIO_NO_IO: c_int = -1000;
pub const ASIO_BUFFER_SIZE_NOT_SUPPORTED: c_int = -997;
pub const ASIO_HW_MALFUNCTION: c_int = -999;
pub const ASIO_NO_MEMORY: c_int = -994;
pub const ASIO_SAMPLE_RATE_NOT_SUPPORTED: c_int = -995;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);

pub const ASIOST_FLOAT32_LSB: c_int = 19;
const ASIO_MESSAGE_RESET_REQUEST: c_int = 3;
const ASIO_MESSAGE_RESYNC_REQUEST: c_int = 5;
pub const ASIO_MESSAGE_SUPPORTS_TIME_INFO: c_int = 7;
const WORKER_POLL_HEARTBEAT_MS: c_int = 100;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AsioBufferInfo {
    pub is_input_type: c_int,
    pub channel_number: c_int,
    pub buffers: [*mut c_void; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AsioInt64 {
    pub hi: u32,
    pub lo: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AsioTimeInfo {
    pub reserved: [c_int; 4],
    pub speed: f64,
    pub time_stamp: AsioInt64,
    pub sample_position: AsioInt64,
    pub sample_rate: f64,
    pub flags: u32,
    pub reserved_2: [u8; 12],
    pub speed_for_time_code: f64,
    pub time_code: AsioInt64,
    pub time_code_flags: u32,
    pub reserved_3: [u8; 64],
}

pub type BufferSwitch = unsafe extern "win64" fn(index: c_int, direct_process: c_int);
pub type SampleRateChanged = unsafe extern "win64" fn(sample_rate: f64);
pub type AsioMessage = unsafe extern "win64" fn(
    selector: c_int,
    value: c_int,
    message: *mut c_void,
    opt: *mut f64,
) -> c_int;
pub type BufferSwitchTimeInfo = unsafe extern "win64" fn(
    time_info: *mut AsioTimeInfo,
    index: c_int,
    direct_process: c_int,
) -> *mut AsioTimeInfo;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AsioCallbacks {
    pub buffer_switch: Option<BufferSwitch>,
    pub sample_rate_changed: Option<SampleRateChanged>,
    pub asio_message: Option<AsioMessage>,
    pub buffer_switch_time_info: Option<BufferSwitchTimeInfo>,
}

pub type ThreadCreate = unsafe extern "C" fn(
    context: *mut c_void,
    handle: *mut *mut c_void,
    thread_id: *mut u32,
) -> c_int;
pub type ThreadJoin = unsafe extern "C" fn(handle: *mut c_void, timeout_ms: u32) -> c_int;
pub type CurrentThreadId = unsafe extern "C" fn() -> u32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioThreadOps {
    pub create: Option<ThreadCreate>,
    pub join: Option<ThreadJoin>,
    pub current_thread_id: Option<CurrentThreadId>,
}

#[derive(Clone, Copy)]
struct ThreadOps {
    create: ThreadCreate,
    join: ThreadJoin,
    current_thread_id: CurrentThreadId,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AsioChannelInfo {
    pub channel: c_int,
    pub is_input: c_int,
    pub is_active: c_int,
    pub channel_group: c_int,
    pub sample_type: c_int,
    pub name: [c_char; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DriverState {
    Loaded,
    Initialized,
    Prepared,
    Running,
    Stopping,
    Destroying,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
enum WorkerFailure {
    DaemonDisconnected = 1,
    CaptureDiscontinuity = 2,
    Callback = 3,
    Stream = 4,
}

impl WorkerFailure {
    fn from_error(error: &AsioError) -> Self {
        match error {
            AsioError::DaemonDisconnected | AsioError::Client(ClientError::Closed) => {
                Self::DaemonDisconnected
            }
            AsioError::CaptureSequenceBackward
            | AsioError::Client(ClientError::CaptureDiscontinuity) => Self::CaptureDiscontinuity,
            AsioError::CallbackPanic => Self::Callback,
            _ => Self::Stream,
        }
    }

    fn from_raw(value: u64) -> Option<Self> {
        match value {
            1 => Some(Self::DaemonDisconnected),
            2 => Some(Self::CaptureDiscontinuity),
            3 => Some(Self::Callback),
            4 => Some(Self::Stream),
            _ => None,
        }
    }

    fn error(self) -> AsioError {
        match self {
            Self::DaemonDisconnected => AsioError::DaemonDisconnected,
            Self::CaptureDiscontinuity => {
                AsioError::WorkerStopped("audio capture must be resynchronized")
            }
            Self::Callback => AsioError::WorkerStopped("host audio callback failed"),
            Self::Stream => AsioError::WorkerStopped("audio stream failed"),
        }
    }

    fn notification(self) -> Option<c_int> {
        match self {
            Self::CaptureDiscontinuity => Some(ASIO_MESSAGE_RESYNC_REQUEST),
            Self::DaemonDisconnected | Self::Stream => Some(ASIO_MESSAGE_RESET_REQUEST),
            Self::Callback => None,
        }
    }
}

struct PositionSnapshot {
    sequence: AtomicU64,
    samples: AtomicU64,
    stamp: AtomicU64,
}

impl PositionSnapshot {
    const fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            samples: AtomicU64::new(0),
            stamp: AtomicU64::new(0),
        }
    }

    fn publish(&self, samples: u64, stamp: u64) {
        self.sequence.fetch_add(1, Ordering::SeqCst);
        self.samples.store(samples, Ordering::SeqCst);
        self.stamp.store(stamp, Ordering::SeqCst);
        self.sequence.fetch_add(1, Ordering::SeqCst);
    }

    fn load(&self) -> (u64, u64) {
        loop {
            let before = self.sequence.load(Ordering::SeqCst);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let samples = self.samples.load(Ordering::SeqCst);
            let stamp = self.stamp.load(Ordering::SeqCst);
            let after = self.sequence.load(Ordering::SeqCst);
            if before == after {
                return (samples, stamp);
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum AsioError {
    #[error("driver is in an invalid state")]
    InvalidState,
    #[error("invalid parameter")]
    InvalidParameter,
    #[error("no audio I/O")]
    NoIo,
    #[error("buffer size is not supported")]
    BufferSizeNotSupported,
    #[error("sample rate is not supported")]
    SampleRateNotSupported,
    #[error("driver has no memory")]
    NoMemory,
    #[error("client error: {0}")]
    Client(#[from] ClientError),
    #[error("worker thread failed")]
    Worker,
    #[error("timed out waiting for the audio worker")]
    WorkerTimeout,
    #[error("realtime scheduling failed: {0}")]
    RealtimeScheduling(#[source] std::io::Error),
    #[error("worker callback panicked")]
    CallbackPanic,
    #[error("driver method called from audio callback")]
    Reentrant,
    #[error("capture sequence moved backward")]
    CaptureSequenceBackward,
    #[error("SideALSA daemon disconnected")]
    DaemonDisconnected,
    #[error("audio worker stopped: {0}")]
    WorkerStopped(&'static str),
}

impl AsioError {
    pub fn code(&self) -> c_int {
        match self {
            Self::InvalidState | Self::NoIo => ASIO_NO_IO,
            Self::InvalidParameter => ASIO_INVALID_PARAMETER,
            Self::BufferSizeNotSupported => ASIO_BUFFER_SIZE_NOT_SUPPORTED,
            Self::SampleRateNotSupported => ASIO_SAMPLE_RATE_NOT_SUPPORTED,
            Self::NoMemory => ASIO_NO_MEMORY,
            Self::Client(_)
            | Self::Worker
            | Self::WorkerTimeout
            | Self::RealtimeScheduling(_)
            | Self::CaptureSequenceBackward
            | Self::DaemonDisconnected
            | Self::WorkerStopped(_) => ASIO_HW_MALFUNCTION,
            Self::CallbackPanic | Self::Reentrant => ASIO_NO_IO,
        }
    }
}

struct DriverInner {
    state: DriverState,
    device: Option<DeviceInfo>,
    shared: Option<SharedRegionInfo>,
    stream: Option<AudioStream>,
    buffers: Option<Arc<HostBuffers>>,
    active: Option<Arc<ActiveChannels>>,
    callbacks: Option<AsioCallbacks>,
    time_info: bool,
    worker: Option<WorkerHandle>,
    worker_thread: Option<u32>,
    quiesce_generation: Option<u64>,
    position: Arc<PositionSnapshot>,
    worker_failure: Option<WorkerFailure>,
    last_error: Option<String>,
}

pub struct AsioDriver {
    inner: Mutex<DriverInner>,
    lifecycle: Mutex<()>,
    thread_ops: Option<ThreadOps>,
    control_timeout: Duration,
}

impl AsioDriver {
    pub fn new() -> Self {
        Self::new_with_thread_ops(None)
    }

    fn new_with_thread_ops(thread_ops: Option<ThreadOps>) -> Self {
        Self::new_with_thread_ops_and_timeout(thread_ops, CONTROL_TIMEOUT)
    }

    fn new_with_thread_ops_and_timeout(
        thread_ops: Option<ThreadOps>,
        control_timeout: Duration,
    ) -> Self {
        Self {
            inner: Mutex::new(DriverInner {
                state: DriverState::Loaded,
                device: None,
                shared: None,
                stream: None,
                buffers: None,
                active: None,
                callbacks: None,
                time_info: false,
                worker: None,
                worker_thread: None,
                quiesce_generation: None,
                position: Arc::new(PositionSnapshot::new()),
                worker_failure: None,
                last_error: None,
            }),
            lifecycle: Mutex::new(()),
            thread_ops,
            control_timeout,
        }
    }

    pub fn init(&self, socket: impl AsRef<Path>) -> Result<(), AsioError> {
        let result = (|| {
            let mut client = SideAlsaClient::connect_with_timeout(socket, self.control_timeout)?;
            let device = client.get_info()?;
            let stream = client.open_pro()?;
            let shared = stream.info();

            let mut inner = self.lock()?;
            if inner.state != DriverState::Loaded {
                return Err(AsioError::InvalidState);
            }
            if shared.period_frames != device.period_size
                || shared.playback_channels != device.playback_channels
                || shared.capture_channels != device.capture_channels
            {
                return Err(AsioError::NoIo);
            }
            inner.device = Some(device);
            inner.shared = Some(shared);
            inner.stream = Some(stream);
            inner.worker_failure = None;
            inner.last_error = None;
            inner.state = DriverState::Initialized;
            Ok(())
        })();
        if let Err(error) = &result
            && let Ok(mut inner) = self.inner.lock()
        {
            inner.last_error = Some(error.to_string());
        }
        result
    }

    pub fn device(&self) -> Result<DeviceInfo, AsioError> {
        let mut inner = self.lock()?;
        ensure_worker_healthy(&mut inner)?;
        inner.device.clone().ok_or(AsioError::InvalidState)
    }

    pub fn get_channels(&self) -> Result<(c_int, c_int), AsioError> {
        let mut inner = self.lock()?;
        ensure_worker_healthy(&mut inner)?;
        ensure_initialized(inner.state)?;
        let device = inner.device.as_ref().ok_or(AsioError::InvalidState)?;
        Ok((
            checked_c_int(device.capture_channels)?,
            checked_c_int(device.playback_channels)?,
        ))
    }

    pub fn get_buffer_size(&self) -> Result<(c_int, c_int, c_int, c_int), AsioError> {
        let mut inner = self.lock()?;
        ensure_worker_healthy(&mut inner)?;
        ensure_initialized(inner.state)?;
        let device = inner.device.as_ref().ok_or(AsioError::InvalidState)?;
        let period = checked_c_int(device.period_size)?;
        Ok((period, period, period, 0))
    }

    pub fn get_sample_rate(&self) -> Result<f64, AsioError> {
        let mut inner = self.lock()?;
        ensure_worker_healthy(&mut inner)?;
        ensure_initialized(inner.state)?;
        Ok(f64::from(
            inner.device.as_ref().ok_or(AsioError::InvalidState)?.rate,
        ))
    }

    pub fn set_sample_rate(&self, sample_rate: f64) -> Result<(), AsioError> {
        let current = self.get_sample_rate()?;
        if sample_rate != current {
            return Err(AsioError::SampleRateNotSupported);
        }
        Ok(())
    }

    pub fn get_latencies(&self) -> Result<(c_int, c_int), AsioError> {
        let mut inner = self.lock()?;
        ensure_worker_healthy(&mut inner)?;
        ensure_initialized(inner.state)?;
        let device = inner.device.as_ref().ok_or(AsioError::InvalidState)?;
        let period = checked_c_int(device.period_size)?;
        let output = checked_c_int(device.pro_output_latency_frames)?;
        Ok((period, output))
    }

    pub fn get_channel_info(
        &self,
        channel: c_int,
        is_input: c_int,
    ) -> Result<AsioChannelInfo, AsioError> {
        let mut inner = self.lock()?;
        ensure_worker_healthy(&mut inner)?;
        ensure_initialized(inner.state)?;
        let device = inner.device.as_ref().ok_or(AsioError::InvalidState)?;
        let count = if is_input != 0 {
            device.capture_channels
        } else {
            device.playback_channels
        };
        if channel < 0 || u32::try_from(channel).map_or(true, |value| value >= count) {
            return Err(AsioError::InvalidParameter);
        }
        let channel_index = usize::try_from(channel).map_err(|_| AsioError::InvalidParameter)?;
        let mut name = [0; 32];
        let prefix = if is_input != 0 { "Input " } else { "Output " };
        write_channel_name(&mut name, prefix.as_bytes(), channel_index + 1);
        let is_active = inner.active.as_ref().is_some_and(|active| {
            if is_input != 0 {
                active.input.get(channel_index).copied().unwrap_or(false)
            } else {
                active.output.get(channel_index).copied().unwrap_or(false)
            }
        });
        Ok(AsioChannelInfo {
            channel,
            is_input: if is_input != 0 { 1 } else { 0 },
            is_active: c_int::from(is_active),
            channel_group: 0,
            sample_type: ASIOST_FLOAT32_LSB,
            name,
        })
    }

    pub fn create_buffers(
        &self,
        infos: &mut [AsioBufferInfo],
        buffer_size: c_int,
        callbacks: AsioCallbacks,
    ) -> Result<(), AsioError> {
        let mut inner = self.lock()?;
        ensure_worker_healthy(&mut inner)?;
        if inner.state != DriverState::Initialized {
            return Err(AsioError::InvalidState);
        }
        if callbacks.buffer_switch.is_none() {
            return Err(AsioError::InvalidParameter);
        }

        let device = inner.device.as_ref().ok_or(AsioError::InvalidState)?;
        if buffer_size < 0 || u32::try_from(buffer_size).ok() != Some(device.period_size) {
            return Err(AsioError::BufferSizeNotSupported);
        }
        let input_count =
            usize::try_from(device.capture_channels).map_err(|_| AsioError::NoMemory)?;
        let output_count =
            usize::try_from(device.playback_channels).map_err(|_| AsioError::NoMemory)?;
        let mut input_active = checked_bool_buffer(input_count)?;
        let mut output_active = checked_bool_buffer(output_count)?;

        for info in infos.iter() {
            if info.channel_number < 0 {
                return Err(AsioError::InvalidParameter);
            }
            let channel =
                usize::try_from(info.channel_number).map_err(|_| AsioError::InvalidParameter)?;
            let active = if info.is_input_type != 0 {
                input_active
                    .get_mut(channel)
                    .ok_or(AsioError::InvalidParameter)?
            } else {
                output_active
                    .get_mut(channel)
                    .ok_or(AsioError::InvalidParameter)?
            };
            if *active {
                return Err(AsioError::InvalidParameter);
            }
            *active = true;
        }

        let buffers = Arc::new(HostBuffers::new(
            input_count,
            output_count,
            usize::try_from(buffer_size).map_err(|_| AsioError::NoMemory)?,
        )?);
        for info in infos.iter_mut() {
            let channel =
                usize::try_from(info.channel_number).map_err(|_| AsioError::InvalidParameter)?;
            let buffer = if info.is_input_type != 0 {
                &buffers.input[channel]
            } else {
                &buffers.output[channel]
            };
            info.buffers = buffer.pointers();
        }

        drop(inner);
        let time_info = callbacks.asio_message.is_some_and(|message| unsafe {
            message(
                ASIO_MESSAGE_SUPPORTS_TIME_INFO,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            ) != 0
        });
        let mut inner = self.lock()?;
        ensure_worker_healthy(&mut inner)?;
        if inner.state != DriverState::Initialized {
            return Err(AsioError::InvalidState);
        }
        inner.buffers = Some(buffers);
        inner.active = Some(Arc::new(ActiveChannels {
            input: input_active,
            output: output_active,
        }));
        inner.callbacks = Some(callbacks);
        inner.time_info = time_info;
        inner.state = DriverState::Prepared;
        Ok(())
    }

    pub fn start(&self) -> Result<(), AsioError> {
        let current = self.current_thread_id();
        if self.lock()?.worker_thread == Some(current) {
            return Err(AsioError::Reentrant);
        }
        let _lifecycle = self.lifecycle.lock().map_err(|_| AsioError::Worker)?;
        if self.lock()?.state == DriverState::Stopping {
            self.stop_if_running_locked()?;
        }
        if let Some(error) = self.reap_finished_worker()? {
            return Err(error);
        }
        let terminating = self
            .lock()?
            .worker
            .as_ref()
            .is_some_and(|handle| handle.stop.is_requested());
        if terminating && let Some(error) = self.terminate_worker()? {
            return Err(error);
        }
        let mut inner = self.lock()?;
        ensure_worker_healthy(&mut inner)?;
        if inner.state != DriverState::Prepared {
            return Err(AsioError::InvalidState);
        }
        if let Some(handle) = inner.worker.as_ref() {
            let gate = Arc::clone(&handle.gate);
            inner.position.publish(0, 0);
            let generation = gate.set_running(true);
            inner.state = DriverState::Running;
            drop(inner);
            if let Err(error) =
                wait_for_acknowledgement(&gate, generation, true, self.control_timeout)
            {
                self.cancel_failed_start(&gate);
                return self.complete_worker_wait(error);
            }
            return Ok(());
        }
        let thread_ops = self.thread_ops.ok_or(AsioError::Worker)?;
        let device = inner.device.as_ref().ok_or(AsioError::InvalidState)?;
        let rate = device.rate;
        let realtime_priority = c_int::try_from(device.pro_realtime_priority)
            .map_err(|_| AsioError::InvalidParameter)?;
        let shared = inner.shared.ok_or(AsioError::InvalidState)?;
        let buffers = inner.buffers.clone().ok_or(AsioError::InvalidState)?;
        let active = inner.active.clone().ok_or(AsioError::InvalidState)?;
        let callbacks = inner.callbacks.ok_or(AsioError::InvalidState)?;
        let time_info = inner.time_info;
        let stop = Arc::new(StopSignal::new()?);
        let gate = Arc::new(WorkerGate::new(true)?);
        let start_gate = Arc::clone(&gate);
        let position = Arc::clone(&inner.position);
        let period_frames =
            usize::try_from(shared.period_frames).map_err(|_| AsioError::NoMemory)?;
        let capture_samples = period_frames
            .checked_mul(usize::try_from(shared.capture_channels).map_err(|_| AsioError::NoMemory)?)
            .ok_or(AsioError::NoMemory)?;
        let playback_samples = period_frames
            .checked_mul(
                usize::try_from(shared.playback_channels).map_err(|_| AsioError::NoMemory)?,
            )
            .ok_or(AsioError::NoMemory)?;
        let sample_kernels = SampleKernels::detect(&active);
        let stream = inner.stream.take().ok_or(AsioError::NoIo)?;
        let context = Box::new(WorkerContext {
            ready: AtomicBool::new(false),
            stream: Mutex::new(Some(stream)),
            input: WorkerInput {
                stop: Arc::clone(&stop),
                gate: Arc::clone(&gate),
                buffers,
                active,
                callbacks,
                time_info,
                rate,
                realtime_priority,
                period_frames,
                capture_samples,
                playback_samples,
                sample_kernels,
                position,
            },
            error: Mutex::new(None),
        });
        let context = Box::into_raw(context);
        let mut thread_handle = ptr::null_mut();
        let mut thread_id = 0;
        let create_result =
            unsafe { (thread_ops.create)(context.cast(), &mut thread_handle, &mut thread_id) };
        if create_result != 0 || thread_handle.is_null() || thread_id == 0 {
            let context = unsafe { Box::from_raw(context) };
            inner.stream = context.take_stream();
            return Err(AsioError::Worker);
        }
        inner.worker = Some(WorkerHandle {
            stop,
            gate,
            thread_id,
            handle: thread_handle,
            context,
            thread_ops,
        });
        inner.worker_thread = Some(thread_id);
        inner.quiesce_generation = None;
        inner.position.publish(0, 0);
        inner.state = DriverState::Running;
        unsafe { &*context }.ready.store(true, Ordering::Release);
        drop(inner);
        if let Err(error) = wait_for_acknowledgement(&start_gate, 0, true, self.control_timeout) {
            self.cancel_failed_start(&start_gate);
            return self.complete_worker_wait(error);
        }
        Ok(())
    }

    pub fn stop(&self) -> Result<(), AsioError> {
        let current = self.current_thread_id();
        if self.lock()?.worker_thread == Some(current) {
            return self.stop_from_callback();
        }
        let _lifecycle = self.lifecycle.lock().map_err(|_| AsioError::Worker)?;
        if let Some(error) = self.reap_finished_worker()? {
            return Err(error);
        }
        {
            let mut inner = self.lock()?;
            ensure_worker_healthy(&mut inner)?;
        }
        self.stop_if_running_locked()
    }

    pub fn dispose_buffers(&self) -> Result<(), AsioError> {
        if self.lock()?.worker_thread == Some(self.current_thread_id()) {
            return Err(AsioError::Reentrant);
        }
        let _lifecycle = self.lifecycle.lock().map_err(|_| AsioError::Worker)?;
        self.stop_if_running_locked()?;
        if let Some(error) = self.terminate_worker()? {
            return Err(error);
        }
        let mut inner = self.lock()?;
        ensure_worker_healthy(&mut inner)?;
        if inner.state != DriverState::Prepared {
            return Err(AsioError::InvalidState);
        }
        inner.buffers = None;
        inner.active = None;
        inner.callbacks = None;
        inner.time_info = false;
        inner.state = DriverState::Initialized;
        Ok(())
    }

    pub fn close(&self) -> Result<(), AsioError> {
        let current = self.current_thread_id();
        {
            let inner = self.lock()?;
            if inner.worker_thread == Some(current) {
                return Err(AsioError::Reentrant);
            }
        }
        let _lifecycle = self.lifecycle.lock().map_err(|_| AsioError::Worker)?;
        let state = self.lock()?.state;
        if matches!(state, DriverState::Running | DriverState::Stopping)
            && let Err(error) = self.stop_locked()
        {
            self.record_error(&error);
            if !self.worker_has_exited()? {
                return Err(error);
            }
        }
        let _ = self.terminate_worker()?;
        let stream = {
            let mut inner = self.lock()?;
            if inner.state == DriverState::Destroying || inner.state == DriverState::Loaded {
                return Ok(());
            }
            if inner.state == DriverState::Prepared {
                inner.buffers = None;
                inner.active = None;
                inner.callbacks = None;
                inner.time_info = false;
            }
            inner.state = DriverState::Destroying;
            inner.stream.take()
        };
        if let Some(mut stream) = stream {
            match stream.close() {
                Ok(()) | Err(ClientError::Closed) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub fn sample_position(&self) -> Result<(u64, u64), AsioError> {
        let mut inner = self.lock()?;
        ensure_worker_healthy(&mut inner)?;
        ensure_initialized(inner.state)?;
        Ok(inner.position.load())
    }

    pub fn last_error(&self) -> Option<String> {
        let mut inner = self.inner.lock().ok()?;
        let _ = observe_worker_failure(&mut inner);
        inner.last_error.clone()
    }

    fn stop_if_running_locked(&self) -> Result<(), AsioError> {
        let state = self.lock()?.state;
        if matches!(state, DriverState::Running | DriverState::Stopping)
            && let Err(error) = self.stop_locked()
        {
            return self.complete_worker_wait(error);
        }
        Ok(())
    }

    fn stop_from_callback(&self) -> Result<(), AsioError> {
        let mut inner = self.lock()?;
        if inner.state == DriverState::Stopping {
            return Ok(());
        }
        if inner.state != DriverState::Running {
            return Err(AsioError::InvalidState);
        }
        let gate = Arc::clone(&inner.worker.as_ref().ok_or(AsioError::NoIo)?.gate);
        let generation = gate.set_running(false);
        inner.quiesce_generation = Some(generation);
        inner.state = DriverState::Stopping;
        Ok(())
    }

    fn stop_locked(&self) -> Result<(), AsioError> {
        let (gate, generation) = {
            let mut inner = self.lock()?;
            if inner.state == DriverState::Prepared {
                return Ok(());
            }
            if !matches!(inner.state, DriverState::Running | DriverState::Stopping) {
                return Err(AsioError::InvalidState);
            }
            let gate = Arc::clone(&inner.worker.as_ref().ok_or(AsioError::NoIo)?.gate);
            let generation = if inner.state == DriverState::Running {
                let generation = gate.set_running(false);
                inner.quiesce_generation = Some(generation);
                inner.state = DriverState::Stopping;
                generation
            } else {
                inner.quiesce_generation.ok_or(AsioError::Worker)?
            };
            (gate, generation)
        };
        wait_for_acknowledgement(&gate, generation, false, self.control_timeout)?;
        let mut inner = self.lock()?;
        if inner.state == DriverState::Stopping && inner.quiesce_generation == Some(generation) {
            inner.quiesce_generation = None;
            inner.state = DriverState::Prepared;
        }
        Ok(())
    }

    fn cancel_failed_start(&self, gate: &WorkerGate) {
        let generation = gate.set_running(false);
        if let Ok(mut inner) = self.inner.lock() {
            inner.quiesce_generation = Some(generation);
            inner.state = DriverState::Stopping;
        }
    }

    fn complete_worker_wait(&self, error: AsioError) -> Result<(), AsioError> {
        self.record_error(&error);
        if self.worker_has_exited()?
            && let Some(worker_error) = self.terminate_worker()?
        {
            return Err(worker_error);
        }
        Err(error)
    }

    fn worker_has_exited(&self) -> Result<bool, AsioError> {
        Ok(self
            .lock()?
            .worker
            .as_ref()
            .is_some_and(|handle| !handle.gate.is_alive()))
    }

    fn reap_finished_worker(&self) -> Result<Option<AsioError>, AsioError> {
        if self.worker_has_exited()? {
            self.terminate_worker()
        } else {
            Ok(None)
        }
    }

    fn terminate_worker(&self) -> Result<Option<AsioError>, AsioError> {
        let current = self.current_thread_id();
        let handle = {
            let mut inner = self.lock()?;
            if inner.worker_thread == Some(current) {
                return Err(AsioError::Reentrant);
            }
            let Some(handle) = inner.worker.take() else {
                inner.worker_thread = None;
                inner.quiesce_generation = None;
                if inner.state == DriverState::Stopping {
                    inner.state = DriverState::Prepared;
                }
                return Ok(None);
            };
            handle
        };
        self.finish_worker(handle)
    }

    fn finish_worker(&self, handle: WorkerHandle) -> Result<Option<AsioError>, AsioError> {
        handle.stop.request();
        if unsafe { (handle.thread_ops.join)(handle.handle, duration_millis(self.control_timeout)) }
            != 0
        {
            let mut inner = self.lock()?;
            inner.worker = Some(handle);
            let error = AsioError::WorkerTimeout;
            inner.last_error = Some(error.to_string());
            return Err(error);
        }
        let context = unsafe { Box::from_raw(handle.context) };
        let (stream, error) = context.into_exit();
        let mut inner = self.lock()?;
        inner.stream = stream;
        inner.worker_thread = None;
        inner.quiesce_generation = None;
        inner.state = DriverState::Prepared;
        if let Some(error) = error.as_ref() {
            inner.worker_failure = Some(WorkerFailure::from_error(error));
            inner.last_error = Some(error.to_string());
        }
        Ok(error)
    }

    fn record_error(&self, error: &AsioError) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.last_error = Some(error.to_string());
        }
    }

    fn current_thread_id(&self) -> u32 {
        self.thread_ops
            .map_or(0, |ops| unsafe { (ops.current_thread_id)() })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, DriverInner>, AsioError> {
        self.inner.lock().map_err(|_| AsioError::Worker)
    }
}

impl Default for AsioDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AsioDriver {
    fn drop(&mut self) {
        let current = self.current_thread_id();
        let handle = self
            .inner
            .get_mut()
            .ok()
            .and_then(|inner| inner.worker.take());
        if let Some(handle) = handle {
            handle.stop.request();
            if handle.thread_id != current
                && unsafe { (handle.thread_ops.join)(handle.handle, 0) } == 0
            {
                let context = unsafe { Box::from_raw(handle.context) };
                if let Some(mut stream) = context.take_stream() {
                    let _ = stream.close();
                }
            } else {
                // A live worker still owns its context and host buffers. Leaking this small
                // handle is safer than freeing memory that the callback thread can still use.
                std::mem::forget(handle);
            }
        }
        if let Ok(inner) = self.inner.get_mut()
            && let Some(mut stream) = inner.stream.take()
        {
            let _ = stream.close();
        }
    }
}

struct WorkerHandle {
    stop: Arc<StopSignal>,
    gate: Arc<WorkerGate>,
    thread_id: u32,
    handle: *mut c_void,
    context: *mut WorkerContext,
    thread_ops: ThreadOps,
}

unsafe impl Send for WorkerHandle {}

struct WorkerInput {
    stop: Arc<StopSignal>,
    gate: Arc<WorkerGate>,
    buffers: Arc<HostBuffers>,
    active: Arc<ActiveChannels>,
    callbacks: AsioCallbacks,
    time_info: bool,
    rate: u32,
    realtime_priority: c_int,
    period_frames: usize,
    capture_samples: usize,
    playback_samples: usize,
    sample_kernels: SampleKernels,
    position: Arc<PositionSnapshot>,
}

struct RealtimeGuard {
    policy: libc::c_int,
    priority: libc::c_int,
}

impl RealtimeGuard {
    fn enter(priority: libc::c_int) -> Result<Self, AsioError> {
        let policy = unsafe { libc::sched_getscheduler(0) };
        if policy < 0 {
            return Err(AsioError::RealtimeScheduling(
                std::io::Error::last_os_error(),
            ));
        }
        let mut parameters = libc::sched_param { sched_priority: 0 };
        if unsafe { libc::sched_getparam(0, &mut parameters) } != 0 {
            return Err(AsioError::RealtimeScheduling(
                std::io::Error::last_os_error(),
            ));
        }
        let realtime = libc::sched_param {
            sched_priority: priority,
        };
        if unsafe {
            libc::sched_setscheduler(0, libc::SCHED_FIFO | libc::SCHED_RESET_ON_FORK, &realtime)
        } != 0
        {
            return Err(AsioError::RealtimeScheduling(
                std::io::Error::last_os_error(),
            ));
        }
        Ok(Self {
            policy,
            priority: parameters.sched_priority,
        })
    }
}

impl Drop for RealtimeGuard {
    fn drop(&mut self) {
        let parameters = libc::sched_param {
            sched_priority: self.priority,
        };
        unsafe {
            let _ = libc::sched_setscheduler(0, self.policy, &parameters);
        }
    }
}

struct WorkerContext {
    ready: AtomicBool,
    stream: Mutex<Option<AudioStream>>,
    input: WorkerInput,
    error: Mutex<Option<AsioError>>,
}

impl WorkerContext {
    fn take_stream(&self) -> Option<AudioStream> {
        self.stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn into_exit(self) -> (Option<AudioStream>, Option<AsioError>) {
        let stream = self
            .stream
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let error = self
            .error
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (stream, error)
    }
}

struct ActiveChannels {
    input: Box<[bool]>,
    output: Box<[bool]>,
}

#[repr(C, align(32))]
#[derive(Clone, Copy)]
struct AlignedSampleBlock([f32; 8]);

struct ChannelBuffer {
    data: std::cell::UnsafeCell<Box<[AlignedSampleBlock]>>,
    period: usize,
    half_stride: usize,
}

// Host owns one half while worker owns other half; callback ordering provides synchronization.
unsafe impl Send for ChannelBuffer {}
unsafe impl Sync for ChannelBuffer {}

impl ChannelBuffer {
    fn new(period: usize) -> Result<Self, AsioError> {
        let half_stride = period.checked_add(7).ok_or(AsioError::NoMemory)? / 8 * 8;
        let blocks = half_stride.checked_mul(2).ok_or(AsioError::NoMemory)? / 8;
        let mut data = Vec::new();
        data.try_reserve_exact(blocks)
            .map_err(|_| AsioError::NoMemory)?;
        data.resize(blocks, AlignedSampleBlock([0.0; 8]));
        Ok(Self {
            data: std::cell::UnsafeCell::new(data.into_boxed_slice()),
            period,
            half_stride,
        })
    }

    fn pointers(&self) -> [*mut c_void; 2] {
        unsafe {
            let data = &mut *self.data.get();
            let samples = data.as_mut_ptr().cast::<f32>();
            [samples.cast(), samples.add(self.half_stride).cast()]
        }
    }

    fn half(&self, index: usize) -> &[f32] {
        unsafe {
            let data = &*self.data.get();
            let samples = std::slice::from_raw_parts(data.as_ptr().cast::<f32>(), data.len() * 8);
            let start = index
                .checked_mul(self.half_stride)
                .expect("channel buffer index overflow");
            let end = start
                .checked_add(self.period)
                .expect("channel buffer index overflow");
            &samples[start..end]
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn half_mut(&self, index: usize) -> &mut [f32] {
        unsafe {
            let data = &mut *self.data.get();
            let sample_count = data.len() * 8;
            let samples = std::slice::from_raw_parts_mut(data.as_mut_ptr().cast(), sample_count);
            let start = index
                .checked_mul(self.half_stride)
                .expect("channel buffer index overflow");
            let end = start
                .checked_add(self.period)
                .expect("channel buffer index overflow");
            &mut samples[start..end]
        }
    }
}

struct HostBuffers {
    input: Box<[ChannelBuffer]>,
    output: Box<[ChannelBuffer]>,
}

impl HostBuffers {
    fn new(input_count: usize, output_count: usize, period: usize) -> Result<Self, AsioError> {
        let mut input = Vec::new();
        input
            .try_reserve_exact(input_count)
            .map_err(|_| AsioError::NoMemory)?;
        for _ in 0..input_count {
            input.push(ChannelBuffer::new(period)?);
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(output_count)
            .map_err(|_| AsioError::NoMemory)?;
        for _ in 0..output_count {
            output.push(ChannelBuffer::new(period)?);
        }
        Ok(Self {
            input: input.into_boxed_slice(),
            output: output.into_boxed_slice(),
        })
    }
}

struct WorkerGate {
    fd: RawFd,
    command: AtomicU64,
    running_acknowledged: AtomicU64,
    stopped_acknowledged: AtomicU64,
    failure: AtomicU64,
    alive: AtomicBool,
}

impl WorkerGate {
    fn new(running: bool) -> Result<Self, AsioError> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(AsioError::Client(ClientError::Io(
                std::io::Error::last_os_error(),
            )));
        }
        Ok(Self {
            fd,
            command: AtomicU64::new(u64::from(running)),
            running_acknowledged: AtomicU64::new(u64::MAX),
            stopped_acknowledged: AtomicU64::new(u64::MAX),
            failure: AtomicU64::new(0),
            alive: AtomicBool::new(true),
        })
    }

    fn set_running(&self, running: bool) -> u64 {
        let mut current = self.command.load(Ordering::Acquire);
        loop {
            if current & 1 == u64::from(running) {
                return current >> 1;
            }
            let generation = (current >> 1).wrapping_add(1) & (u64::MAX >> 1);
            let next = generation << 1 | u64::from(running);
            match self.command.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.notify();
                    return generation;
                }
                Err(observed) => current = observed,
            }
        }
    }

    fn command(&self) -> (u64, bool) {
        let command = self.command.load(Ordering::Acquire);
        (command >> 1, command & 1 != 0)
    }

    fn acknowledge(&self, generation: u64, running: bool) {
        let acknowledged = if running {
            &self.running_acknowledged
        } else {
            &self.stopped_acknowledged
        };
        acknowledged.store(generation, Ordering::Release);
    }

    fn acknowledged(&self, running: bool) -> u64 {
        let acknowledged = if running {
            &self.running_acknowledged
        } else {
            &self.stopped_acknowledged
        };
        acknowledged.load(Ordering::Acquire)
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn failure(&self) -> Option<WorkerFailure> {
        WorkerFailure::from_raw(self.failure.load(Ordering::Acquire))
    }

    fn publish_failure(&self, failure: WorkerFailure) {
        self.failure.store(failure as u64, Ordering::Release);
        self.notify();
    }

    fn mark_dead(&self) {
        self.alive.store(false, Ordering::Release);
    }

    fn notify(&self) {
        let value = 1_u64;
        unsafe {
            let _ = libc::write(
                self.fd,
                (&value as *const u64).cast(),
                std::mem::size_of::<u64>(),
            );
        }
    }

    fn drain(&self) {
        let mut value = 0_u64;
        unsafe {
            let _ = libc::read(
                self.fd,
                (&mut value as *mut u64).cast(),
                std::mem::size_of::<u64>(),
            );
        }
    }
}

impl Drop for WorkerGate {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

fn wait_for_acknowledgement(
    gate: &WorkerGate,
    generation: u64,
    running: bool,
    timeout: Duration,
) -> Result<(), AsioError> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    while gate.acknowledged(running) != generation {
        if let Some(failure) = gate.failure() {
            return Err(failure.error());
        }
        if !gate.is_alive() {
            return Err(AsioError::Worker);
        }
        if Instant::now() >= deadline {
            return Err(AsioError::WorkerTimeout);
        }
        thread::sleep(Duration::from_micros(50));
    }
    if let Some(failure) = gate.failure() {
        return Err(failure.error());
    }
    if !gate.is_alive() {
        return Err(AsioError::Worker);
    }
    Ok(())
}

struct StopSignal {
    fd: RawFd,
    requested: AtomicBool,
}

impl StopSignal {
    fn new() -> Result<Self, AsioError> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(AsioError::Client(ClientError::Io(
                std::io::Error::last_os_error(),
            )));
        }
        Ok(Self {
            fd,
            requested: AtomicBool::new(false),
        })
    }

    fn request(&self) {
        if !self.requested.swap(true, Ordering::Release) {
            let value = 1_u64;
            unsafe {
                let _ = libc::write(
                    self.fd,
                    (&value as *const u64).cast(),
                    std::mem::size_of::<u64>(),
                );
            }
        }
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

unsafe impl Send for StopSignal {}
unsafe impl Sync for StopSignal {}

impl Drop for StopSignal {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

fn run_worker(stream: &mut AudioStream, input: &WorkerInput) -> Result<(), AsioError> {
    let mut capture = checked_i32_buffer(input.capture_samples)?;
    let mut playback = checked_i32_buffer(input.playback_samples)?;
    let audio_fd = unsafe { OwnedFd::from_raw_fd(stream.notification_fd()?) };
    let control_fd = unsafe { OwnedFd::from_raw_fd(stream.control_fd()?) };
    let realtime = if input.realtime_priority == 0 {
        None
    } else {
        match RealtimeGuard::enter(input.realtime_priority) {
            Ok(realtime) => Some(realtime),
            Err(_) => {
                stream.record_realtime_failure();
                None
            }
        }
    };
    let result = match stream.start() {
        Ok(()) => catch_unwind(AssertUnwindSafe(|| {
            worker_loop(
                stream,
                audio_fd.as_raw_fd(),
                control_fd.as_raw_fd(),
                &input.stop,
                &input.gate,
                &input.buffers,
                &input.active,
                input.callbacks,
                input.time_info,
                input.rate,
                input.period_frames,
                &mut capture,
                &mut playback,
                input.sample_kernels,
                &input.position,
            )
        }))
        .unwrap_or(Err(AsioError::Worker)),
        Err(error) => Err(error.into()),
    };
    drop(realtime);
    let stop_result = stream.stop().map_err(AsioError::from);
    match result {
        Err(error) => Err(error),
        Ok(()) => stop_result,
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    stream: &mut AudioStream,
    audio_fd: RawFd,
    control_fd: RawFd,
    stop: &StopSignal,
    gate: &WorkerGate,
    buffers: &HostBuffers,
    active: &ActiveChannels,
    callbacks: AsioCallbacks,
    time_info: bool,
    rate: u32,
    period_frames: usize,
    capture: &mut [i32],
    playback: &mut [i32],
    sample_kernels: SampleKernels,
    position: &PositionSnapshot,
) -> Result<(), AsioError> {
    let mut buffer_index = 0_usize;
    let mut previous_sequence = None;
    let mut run_generation = u64::MAX;
    let mut last_active_sequence = None;
    let mut stopping_after = None;
    macro_rules! recover_discontinuity {
        ($result:expr) => {
            match $result.map_err(AsioError::from) {
                Ok(value) => value,
                Err(error) if is_recoverable_discontinuity(&error) => {
                    recover_worker_discontinuity(
                        stream,
                        gate,
                        callbacks,
                        &mut run_generation,
                        &mut buffer_index,
                        &mut previous_sequence,
                        &mut last_active_sequence,
                        &mut stopping_after,
                    )?;
                    continue;
                }
                Err(error) => return Err(error),
            }
        };
    }
    while !stop.is_requested() {
        let _ = apply_gate_transition(
            gate,
            &mut run_generation,
            &mut buffer_index,
            &mut previous_sequence,
            &mut last_active_sequence,
            &mut stopping_after,
        );
        let acquisition_generation = run_generation;
        let sequence = match recover_discontinuity!(wait_worker_event(
            stream, audio_fd, control_fd, stop, gate
        )) {
            WorkerEvent::Cycle(sequence) => sequence,
            WorkerEvent::Gate => continue,
            WorkerEvent::Stop => break,
        };
        let Some(block_sequence) =
            recover_discontinuity!(stream.capture_buffer_for_sequence(sequence, capture))
        else {
            continue;
        };
        check_daemon_control(control_fd)?;
        let running = apply_gate_transition(
            gate,
            &mut run_generation,
            &mut buffer_index,
            &mut previous_sequence,
            &mut last_active_sequence,
            &mut stopping_after,
        );
        if !running {
            playback.fill(0);
            let _ = recover_discontinuity!(stream.submit_playback(block_sequence, playback));
            if stopping_after
                .is_some_and(|sequence| sequence_is_after(stream.playback_sequence(), sequence))
            {
                gate.acknowledge(run_generation, false);
            }
            continue;
        }
        if !acquired_block_is_current(acquisition_generation, run_generation) {
            playback.fill(0);
            let _ = recover_discontinuity!(stream.submit_playback(block_sequence, playback));
            continue;
        }
        let (next_buffer_index, samples) = match advance_asio_cycle(
            previous_sequence,
            block_sequence,
            u64::try_from(period_frames).unwrap_or(0),
            buffer_index,
            position.load().0,
        ) {
            CycleAdvance::Ready {
                buffer_index,
                sample_position,
            } => (buffer_index, sample_position),
            CycleAdvance::Duplicate => {
                playback.fill(0);
                let _ = recover_discontinuity!(stream.submit_playback(block_sequence, playback));
                continue;
            }
            CycleAdvance::Backward => {
                recover_worker_discontinuity(
                    stream,
                    gate,
                    callbacks,
                    &mut run_generation,
                    &mut buffer_index,
                    &mut previous_sequence,
                    &mut last_active_sequence,
                    &mut stopping_after,
                )?;
                continue;
            }
        };
        buffer_index = next_buffer_index;
        previous_sequence = Some(block_sequence);
        copy_capture_to_host(capture, buffers, active, buffer_index, period_frames);
        let stamp = monotonic_nanos();
        position.publish(samples, stamp);
        let callback_start = stamp;
        invoke_callback(callbacks, time_info, buffer_index, samples, stamp, rate)?;
        let callback_duration = monotonic_nanos().saturating_sub(callback_start);
        let period_nanos = u64::try_from(period_frames)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            / u64::from(rate);
        stream.record_callback_timing(callback_duration, period_nanos);
        // Kernel selection proved its target features and buffer geometry before worker startup.
        unsafe {
            (sample_kernels.copy_host_to_playback)(
                buffers,
                active,
                buffer_index,
                playback,
                period_frames,
            )
        };
        if !recover_discontinuity!(stream.submit_playback(block_sequence, playback)) {
            continue;
        }
        last_active_sequence = Some(block_sequence);
    }
    Ok(())
}

fn is_recoverable_discontinuity(error: &AsioError) -> bool {
    matches!(
        error,
        AsioError::Client(ClientError::CaptureDiscontinuity) | AsioError::CaptureSequenceBackward
    )
}

trait WorkerStreamRecovery {
    fn restart_after_discontinuity(&mut self) -> Result<(), AsioError>;
}

impl WorkerStreamRecovery for AudioStream {
    fn restart_after_discontinuity(&mut self) -> Result<(), AsioError> {
        self.prepare()?;
        self.start()?;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn recover_worker_discontinuity<S: WorkerStreamRecovery>(
    stream: &mut S,
    gate: &WorkerGate,
    callbacks: AsioCallbacks,
    run_generation: &mut u64,
    buffer_index: &mut usize,
    previous_sequence: &mut Option<u64>,
    last_active_sequence: &mut Option<u64>,
    stopping_after: &mut Option<u64>,
) -> Result<(), AsioError> {
    stream.restart_after_discontinuity()?;
    *buffer_index = 0;
    *previous_sequence = None;
    *last_active_sequence = None;
    *stopping_after = None;
    let (generation, running) = gate.command();
    *run_generation = generation;
    gate.acknowledge(generation, running);
    if running {
        notify_worker_failure(callbacks, WorkerFailure::CaptureDiscontinuity);
    }
    Ok(())
}

fn acquired_block_is_current(acquisition_generation: u64, run_generation: u64) -> bool {
    acquisition_generation == run_generation
}

fn apply_gate_transition(
    gate: &WorkerGate,
    run_generation: &mut u64,
    buffer_index: &mut usize,
    previous_sequence: &mut Option<u64>,
    last_active_sequence: &mut Option<u64>,
    stopping_after: &mut Option<u64>,
) -> bool {
    let (generation, running) = gate.command();
    if generation == *run_generation {
        return running;
    }
    *run_generation = generation;
    if running {
        *buffer_index = 0;
        *previous_sequence = None;
        *last_active_sequence = None;
        *stopping_after = None;
        gate.acknowledge(generation, true);
    } else {
        *stopping_after = *last_active_sequence;
        if stopping_after.is_none() {
            gate.acknowledge(generation, false);
        }
    }
    running
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CycleAdvance {
    Ready {
        buffer_index: usize,
        sample_position: u64,
    },
    Duplicate,
    Backward,
}

fn advance_asio_cycle(
    previous_sequence: Option<u64>,
    sequence: u64,
    period_frames: u64,
    buffer_index: usize,
    sample_position: u64,
) -> CycleAdvance {
    let Some(previous) = previous_sequence else {
        return CycleAdvance::Ready {
            buffer_index,
            sample_position,
        };
    };
    let elapsed_periods = sequence.wrapping_sub(previous);
    if elapsed_periods == 0 {
        return CycleAdvance::Duplicate;
    }
    if elapsed_periods >= (1_u64 << 63) {
        return CycleAdvance::Backward;
    }
    CycleAdvance::Ready {
        buffer_index: buffer_index ^ (elapsed_periods as usize & 1),
        sample_position: sample_position
            .saturating_add(period_frames.saturating_mul(elapsed_periods)),
    }
}

fn sequence_is_after(candidate: u64, reference: u64) -> bool {
    let distance = candidate.wrapping_sub(reference);
    distance != 0 && distance < (1_u64 << 63)
}

enum WorkerEvent {
    Cycle(u64),
    Gate,
    Stop,
}

fn wait_worker_event(
    stream: &mut AudioStream,
    audio_fd: RawFd,
    control_fd: RawFd,
    stop: &StopSignal,
    gate: &WorkerGate,
) -> Result<WorkerEvent, AsioError> {
    let mut fds = [
        libc::pollfd {
            fd: audio_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stop.fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: gate.fd,
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
        if stop.is_requested() {
            return Ok(WorkerEvent::Stop);
        }
        match stream.wait_period(Duration::ZERO) {
            Ok(sequence) => {
                check_daemon_control(control_fd)?;
                return Ok(WorkerEvent::Cycle(sequence));
            }
            Err(ClientError::Timeout) => {}
            Err(error) => return Err(error.into()),
        }
        for fd in &mut fds {
            fd.revents = 0;
        }
        let result = unsafe {
            libc::poll(
                fds.as_mut_ptr(),
                fds.len() as libc::nfds_t,
                WORKER_POLL_HEARTBEAT_MS,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(ClientError::Io(error).into());
        }
        if fds[1].revents & libc::POLLIN != 0 || stop.is_requested() {
            return Ok(WorkerEvent::Stop);
        }
        if fds[1].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(AsioError::WorkerStopped("worker stop signal failed"));
        }
        if fds[2].revents & libc::POLLIN != 0 {
            gate.drain();
            return Ok(WorkerEvent::Gate);
        }
        if fds[2].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(AsioError::WorkerStopped("worker gate failed"));
        }
        if daemon_control_failed(control_fd, fds[3].revents)? {
            return Err(AsioError::DaemonDisconnected);
        }
        if fds[0].revents & libc::POLLIN != 0 {
            continue;
        }
        if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(AsioError::WorkerStopped("audio notification failed"));
        }
    }
}

fn check_daemon_control(fd: RawFd) -> Result<(), AsioError> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLRDHUP,
        revents: 0,
    };
    loop {
        let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
        if result > 0 {
            return if daemon_control_failed(fd, pollfd.revents)? {
                Err(AsioError::DaemonDisconnected)
            } else {
                Ok(())
            };
        }
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(ClientError::Io(error).into());
        }
        pollfd.revents = 0;
    }
}

fn daemon_control_failed(fd: RawFd, revents: libc::c_short) -> Result<bool, AsioError> {
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
            return Err(AsioError::WorkerStopped(
                "unexpected daemon control response",
            ));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
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
        return Err(ClientError::Io(error).into());
    }
}

fn copy_capture_to_host(
    capture: &[i32],
    buffers: &HostBuffers,
    active: &ActiveChannels,
    index: usize,
    period_frames: usize,
) {
    let channels = active.input.len();
    for (channel, is_active) in active.input.iter().copied().enumerate() {
        if !is_active {
            continue;
        }
        let destination = buffers.input[channel].half_mut(index);
        for frame in 0..period_frames {
            destination[frame] = s32_to_f32(capture[frame * channels + channel]);
        }
    }
}

type CopyHostToPlayback = unsafe fn(&HostBuffers, &ActiveChannels, usize, &mut [i32], usize);

#[derive(Clone, Copy)]
struct SampleKernels {
    copy_host_to_playback: CopyHostToPlayback,
}

impl SampleKernels {
    const fn scalar() -> Self {
        Self {
            copy_host_to_playback: copy_host_to_playback_scalar,
        }
    }

    fn detect(active: &ActiveChannels) -> Self {
        #[cfg(target_arch = "x86_64")]
        if active.output.len() == 8
            && active.output.iter().all(|active| *active)
            && std::arch::is_x86_feature_detected!("avx2")
        {
            unsafe { prewarm_avx2() };
            return Self {
                copy_host_to_playback: copy_host_to_playback_avx2,
            };
        }
        Self::scalar()
    }
}

fn copy_host_to_playback_scalar(
    buffers: &HostBuffers,
    active: &ActiveChannels,
    index: usize,
    playback: &mut [i32],
    period_frames: usize,
) {
    playback.fill(0);
    let channels = active.output.len();
    for (channel, is_active) in active.output.iter().copied().enumerate() {
        if !is_active {
            continue;
        }
        let source = buffers.output[channel].half(index);
        for frame in 0..period_frames {
            playback[frame * channels + channel] = f32_to_s32(source[frame]);
        }
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn copy_host_to_playback_avx2(
    buffers: &HostBuffers,
    active: &ActiveChannels,
    index: usize,
    playback: &mut [i32],
    period_frames: usize,
) {
    unsafe {
        copy_host_to_playback_avx2_inner(buffers, active, index, playback, period_frames);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn copy_host_to_playback_avx2_inner(
    buffers: &HostBuffers,
    active: &ActiveChannels,
    index: usize,
    playback: &mut [i32],
    period_frames: usize,
) {
    use std::arch::x86_64::*;

    debug_assert_eq!(active.output.len(), 8);
    debug_assert!(active.output.iter().all(|active| *active));
    debug_assert!(playback.len() >= period_frames * 8);
    let vector_frames = period_frames / 8 * 8;
    for frame in (0..vector_frames).step_by(8) {
        let rows = std::array::from_fn(|channel| unsafe {
            convert_f32x8_to_s32(buffers.output[channel].half(index).as_ptr().add(frame))
        });
        let frames = unsafe { transpose_8x8_i32(rows) };
        for (lane, values) in frames.into_iter().enumerate() {
            unsafe {
                _mm256_storeu_si256(playback.as_mut_ptr().add((frame + lane) * 8).cast(), values);
            }
        }
    }
    for frame in vector_frames..period_frames {
        for channel in 0..8 {
            playback[frame * 8 + channel] = f32_to_s32(buffers.output[channel].half(index)[frame]);
        }
    }
    _mm256_zeroupper();
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn convert_f32x8_to_s32(source: *const f32) -> std::arch::x86_64::__m256i {
    use std::arch::x86_64::*;

    unsafe {
        let samples = _mm256_loadu_ps(source);
        let zero = _mm256_setzero_ps();
        let one = _mm256_set1_ps(1.0);
        let negative_one = _mm256_set1_ps(-1.0);
        let ordered = _mm256_cmp_ps(samples, samples, _CMP_ORD_Q);
        let above_min = _mm256_cmp_ps(samples, negative_one, _CMP_GT_OQ);
        let below_max = _mm256_cmp_ps(samples, one, _CMP_LT_OQ);
        let interior = _mm256_and_ps(ordered, _mm256_and_ps(above_min, below_max));
        let safe_samples = _mm256_blendv_ps(zero, samples, interior);

        let low = _mm256_cvtps_pd(_mm256_castps256_ps128(safe_samples));
        let high = _mm256_cvtps_pd(_mm256_extractf128_ps(safe_samples, 1));
        let scale = _mm256_set1_pd(2_147_483_648.0);
        let half = _mm256_set1_pd(0.5);
        let negative_half = _mm256_set1_pd(-0.5);
        let zero_pd = _mm256_setzero_pd();

        let low_scaled = _mm256_mul_pd(low, scale);
        let high_scaled = _mm256_mul_pd(high, scale);
        let low_adjustment = _mm256_blendv_pd(
            half,
            negative_half,
            _mm256_cmp_pd(low_scaled, zero_pd, _CMP_LT_OQ),
        );
        let high_adjustment = _mm256_blendv_pd(
            half,
            negative_half,
            _mm256_cmp_pd(high_scaled, zero_pd, _CMP_LT_OQ),
        );
        let low_i32 = _mm256_cvttpd_epi32(_mm256_add_pd(low_scaled, low_adjustment));
        let high_i32 = _mm256_cvttpd_epi32(_mm256_add_pd(high_scaled, high_adjustment));
        let interior_i32 = _mm256_inserti128_si256(_mm256_castsi128_si256(low_i32), high_i32, 1);

        let minimum_mask = _mm256_castps_si256(_mm256_cmp_ps(samples, negative_one, _CMP_LE_OQ));
        let maximum_mask = _mm256_castps_si256(_mm256_cmp_ps(samples, one, _CMP_GE_OQ));
        let nan_mask = _mm256_castps_si256(_mm256_cmp_ps(samples, samples, _CMP_UNORD_Q));
        let with_minimum =
            _mm256_blendv_epi8(interior_i32, _mm256_set1_epi32(i32::MIN), minimum_mask);
        let with_maximum =
            _mm256_blendv_epi8(with_minimum, _mm256_set1_epi32(i32::MAX), maximum_mask);
        _mm256_blendv_epi8(with_maximum, _mm256_setzero_si256(), nan_mask)
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn transpose_8x8_i32(
    rows: [std::arch::x86_64::__m256i; 8],
) -> [std::arch::x86_64::__m256i; 8] {
    use std::arch::x86_64::*;

    let [r0, r1, r2, r3, r4, r5, r6, r7] = rows;
    let a0 = _mm256_unpacklo_epi32(r0, r1);
    let a1 = _mm256_unpackhi_epi32(r0, r1);
    let a2 = _mm256_unpacklo_epi32(r2, r3);
    let a3 = _mm256_unpackhi_epi32(r2, r3);
    let a4 = _mm256_unpacklo_epi32(r4, r5);
    let a5 = _mm256_unpackhi_epi32(r4, r5);
    let a6 = _mm256_unpacklo_epi32(r6, r7);
    let a7 = _mm256_unpackhi_epi32(r6, r7);

    let b0 = _mm256_unpacklo_epi64(a0, a2);
    let b1 = _mm256_unpackhi_epi64(a0, a2);
    let b2 = _mm256_unpacklo_epi64(a1, a3);
    let b3 = _mm256_unpackhi_epi64(a1, a3);
    let b4 = _mm256_unpacklo_epi64(a4, a6);
    let b5 = _mm256_unpackhi_epi64(a4, a6);
    let b6 = _mm256_unpacklo_epi64(a5, a7);
    let b7 = _mm256_unpackhi_epi64(a5, a7);

    [
        _mm256_permute2x128_si256(b0, b4, 0x20),
        _mm256_permute2x128_si256(b1, b5, 0x20),
        _mm256_permute2x128_si256(b2, b6, 0x20),
        _mm256_permute2x128_si256(b3, b7, 0x20),
        _mm256_permute2x128_si256(b0, b4, 0x31),
        _mm256_permute2x128_si256(b1, b5, 0x31),
        _mm256_permute2x128_si256(b2, b6, 0x31),
        _mm256_permute2x128_si256(b3, b7, 0x31),
    ]
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn prewarm_avx2() {
    use std::arch::x86_64::*;

    let source = [0.0_f32; 8];
    let mut destination = [0_i32; 8];
    unsafe {
        let converted = convert_f32x8_to_s32(source.as_ptr());
        _mm256_storeu_si256(destination.as_mut_ptr().cast(), converted);
        _mm256_zeroupper();
    }
    std::hint::black_box(destination);
}

fn invoke_callback(
    callbacks: AsioCallbacks,
    time_info_supported: bool,
    index: usize,
    samples: u64,
    stamp: u64,
    rate: u32,
) -> Result<(), AsioError> {
    let call = || unsafe {
        if time_info_supported {
            if let Some(callback) = callbacks.buffer_switch_time_info {
                let mut info = AsioTimeInfo {
                    reserved: [0; 4],
                    speed: 1.0,
                    time_stamp: split_i64(stamp),
                    sample_position: split_i64(samples),
                    sample_rate: f64::from(rate),
                    flags: 0x7,
                    reserved_2: [0; 12],
                    speed_for_time_code: 0.0,
                    time_code: AsioInt64 { hi: 0, lo: 0 },
                    time_code_flags: 0,
                    reserved_3: [0; 64],
                };
                let _ = callback(&mut info, index as c_int, 1);
            } else if let Some(callback) = callbacks.buffer_switch {
                callback(index as c_int, 1);
            }
        } else if let Some(callback) = callbacks.buffer_switch {
            callback(index as c_int, 1);
        }
    };
    catch_unwind(AssertUnwindSafe(call)).map_err(|_| AsioError::CallbackPanic)
}

fn notify_worker_failure(callbacks: AsioCallbacks, failure: WorkerFailure) {
    let (Some(callback), Some(selector)) = (callbacks.asio_message, failure.notification()) else {
        return;
    };
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        let _ = callback(selector, 0, ptr::null_mut(), ptr::null_mut());
    }));
}

fn s32_to_f32(sample: i32) -> f32 {
    sample as f32 / 2_147_483_648.0
}

fn f32_to_s32(sample: f32) -> i32 {
    if sample.is_nan() {
        return 0;
    }
    if sample <= -1.0 {
        return i32::MIN;
    }
    if sample >= 1.0 {
        return i32::MAX;
    }
    let scaled = f64::from(sample) * 2_147_483_648.0;
    let adjustment = if scaled.is_sign_negative() { -0.5 } else { 0.5 };
    (scaled + adjustment) as i32
}

fn split_i64(value: u64) -> AsioInt64 {
    AsioInt64 {
        hi: (value >> 32) as u32,
        lo: value as u32,
    }
}

fn monotonic_nanos() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    if result != 0 {
        return 0;
    }
    u64::try_from(time.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|seconds| {
            u64::try_from(time.tv_nsec)
                .ok()
                .and_then(|nanos| seconds.checked_add(nanos))
        })
        .unwrap_or(0)
}

fn checked_bool_buffer(length: usize) -> Result<Box<[bool]>, AsioError> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(length)
        .map_err(|_| AsioError::NoMemory)?;
    buffer.resize(length, false);
    Ok(buffer.into_boxed_slice())
}

fn checked_i32_buffer(length: usize) -> Result<Box<[i32]>, AsioError> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(length)
        .map_err(|_| AsioError::NoMemory)?;
    buffer.resize(length, 0);
    Ok(buffer.into_boxed_slice())
}

fn write_channel_name(destination: &mut [c_char; 32], prefix: &[u8], mut number: usize) {
    let mut offset = 0;
    for byte in prefix.iter().copied().take(destination.len() - 1) {
        destination[offset] = byte as c_char;
        offset += 1;
    }
    let mut digits = [0_u8; 20];
    let mut digit_count = 0;
    loop {
        digits[digit_count] = b'0' + (number % 10) as u8;
        digit_count += 1;
        number /= 10;
        if number == 0 {
            break;
        }
    }
    for byte in digits[..digit_count].iter().rev().copied() {
        if offset == destination.len() - 1 {
            break;
        }
        destination[offset] = byte as c_char;
        offset += 1;
    }
}

fn duration_millis(duration: Duration) -> u32 {
    duration
        .as_millis()
        .saturating_add(u128::from(
            !duration.subsec_nanos().is_multiple_of(1_000_000),
        ))
        .min(u128::from(u32::MAX)) as u32
}

fn observe_worker_failure(inner: &mut DriverInner) -> Option<WorkerFailure> {
    let failure = inner.worker_failure.or_else(|| {
        inner
            .worker
            .as_ref()
            .and_then(|handle| handle.gate.failure())
    });
    if let Some(failure) = failure
        && inner.worker_failure.is_none()
    {
        inner.worker_failure = Some(failure);
        inner.last_error = Some(failure.error().to_string());
    }
    failure
}

fn ensure_worker_healthy(inner: &mut DriverInner) -> Result<(), AsioError> {
    observe_worker_failure(inner).map_or(Ok(()), |failure| Err(failure.error()))
}

fn ensure_initialized(state: DriverState) -> Result<(), AsioError> {
    if matches!(
        state,
        DriverState::Initialized
            | DriverState::Prepared
            | DriverState::Running
            | DriverState::Stopping
    ) {
        Ok(())
    } else {
        Err(AsioError::InvalidState)
    }
}

fn checked_c_int(value: u32) -> Result<c_int, AsioError> {
    c_int::try_from(value).map_err(|_| AsioError::InvalidParameter)
}

fn status(result: Result<(), AsioError>) -> c_int {
    result.map_or_else(|error| error.code(), |_| ASIO_SUCCESS)
}

fn with_driver<F>(driver: *mut AsioDriver, function: F) -> c_int
where
    F: FnOnce(&AsioDriver) -> Result<(), AsioError>,
{
    if driver.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    catch_unwind(AssertUnwindSafe(|| unsafe { function(&*driver) }))
        .map_or(ASIO_HW_MALFUNCTION, status)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `context` must point to a live `WorkerContext` until its thread is joined.
pub unsafe extern "C" fn sidealsa_asio_worker_entry(context: *mut c_void) {
    let Some(context) = (unsafe { (context.cast::<WorkerContext>()).as_ref() }) else {
        return;
    };
    while !context.ready.load(Ordering::Acquire) {
        thread::yield_now();
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut stream = context
            .stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stream = stream.as_mut().ok_or(AsioError::Worker)?;
        run_worker(stream, &context.input)
    }));
    let error = match result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(_) => Some(AsioError::Worker),
    };
    let failure = error.as_ref().map(WorkerFailure::from_error);
    *context
        .error
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = error;
    if let Some(failure) = failure {
        context.input.gate.publish_failure(failure);
        let (_, callbacks_running) = context.input.gate.command();
        if callbacks_running && !context.input.stop.is_requested() {
            notify_worker_failure(context.input.callbacks, failure);
        }
    }
    context.input.gate.mark_dead();
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `thread_ops` must point to a valid function table and `out` must be writable.
pub unsafe extern "C" fn sidealsa_asio_new(
    thread_ops: *const AsioThreadOps,
    out: *mut *mut AsioDriver,
) -> c_int {
    if out.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    unsafe {
        *out = ptr::null_mut();
    }
    if thread_ops.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    let thread_ops = unsafe { *thread_ops };
    let (Some(create), Some(join), Some(current_thread_id)) = (
        thread_ops.create,
        thread_ops.join,
        thread_ops.current_thread_id,
    ) else {
        return ASIO_INVALID_PARAMETER;
    };
    let driver = Box::new(AsioDriver::new_with_thread_ops(Some(ThreadOps {
        create,
        join,
        current_thread_id,
    })));
    unsafe {
        *out = Box::into_raw(driver);
    }
    ASIO_SUCCESS
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle and `socket` must be a valid NUL-terminated path.
pub unsafe extern "C" fn sidealsa_asio_init(
    driver: *mut AsioDriver,
    socket: *const c_char,
) -> c_int {
    if socket.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    let path = match unsafe { CStr::from_ptr(socket) }.to_str() {
        Ok(path) => path,
        Err(_) => return ASIO_INVALID_PARAMETER,
    };
    with_driver(driver, |driver| driver.init(path))
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle and `output` must reference `capacity` writable bytes.
pub unsafe extern "C" fn sidealsa_asio_get_error_message(
    driver: *mut AsioDriver,
    output: *mut c_char,
    capacity: usize,
) -> c_int {
    if driver.is_null() || output.is_null() || capacity == 0 {
        return ASIO_INVALID_PARAMETER;
    }
    catch_unwind(AssertUnwindSafe(|| unsafe {
        let message = (&*driver)
            .last_error()
            .unwrap_or_else(|| "SideALSA operation failed; check sidealsad".into());
        let length = message.len().min(capacity - 1);
        ptr::copy_nonoverlapping(message.as_ptr().cast::<c_char>(), output, length);
        *output.add(length) = 0;
        ASIO_SUCCESS
    }))
    .unwrap_or(ASIO_HW_MALFUNCTION)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle.
pub unsafe extern "C" fn sidealsa_asio_get_channels(
    driver: *mut AsioDriver,
    inputs: *mut c_int,
    outputs: *mut c_int,
) -> c_int {
    if inputs.is_null() || outputs.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    if driver.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    catch_unwind(AssertUnwindSafe(|| unsafe {
        match (&*driver).get_channels() {
            Ok((input, output)) => {
                *inputs = input;
                *outputs = output;
                ASIO_SUCCESS
            }
            Err(error) => error.code(),
        }
    }))
    .unwrap_or(ASIO_HW_MALFUNCTION)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle and output pointers must be writable.
pub unsafe extern "C" fn sidealsa_asio_get_buffer_size(
    driver: *mut AsioDriver,
    min_size: *mut c_int,
    max_size: *mut c_int,
    preferred_size: *mut c_int,
    granularity: *mut c_int,
) -> c_int {
    if min_size.is_null() || max_size.is_null() || preferred_size.is_null() || granularity.is_null()
    {
        return ASIO_INVALID_PARAMETER;
    }
    if driver.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    catch_unwind(AssertUnwindSafe(|| unsafe {
        match (&*driver).get_buffer_size() {
            Ok((min, max, preferred, step)) => {
                *min_size = min;
                *max_size = max;
                *preferred_size = preferred;
                *granularity = step;
                ASIO_SUCCESS
            }
            Err(error) => error.code(),
        }
    }))
    .unwrap_or(ASIO_HW_MALFUNCTION)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle and `infos`/`callbacks` must be valid for the call.
pub unsafe extern "C" fn sidealsa_asio_create_buffers(
    driver: *mut AsioDriver,
    infos: *mut AsioBufferInfo,
    count: c_int,
    buffer_size: c_int,
    callbacks: *const AsioCallbacks,
) -> c_int {
    if count < 0 || callbacks.is_null() || (count > 0 && infos.is_null()) {
        return ASIO_INVALID_PARAMETER;
    }
    if driver.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    catch_unwind(AssertUnwindSafe(|| unsafe {
        let infos = if count == 0 {
            &mut []
        } else {
            slice::from_raw_parts_mut(infos, count as usize)
        };
        match (&*driver).create_buffers(infos, buffer_size, *callbacks) {
            Ok(()) => ASIO_SUCCESS,
            Err(error) => error.code(),
        }
    }))
    .unwrap_or(ASIO_HW_MALFUNCTION)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle.
pub unsafe extern "C" fn sidealsa_asio_start(driver: *mut AsioDriver) -> c_int {
    with_driver(driver, AsioDriver::start)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle.
pub unsafe extern "C" fn sidealsa_asio_stop(driver: *mut AsioDriver) -> c_int {
    with_driver(driver, AsioDriver::stop)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle.
pub unsafe extern "C" fn sidealsa_asio_dispose_buffers(driver: *mut AsioDriver) -> c_int {
    with_driver(driver, AsioDriver::dispose_buffers)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle. On success, ownership is consumed; on
/// failure, handle remains live.
pub unsafe extern "C" fn sidealsa_asio_close(driver: *mut AsioDriver) -> c_int {
    if driver.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let result = unsafe { (&*driver).close() };
        if result.is_ok() {
            unsafe {
                drop(Box::from_raw(driver));
            }
        }
        status(result)
    }))
    .unwrap_or(ASIO_HW_MALFUNCTION)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle and `info` must be writable.
pub unsafe extern "C" fn sidealsa_asio_get_channel_info(
    driver: *mut AsioDriver,
    info: *mut AsioChannelInfo,
) -> c_int {
    if info.is_null() || driver.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    let request = unsafe { *info };
    catch_unwind(AssertUnwindSafe(|| unsafe {
        match (&*driver).get_channel_info(request.channel, request.is_input) {
            Ok(result) => {
                *info = result;
                ASIO_SUCCESS
            }
            Err(error) => error.code(),
        }
    }))
    .unwrap_or(ASIO_HW_MALFUNCTION)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle and output pointers must be writable.
pub unsafe extern "C" fn sidealsa_asio_get_sample_rate(
    driver: *mut AsioDriver,
    rate: *mut f64,
) -> c_int {
    if rate.is_null() || driver.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    catch_unwind(AssertUnwindSafe(|| unsafe {
        match (&*driver).get_sample_rate() {
            Ok(value) => {
                *rate = value;
                ASIO_SUCCESS
            }
            Err(error) => error.code(),
        }
    }))
    .unwrap_or(ASIO_HW_MALFUNCTION)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle.
pub unsafe extern "C" fn sidealsa_asio_set_sample_rate(
    driver: *mut AsioDriver,
    rate: f64,
) -> c_int {
    with_driver(driver, |driver| driver.set_sample_rate(rate))
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle and output pointers must be writable.
pub unsafe extern "C" fn sidealsa_asio_get_latencies(
    driver: *mut AsioDriver,
    input: *mut c_int,
    output: *mut c_int,
) -> c_int {
    if input.is_null() || output.is_null() || driver.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    catch_unwind(AssertUnwindSafe(|| unsafe {
        match (&*driver).get_latencies() {
            Ok((input_latency, output_latency)) => {
                *input = input_latency;
                *output = output_latency;
                ASIO_SUCCESS
            }
            Err(error) => error.code(),
        }
    }))
    .unwrap_or(ASIO_HW_MALFUNCTION)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `driver` must be a live handle and output pointers must be writable.
pub unsafe extern "C" fn sidealsa_asio_get_sample_position(
    driver: *mut AsioDriver,
    samples: *mut AsioInt64,
    stamp: *mut AsioInt64,
) -> c_int {
    if samples.is_null() || stamp.is_null() || driver.is_null() {
        return ASIO_INVALID_PARAMETER;
    }
    catch_unwind(AssertUnwindSafe(|| unsafe {
        match (&*driver).sample_position() {
            Ok((sample_position, time_stamp)) => {
                *samples = split_i64(sample_position);
                *stamp = split_i64(time_stamp);
                ASIO_SUCCESS
            }
            Err(error) => error.code(),
        }
    }))
    .unwrap_or(ASIO_HW_MALFUNCTION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        os::unix::net::UnixStream,
        ptr::NonNull,
        sync::{Arc, Weak, mpsc},
    };

    thread_local! {
        static TEST_THREAD_ID: Cell<u32> = const { Cell::new(1) };
    }

    static FAILURE_NOTIFICATION: std::sync::atomic::AtomicI32 =
        std::sync::atomic::AtomicI32::new(0);
    static FAILURE_NOTIFICATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[derive(Default)]
    struct FakeRecoveryStream {
        restarts: usize,
        gate_transition: Option<(Arc<WorkerGate>, bool)>,
    }

    impl WorkerStreamRecovery for FakeRecoveryStream {
        fn restart_after_discontinuity(&mut self) -> Result<(), AsioError> {
            self.restarts += 1;
            if let Some((gate, running)) = &self.gate_transition {
                gate.set_running(*running);
            }
            Ok(())
        }
    }

    unsafe extern "C" fn unused_create(
        _context: *mut c_void,
        _handle: *mut *mut c_void,
        _thread_id: *mut u32,
    ) -> c_int {
        ASIO_HW_MALFUNCTION
    }

    unsafe extern "C" fn unused_join(_handle: *mut c_void, _timeout_ms: u32) -> c_int {
        ASIO_HW_MALFUNCTION
    }

    unsafe extern "C" fn test_current_thread_id() -> u32 {
        TEST_THREAD_ID.get()
    }

    unsafe extern "win64" fn test_buffer_switch(_index: c_int, _direct: c_int) {}

    unsafe extern "win64" fn test_time_info_callback(
        _time_info: *mut AsioTimeInfo,
        _index: c_int,
        _direct: c_int,
    ) -> *mut AsioTimeInfo {
        ptr::null_mut()
    }

    unsafe extern "win64" fn test_asio_message(
        selector: c_int,
        _value: c_int,
        _message: *mut c_void,
        _opt: *mut f64,
    ) -> c_int {
        FAILURE_NOTIFICATION.store(selector, Ordering::Release);
        1
    }

    fn test_device() -> DeviceInfo {
        DeviceInfo {
            name: "Test".into(),
            profile_fingerprint: 0,
            rate: 48000,
            period_size: 64,
            hardware_period_size: 32,
            buffer_size: 192,
            shared_buffer_size: 256,
            pro_latency_periods: 1,
            pro_output_latency_frames: 96,
            pro_realtime_priority: 86,
            shared_latency_periods: 3,
            playback_channels: 8,
            capture_channels: 10,
            playback_ports: Vec::new(),
            capture_ports: Vec::new(),
        }
    }

    fn fake_running_driver(
        timeout: Duration,
    ) -> (Arc<AsioDriver>, Arc<WorkerGate>, Weak<HostBuffers>) {
        let driver = Arc::new(AsioDriver::new_with_thread_ops_and_timeout(
            Some(ThreadOps {
                create: unused_create,
                join: unused_join,
                current_thread_id: test_current_thread_id,
            }),
            timeout,
        ));
        let gate = Arc::new(WorkerGate::new(true).expect("gate should open"));
        gate.acknowledge(0, true);
        let buffers = Arc::new(HostBuffers::new(1, 1, 64).expect("buffers should allocate"));
        let weak_buffers = Arc::downgrade(&buffers);
        let mut inner = driver.inner.lock().expect("lock should work");
        inner.state = DriverState::Running;
        inner.buffers = Some(buffers);
        inner.worker_thread = Some(7);
        inner.worker = Some(WorkerHandle {
            stop: Arc::new(StopSignal::new().expect("stop signal should open")),
            gate: Arc::clone(&gate),
            thread_id: 7,
            handle: NonNull::<u8>::dangling().as_ptr().cast(),
            context: NonNull::<WorkerContext>::dangling().as_ptr(),
            thread_ops: driver.thread_ops.expect("thread operations should exist"),
        });
        drop(inner);
        (driver, gate, weak_buffers)
    }

    fn remove_fake_worker(driver: &AsioDriver) {
        let mut inner = driver.inner.lock().expect("lock should work");
        inner.worker = None;
        inner.worker_thread = None;
        inner.quiesce_generation = None;
        inner.state = DriverState::Prepared;
    }

    #[test]
    fn configured_pro_output_latency_is_reported() {
        let driver = AsioDriver::new();
        let mut inner = driver.inner.lock().expect("lock should work");
        inner.state = DriverState::Initialized;
        inner.device = Some(test_device());
        drop(inner);

        assert_eq!(
            driver.get_buffer_size().expect("size should work"),
            (64, 64, 64, 0)
        );
        assert_eq!(
            driver.get_latencies().expect("latencies should work"),
            (64, 96)
        );
    }

    #[test]
    fn initialization_error_is_available_to_host() {
        let driver = AsioDriver::new();

        driver
            .init("/tmp/sidealsa-asio-test-missing.sock")
            .expect_err("missing daemon should fail");

        assert!(driver.last_error().is_some());
    }

    #[test]
    fn s32_float_conversion_clips_and_preserves_sign() {
        assert_eq!(s32_to_f32(i32::MIN), -1.0);
        assert_eq!(f32_to_s32(-1.0), i32::MIN);
        assert_eq!(f32_to_s32(1.0), i32::MAX);
        assert_eq!(f32_to_s32(f32::NAN), 0);
        assert_eq!(f32_to_s32(0.5), 1_073_741_824);
        assert_eq!(f32_to_s32(f32::from_bits(0x2f80_0000)), 1);
        assert_eq!(f32_to_s32(f32::from_bits(0xaf80_0000)), -1);
    }

    fn f32_to_s32_reference(sample: f32) -> i32 {
        if sample.is_nan() {
            return 0;
        }
        if sample <= -1.0 {
            return i32::MIN;
        }
        if sample >= 1.0 {
            return i32::MAX;
        }
        (f64::from(sample) * 2_147_483_648.0)
            .round()
            .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
    }

    fn conversion_test_samples(count: usize) -> Vec<f32> {
        let mut samples = vec![
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -1.0,
            1.0,
            -0.0,
            0.0,
            -0.5,
            0.5,
            f32::from_bits((-1.0_f32).to_bits() - 1),
            f32::from_bits(1.0_f32.to_bits() - 1),
            f32::from_bits(1),
            f32::from_bits(0x8000_0001),
            f32::from_bits(0x2f80_0000),
            f32::from_bits(0xaf80_0000),
        ];
        let mut state = 0x9e37_79b9_u32;
        while samples.len() < count {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            samples.push(f32::from_bits(state));
        }
        samples
    }

    fn populated_output_buffers(period: usize, channels: usize) -> HostBuffers {
        let buffers = HostBuffers::new(0, channels, period).expect("buffers should allocate");
        let samples = conversion_test_samples(period * channels * 2);
        for index in 0..2 {
            for channel in 0..channels {
                for frame in 0..period {
                    let source = (index * period + frame) * channels + channel;
                    buffers.output[channel].half_mut(index)[frame] = samples[source];
                }
            }
        }
        buffers
    }

    fn reference_copy_host_to_playback(
        buffers: &HostBuffers,
        active: &ActiveChannels,
        index: usize,
        playback: &mut [i32],
        period_frames: usize,
    ) {
        playback.fill(0);
        let channels = active.output.len();
        for (channel, is_active) in active.output.iter().copied().enumerate() {
            if !is_active {
                continue;
            }
            let source = buffers.output[channel].half(index);
            for frame in 0..period_frames {
                playback[frame * channels + channel] = f32_to_s32_reference(source[frame]);
            }
        }
    }

    #[test]
    fn optimized_scalar_conversion_matches_reference() {
        for sample in conversion_test_samples(65_536) {
            assert_eq!(f32_to_s32(sample), f32_to_s32_reference(sample));
        }
    }

    #[test]
    fn sparse_output_uses_scalar_kernel() {
        let active = ActiveChannels {
            input: Box::new([]),
            output: vec![true, false, true, false, true, false, true, false].into_boxed_slice(),
        };
        let kernels = SampleKernels::detect(&active);
        assert!(std::ptr::fn_addr_eq(
            kernels.copy_host_to_playback,
            copy_host_to_playback_scalar as CopyHostToPlayback,
        ));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_playback_conversion_matches_scalar_for_full_channels_and_tails() {
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        let channels = 8;
        for period in [64, 65] {
            let buffers = populated_output_buffers(period, channels);
            let active = ActiveChannels {
                input: Box::new([]),
                output: vec![true; channels].into_boxed_slice(),
            };
            for index in 0..2 {
                let mut scalar = vec![0_i32; period * channels];
                let mut simd = vec![0_i32; period * channels];
                copy_host_to_playback_scalar(&buffers, &active, index, &mut scalar, period);
                unsafe { copy_host_to_playback_avx2(&buffers, &active, index, &mut simd, period) };
                assert_eq!(simd, scalar);
            }
        }

        let source = conversion_test_samples(65_536);
        let (chunks, remainder) = source.as_chunks::<8>();
        assert!(remainder.is_empty());
        for chunk in chunks {
            let mut simd = [0_i32; 8];
            unsafe {
                use std::arch::x86_64::*;
                let converted = convert_f32x8_to_s32(chunk.as_ptr());
                _mm256_storeu_si256(simd.as_mut_ptr().cast(), converted);
                _mm256_zeroupper();
            }
            for (sample, converted) in chunk.iter().copied().zip(simd) {
                assert_eq!(converted, f32_to_s32_reference(sample));
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    #[ignore = "release-mode conversion microbenchmark"]
    fn benchmark_asio_playback_conversion() {
        use std::{hint::black_box, time::Instant};

        if !std::arch::is_x86_feature_detected!("avx2") {
            eprintln!("AVX2 unavailable");
            return;
        }
        let period = 64;
        let channels = 8;
        let iterations = 100_000_u128;
        let buffers = populated_output_buffers(period, channels);
        let active = ActiveChannels {
            input: Box::new([]),
            output: vec![true; channels].into_boxed_slice(),
        };

        let measure = |kernel: CopyHostToPlayback| {
            let mut playback = vec![0_i32; period * channels];
            let start = Instant::now();
            for _ in 0..iterations {
                unsafe {
                    black_box(kernel)(
                        black_box(&buffers),
                        black_box(&active),
                        0,
                        black_box(&mut playback),
                        period,
                    )
                };
            }
            black_box(playback);
            start.elapsed().as_nanos() / iterations
        };

        let reference = measure(reference_copy_host_to_playback);
        let scalar = measure(copy_host_to_playback_scalar);
        let avx2 = measure(copy_host_to_playback_avx2);
        eprintln!(
            "ASIO Q64 8ch playback: reference={reference} ns, scalar={scalar} ns, avx2={avx2} ns, avx2/reference={:.2}x, avx2/scalar={:.2}x",
            reference as f64 / avx2 as f64,
            scalar as f64 / avx2 as f64,
        );
    }

    #[test]
    fn host_buffers_have_two_period_pointers() {
        for period in [7, 8, 9, 64, 65] {
            let buffers = HostBuffers::new(1, 1, period).expect("buffers should allocate");
            let pointers = buffers.input[0].pointers();
            assert!(!pointers[0].is_null());
            assert!(!pointers[1].is_null());
            assert_ne!(pointers[0], pointers[1]);
            assert_eq!(pointers[0] as usize % 32, 0);
            assert_eq!(pointers[1] as usize % 32, 0);
            assert!(pointers[1] as usize - pointers[0] as usize >= period * size_of::<f32>());
        }
    }

    #[test]
    fn sequence_gap_advances_position_and_buffer_parity_before_callback() {
        let mut previous = None;
        let mut index = 0;
        let mut position = 0;
        let mut observed = Vec::new();
        for sequence in [10, 11, 13, 14] {
            let CycleAdvance::Ready {
                buffer_index,
                sample_position,
            } = advance_asio_cycle(previous, sequence, 64, index, position)
            else {
                panic!("forward sequence should advance");
            };
            index = buffer_index;
            position = sample_position;
            observed.push((index, position));
            previous = Some(sequence);
        }
        assert_eq!(observed, [(0, 0), (1, 64), (1, 192), (0, 256)]);
    }

    #[test]
    fn duplicate_and_backward_sequences_do_not_fabricate_progress() {
        assert_eq!(
            advance_asio_cycle(Some(20), 20, 64, 1, 512),
            CycleAdvance::Duplicate
        );
        assert_eq!(
            advance_asio_cycle(Some(20), 19, 64, 1, 512),
            CycleAdvance::Backward
        );
        assert_eq!(
            advance_asio_cycle(Some(u64::MAX), 0, 64, 0, 512),
            CycleAdvance::Ready {
                buffer_index: 1,
                sample_position: 576,
            }
        );
    }

    #[test]
    fn worker_gate_quiesces_and_restarts_by_generation() {
        let gate = WorkerGate::new(true).expect("gate should open");
        assert_eq!(gate.command(), (0, true));
        gate.acknowledge(0, true);
        wait_for_acknowledgement(&gate, 0, true, Duration::from_millis(10))
            .expect("start should be acknowledged");

        let stopped = gate.set_running(false);
        assert_eq!(gate.command(), (stopped, false));
        gate.acknowledge(stopped, false);
        wait_for_acknowledgement(&gate, stopped, false, Duration::from_millis(10))
            .expect("stop should be acknowledged");

        let restarted = gate.set_running(true);
        assert_eq!(gate.command(), (restarted, true));
        assert_ne!(stopped, restarted);
        gate.acknowledge(restarted, true);
        wait_for_acknowledgement(&gate, restarted, true, Duration::from_millis(10))
            .expect("restart should be acknowledged");
    }

    #[test]
    fn worker_gate_keeps_start_acknowledgement_after_stop_acknowledgement() {
        let gate = WorkerGate::new(true).expect("gate should open");
        gate.acknowledge(0, true);
        let stopped = gate.set_running(false);
        gate.acknowledge(stopped, false);

        wait_for_acknowledgement(&gate, 0, true, Duration::from_millis(10))
            .expect("later stop acknowledgement must not hide start acknowledgement");
    }

    #[test]
    fn worker_failure_after_acknowledgement_is_not_reported_as_success() {
        let gate = WorkerGate::new(true).expect("gate should open");
        gate.acknowledge(0, true);
        gate.publish_failure(WorkerFailure::CaptureDiscontinuity);

        assert!(matches!(
            wait_for_acknowledgement(&gate, 0, true, Duration::from_millis(10)),
            Err(AsioError::WorkerStopped(
                "audio capture must be resynchronized"
            ))
        ));
    }

    #[test]
    fn capture_discontinuities_are_recoverable_worker_events() {
        assert!(is_recoverable_discontinuity(&AsioError::Client(
            ClientError::CaptureDiscontinuity
        )));
        assert!(is_recoverable_discontinuity(
            &AsioError::CaptureSequenceBackward
        ));
        assert!(!is_recoverable_discontinuity(
            &AsioError::DaemonDisconnected
        ));
    }

    #[test]
    fn worker_recovery_restarts_stream_resets_cycle_and_requests_resync() {
        let _notification_guard = FAILURE_NOTIFICATION_LOCK
            .lock()
            .expect("notification lock should work");
        let gate = WorkerGate::new(true).expect("gate should open");
        let mut stream = FakeRecoveryStream::default();
        let callbacks = AsioCallbacks {
            asio_message: Some(test_asio_message),
            ..AsioCallbacks::default()
        };
        let mut run_generation = u64::MAX;
        let mut buffer_index = 1;
        let mut previous_sequence = Some(10);
        let mut last_active_sequence = Some(10);
        let mut stopping_after = Some(10);
        FAILURE_NOTIFICATION.store(0, Ordering::Release);

        recover_worker_discontinuity(
            &mut stream,
            &gate,
            callbacks,
            &mut run_generation,
            &mut buffer_index,
            &mut previous_sequence,
            &mut last_active_sequence,
            &mut stopping_after,
        )
        .expect("capture discontinuity should recover");

        assert_eq!(stream.restarts, 1);
        assert_eq!(run_generation, 0);
        assert_eq!(buffer_index, 0);
        assert_eq!(previous_sequence, None);
        assert_eq!(last_active_sequence, None);
        assert_eq!(stopping_after, None);
        assert_eq!(
            FAILURE_NOTIFICATION.load(Ordering::Acquire),
            ASIO_MESSAGE_RESYNC_REQUEST
        );
        wait_for_acknowledgement(&gate, 0, true, Duration::from_millis(10))
            .expect("recovered running generation should be acknowledged");
        assert_eq!(gate.failure(), None);
        assert!(gate.is_alive());
    }

    #[test]
    fn worker_recovery_acknowledges_start_racing_a_stopped_stream() {
        let _notification_guard = FAILURE_NOTIFICATION_LOCK
            .lock()
            .expect("notification lock should work");
        let gate = Arc::new(WorkerGate::new(false).expect("gate should open"));
        let mut stream = FakeRecoveryStream {
            restarts: 0,
            gate_transition: Some((Arc::clone(&gate), true)),
        };
        let callbacks = AsioCallbacks {
            asio_message: Some(test_asio_message),
            ..AsioCallbacks::default()
        };
        let mut run_generation = 0;
        let mut buffer_index = 1;
        let mut previous_sequence = Some(10);
        let mut last_active_sequence = Some(10);
        let mut stopping_after = Some(10);
        FAILURE_NOTIFICATION.store(0, Ordering::Release);

        recover_worker_discontinuity(
            &mut stream,
            &gate,
            callbacks,
            &mut run_generation,
            &mut buffer_index,
            &mut previous_sequence,
            &mut last_active_sequence,
            &mut stopping_after,
        )
        .expect("capture discontinuity should recover");

        assert_eq!(gate.command(), (1, true));
        wait_for_acknowledgement(&gate, 1, true, Duration::from_millis(10))
            .expect("concurrent start should be acknowledged");
        assert_eq!(run_generation, 1);
        assert_eq!(
            FAILURE_NOTIFICATION.load(Ordering::Acquire),
            ASIO_MESSAGE_RESYNC_REQUEST
        );
    }

    #[test]
    fn worker_recovery_acknowledges_stop_racing_a_running_stream() {
        let gate = Arc::new(WorkerGate::new(true).expect("gate should open"));
        let mut stream = FakeRecoveryStream {
            restarts: 0,
            gate_transition: Some((Arc::clone(&gate), false)),
        };
        let mut run_generation = 0;
        let mut buffer_index = 1;
        let mut previous_sequence = Some(10);
        let mut last_active_sequence = Some(10);
        let mut stopping_after = Some(10);

        recover_worker_discontinuity(
            &mut stream,
            &gate,
            AsioCallbacks::default(),
            &mut run_generation,
            &mut buffer_index,
            &mut previous_sequence,
            &mut last_active_sequence,
            &mut stopping_after,
        )
        .expect("capture discontinuity should recover");

        assert_eq!(gate.command(), (1, false));
        wait_for_acknowledgement(&gate, 1, false, Duration::from_millis(10))
            .expect("concurrent stop should be acknowledged");
        assert_eq!(run_generation, 1);
    }

    #[test]
    fn gate_transition_discards_block_acquired_before_start() {
        assert!(acquired_block_is_current(4, 4));
        assert!(!acquired_block_is_current(4, 5));
    }

    #[test]
    fn callback_stop_bypasses_lifecycle_lock_and_external_stop_finalizes() {
        let (driver, gate, _) = fake_running_driver(Duration::from_millis(50));
        let lifecycle = driver.lifecycle.lock().expect("lifecycle should lock");
        let callback_driver = Arc::clone(&driver);
        let (result_tx, result_rx) = mpsc::channel();
        let callback = thread::spawn(move || {
            TEST_THREAD_ID.set(7);
            result_tx
                .send(callback_driver.stop())
                .expect("callback result should send");
        });

        let result = result_rx.recv_timeout(Duration::from_millis(100));
        drop(lifecycle);
        callback.join().expect("callback thread should finish");
        assert!(result.expect("callback Stop must not wait").is_ok());
        let (generation, running) = gate.command();
        assert!(!running);

        gate.acknowledge(generation, false);
        TEST_THREAD_ID.set(1);
        driver.stop().expect("external Stop should finalize");
        assert_eq!(
            driver.inner.lock().expect("lock should work").state,
            DriverState::Prepared
        );
        remove_fake_worker(&driver);
    }

    #[test]
    fn blocked_callback_stop_timeout_keeps_worker_and_buffers_alive() {
        let (driver, _gate, buffers) = fake_running_driver(Duration::from_millis(30));
        TEST_THREAD_ID.set(1);
        let started = Instant::now();
        let result = driver.stop();
        let elapsed = started.elapsed();

        assert!(matches!(result, Err(AsioError::WorkerTimeout)));
        assert!(elapsed >= Duration::from_millis(20));
        assert!(elapsed < Duration::from_millis(250));
        let inner = driver.inner.lock().expect("lock should work");
        assert_eq!(inner.state, DriverState::Stopping);
        assert!(inner.worker.is_some());
        assert!(
            inner
                .worker
                .as_ref()
                .is_some_and(|handle| !handle.stop.is_requested())
        );
        assert!(
            inner
                .worker
                .as_ref()
                .is_some_and(|handle| !handle.context.is_null())
        );
        assert!(buffers.upgrade().is_some());
        drop(inner);
        remove_fake_worker(&driver);
    }

    #[test]
    fn channel_activity_comes_from_created_channel_map() {
        let mut driver = AsioDriver::new();
        let mut inner = driver.inner.lock().expect("lock should work");
        inner.state = DriverState::Initialized;
        inner.device = Some(test_device());
        drop(inner);

        let mut info = AsioChannelInfo {
            channel: 1,
            is_input: 1,
            is_active: 123,
            channel_group: 0,
            sample_type: 0,
            name: [0; 32],
        };
        assert_eq!(
            unsafe { sidealsa_asio_get_channel_info(&mut driver, &mut info) },
            ASIO_SUCCESS
        );
        assert_eq!(info.is_active, 0);

        let mut inner = driver.inner.lock().expect("lock should work");
        inner.active = Some(Arc::new(ActiveChannels {
            input: [
                false, true, false, false, false, false, false, false, false, false,
            ]
            .into(),
            output: [false; 8].into(),
        }));
        inner.state = DriverState::Prepared;
        drop(inner);
        info.is_active = 0;
        assert_eq!(
            unsafe { sidealsa_asio_get_channel_info(&mut driver, &mut info) },
            ASIO_SUCCESS
        );
        assert_eq!(info.is_active, 1);
    }

    #[test]
    fn buffer_switch_callback_is_required() {
        let driver = AsioDriver::new();
        let mut inner = driver.inner.lock().expect("lock should work");
        inner.state = DriverState::Initialized;
        inner.device = Some(test_device());
        drop(inner);
        let callbacks = AsioCallbacks {
            buffer_switch_time_info: Some(test_time_info_callback),
            ..AsioCallbacks::default()
        };

        assert!(matches!(
            driver.create_buffers(&mut [], 64, callbacks),
            Err(AsioError::InvalidParameter)
        ));
        assert_eq!(ASIO_NO_MEMORY, -994);
    }

    #[test]
    fn invalid_thread_callback_table_clears_output_handle() {
        let operations = AsioThreadOps {
            create: Some(unused_create),
            join: None,
            current_thread_id: Some(test_current_thread_id),
        };
        let mut driver = NonNull::<AsioDriver>::dangling().as_ptr();

        assert_eq!(
            unsafe { sidealsa_asio_new(&operations, &mut driver) },
            ASIO_INVALID_PARAMETER
        );
        assert!(driver.is_null());
    }

    #[test]
    fn position_snapshot_never_mixes_samples_and_timestamp() {
        let position = Arc::new(PositionSnapshot::new());
        let writer_position = Arc::clone(&position);
        let writer = thread::spawn(move || {
            for samples in 1..100_000_u64 {
                writer_position.publish(samples, !samples);
            }
        });
        for _ in 0..100_000 {
            let (samples, stamp) = position.load();
            if samples != 0 {
                assert_eq!(stamp, !samples);
            }
        }
        writer.join().expect("writer should finish");
        let (samples, stamp) = position.load();
        assert_eq!(stamp, !samples);
    }

    #[test]
    fn daemon_control_disconnect_is_detected_and_health_reaches_getters() {
        let (control, peer) = UnixStream::pair().expect("socket pair should open");
        drop(peer);
        let mut pollfd = libc::pollfd {
            fd: control.as_raw_fd(),
            events: libc::POLLIN | libc::POLLRDHUP,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut pollfd, 1, 100) }, 1);
        assert!(
            daemon_control_failed(control.as_raw_fd(), pollfd.revents)
                .expect("disconnect check should work")
        );

        let (driver, gate, _) = fake_running_driver(Duration::from_millis(20));
        {
            let mut inner = driver.inner.lock().expect("lock should work");
            inner.device = Some(test_device());
        }
        gate.publish_failure(WorkerFailure::DaemonDisconnected);
        assert!(matches!(
            driver.get_channels(),
            Err(AsioError::DaemonDisconnected)
        ));
        assert!(
            driver
                .last_error()
                .is_some_and(|message| message.contains("daemon disconnected"))
        );
        remove_fake_worker(&driver);
    }

    #[test]
    fn worker_failure_requests_host_reset_or_resync() {
        let _notification_guard = FAILURE_NOTIFICATION_LOCK
            .lock()
            .expect("notification lock should work");
        let callbacks = AsioCallbacks {
            buffer_switch: Some(test_buffer_switch),
            asio_message: Some(test_asio_message),
            ..AsioCallbacks::default()
        };
        FAILURE_NOTIFICATION.store(0, Ordering::Release);
        notify_worker_failure(callbacks, WorkerFailure::DaemonDisconnected);
        assert_eq!(
            FAILURE_NOTIFICATION.load(Ordering::Acquire),
            ASIO_MESSAGE_RESET_REQUEST
        );
        notify_worker_failure(callbacks, WorkerFailure::CaptureDiscontinuity);
        assert_eq!(
            FAILURE_NOTIFICATION.load(Ordering::Acquire),
            ASIO_MESSAGE_RESYNC_REQUEST
        );
    }
}
