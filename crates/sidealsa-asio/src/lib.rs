// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    ffi::{CStr, c_char, c_int, c_void},
    os::fd::RawFd,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    ptr, slice,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use sidealsa_client::{AudioStream, ClientError, SideAlsaClient};
use sidealsa_protocol::{DeviceInfo, SharedRegionInfo};
use thiserror::Error;

pub const ASIO_SUCCESS: c_int = 0;
pub const ASIO_INVALID_PARAMETER: c_int = -998;
pub const ASIO_NO_IO: c_int = -1000;
pub const ASIO_BUFFER_SIZE_NOT_SUPPORTED: c_int = -997;
pub const ASIO_HW_MALFUNCTION: c_int = -999;
pub const ASIO_NO_MEMORY: c_int = -9991;
pub const ASIO_SAMPLE_RATE_NOT_SUPPORTED: c_int = -995;

pub const ASIOST_FLOAT32_LSB: c_int = 19;
pub const ASIO_MESSAGE_SUPPORTS_TIME_INFO: c_int = 7;

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
pub type ThreadJoin = unsafe extern "C" fn(handle: *mut c_void) -> c_int;
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
    #[error("realtime scheduling failed: {0}")]
    RealtimeScheduling(#[source] std::io::Error),
    #[error("worker callback panicked")]
    CallbackPanic,
    #[error("driver method called from audio callback")]
    Reentrant,
    #[error("audio worker stopped: {0}")]
    WorkerStopped(String),
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
            | Self::RealtimeScheduling(_)
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
    sample_position: Arc<AtomicU64>,
    time_stamp: Arc<AtomicU64>,
    last_error: Option<String>,
}

pub struct AsioDriver {
    inner: Mutex<DriverInner>,
    worker_done: Condvar,
    thread_ops: Option<ThreadOps>,
}

impl AsioDriver {
    pub fn new() -> Self {
        Self::new_with_thread_ops(None)
    }

    fn new_with_thread_ops(thread_ops: Option<ThreadOps>) -> Self {
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
                sample_position: Arc::new(AtomicU64::new(0)),
                time_stamp: Arc::new(AtomicU64::new(0)),
                last_error: None,
            }),
            worker_done: Condvar::new(),
            thread_ops,
        }
    }

    pub fn init(&self, socket: impl AsRef<Path>) -> Result<(), AsioError> {
        let mut client = SideAlsaClient::connect(socket)?;
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
        inner.state = DriverState::Initialized;
        Ok(())
    }

    pub fn device(&self) -> Result<DeviceInfo, AsioError> {
        self.lock()?.device.clone().ok_or(AsioError::InvalidState)
    }

    pub fn get_channels(&self) -> Result<(c_int, c_int), AsioError> {
        let inner = self.lock()?;
        ensure_initialized(inner.state)?;
        let device = inner.device.as_ref().ok_or(AsioError::InvalidState)?;
        Ok((
            checked_c_int(device.capture_channels)?,
            checked_c_int(device.playback_channels)?,
        ))
    }

    pub fn get_buffer_size(&self) -> Result<(c_int, c_int, c_int, c_int), AsioError> {
        let inner = self.lock()?;
        ensure_initialized(inner.state)?;
        let device = inner.device.as_ref().ok_or(AsioError::InvalidState)?;
        let period = checked_c_int(device.period_size)?;
        Ok((period, period, period, 0))
    }

    pub fn get_sample_rate(&self) -> Result<f64, AsioError> {
        let inner = self.lock()?;
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
        let inner = self.lock()?;
        ensure_initialized(inner.state)?;
        let device = inner.device.as_ref().ok_or(AsioError::InvalidState)?;
        let period = checked_c_int(device.period_size)?;
        let output_periods = checked_c_int(device.pro_latency_periods.max(1))?;
        let output = period
            .checked_mul(output_periods)
            .ok_or(AsioError::InvalidParameter)?;
        Ok((period, output))
    }

    pub fn get_channel_info(
        &self,
        channel: c_int,
        is_input: c_int,
    ) -> Result<AsioChannelInfo, AsioError> {
        let inner = self.lock()?;
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
        let mut name = [0; 32];
        let prefix = if is_input != 0 { "Input " } else { "Output " };
        let text = format!("{prefix}{}", channel + 1);
        for (destination, source) in name.iter_mut().zip(text.bytes()) {
            *destination = source as c_char;
        }
        Ok(AsioChannelInfo {
            channel,
            is_input: if is_input != 0 { 1 } else { 0 },
            is_active: 0,
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
        let inner = self.lock()?;
        if inner.state != DriverState::Initialized {
            return Err(AsioError::InvalidState);
        }
        if callbacks.buffer_switch.is_none() && callbacks.buffer_switch_time_info.is_none() {
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
        let mut input_active = vec![false; input_count].into_boxed_slice();
        let mut output_active = vec![false; output_count].into_boxed_slice();

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
        self.reap_worker()?;
        let mut inner = self.lock()?;
        if inner.state != DriverState::Prepared {
            return Err(AsioError::InvalidState);
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
        let sample_position = Arc::clone(&inner.sample_position);
        let time_stamp = Arc::clone(&inner.time_stamp);
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
        let stream = inner.stream.take().ok_or(AsioError::NoIo)?;
        let context = Box::new(WorkerContext {
            ready: AtomicBool::new(false),
            stream: Mutex::new(Some(stream)),
            input: WorkerInput {
                stop: Arc::clone(&stop),
                buffers,
                active,
                callbacks,
                time_info,
                rate,
                realtime_priority,
                period_frames,
                capture_samples,
                playback_samples,
                sample_position,
                time_stamp,
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
            thread_id,
            handle: thread_handle,
            context,
            thread_ops,
        });
        inner.worker_thread = Some(thread_id);
        inner.sample_position.store(0, Ordering::Release);
        inner.time_stamp.store(0, Ordering::Release);
        inner.state = DriverState::Running;
        unsafe { &*context }.ready.store(true, Ordering::Release);
        Ok(())
    }

    pub fn stop(&self) -> Result<(), AsioError> {
        let current = self.current_thread_id();
        let handle = {
            let mut inner = self.lock()?;
            loop {
                if inner.state == DriverState::Prepared {
                    return Ok(());
                }
                if !matches!(inner.state, DriverState::Running | DriverState::Stopping) {
                    return Err(AsioError::InvalidState);
                }
                let Some(handle) = inner.worker.as_mut() else {
                    if inner.worker_thread == Some(current) {
                        inner.state = DriverState::Stopping;
                        return Ok(());
                    }
                    inner = self
                        .worker_done
                        .wait(inner)
                        .map_err(|_| AsioError::Worker)?;
                    continue;
                };
                handle.stop.request();
                if handle.thread_id == current {
                    inner.state = DriverState::Stopping;
                    return Ok(());
                }
                inner.state = DriverState::Stopping;
                break inner.worker.take().ok_or(AsioError::NoIo)?;
            }
        };
        self.finish_worker(handle)
    }

    pub fn dispose_buffers(&self) -> Result<(), AsioError> {
        self.stop_if_running()?;
        let mut inner = self.lock()?;
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
        self.stop_if_running()?;
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
            stream.close()?;
        }
        Ok(())
    }

    pub fn sample_position(&self) -> Result<(u64, u64), AsioError> {
        let inner = self.lock()?;
        ensure_initialized(inner.state)?;
        Ok((
            inner.sample_position.load(Ordering::Acquire),
            inner.time_stamp.load(Ordering::Acquire),
        ))
    }

    pub fn last_error(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.last_error.clone())
    }

    fn stop_if_running(&self) -> Result<(), AsioError> {
        let state = self.lock()?.state;
        if matches!(state, DriverState::Running | DriverState::Stopping) {
            self.stop()
        } else {
            Ok(())
        }
    }

    fn reap_worker(&self) -> Result<(), AsioError> {
        let current = self.current_thread_id();
        let handle = {
            let mut inner = self.lock()?;
            if inner.worker_thread == Some(current) {
                return Err(AsioError::Reentrant);
            }
            let Some(handle) = inner.worker.as_ref() else {
                return Ok(());
            };
            if handle.thread_id == current {
                return Err(AsioError::Reentrant);
            }
            inner.worker.take().ok_or(AsioError::Worker)?
        };
        self.finish_worker(handle)
    }

    fn finish_worker(&self, handle: WorkerHandle) -> Result<(), AsioError> {
        handle.stop.request();
        if unsafe { (handle.thread_ops.join)(handle.handle) } != 0 {
            let mut inner = self.lock()?;
            inner.worker = Some(handle);
            self.worker_done.notify_all();
            return Err(AsioError::Worker);
        }
        let context = unsafe { Box::from_raw(handle.context) };
        let (stream, error) = context.into_exit();
        let mut inner = self.lock()?;
        inner.stream = stream;
        inner.worker_thread = None;
        inner.state = DriverState::Prepared;
        self.worker_done.notify_all();
        if let Some(error) = error {
            inner.last_error = Some(error.to_string());
            return Err(error);
        }
        Ok(())
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
                && unsafe { (handle.thread_ops.join)(handle.handle) } == 0
            {
                let context = unsafe { Box::from_raw(handle.context) };
                if let Some(mut stream) = context.take_stream() {
                    let _ = stream.close();
                }
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
    thread_id: u32,
    handle: *mut c_void,
    context: *mut WorkerContext,
    thread_ops: ThreadOps,
}

unsafe impl Send for WorkerHandle {}

struct WorkerInput {
    stop: Arc<StopSignal>,
    buffers: Arc<HostBuffers>,
    active: Arc<ActiveChannels>,
    callbacks: AsioCallbacks,
    time_info: bool,
    rate: u32,
    realtime_priority: c_int,
    period_frames: usize,
    capture_samples: usize,
    playback_samples: usize,
    sample_position: Arc<AtomicU64>,
    time_stamp: Arc<AtomicU64>,
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

struct ChannelBuffer {
    data: std::cell::UnsafeCell<Box<[f32]>>,
    period: usize,
}

// Host owns one half while worker owns other half; callback ordering provides synchronization.
unsafe impl Send for ChannelBuffer {}
unsafe impl Sync for ChannelBuffer {}

impl ChannelBuffer {
    fn new(period: usize) -> Result<Self, AsioError> {
        let samples = period.checked_mul(2).ok_or(AsioError::NoMemory)?;
        Ok(Self {
            data: std::cell::UnsafeCell::new(vec![0.0; samples].into_boxed_slice()),
            period,
        })
    }

    fn pointers(&self) -> [*mut c_void; 2] {
        unsafe {
            let data = &mut *self.data.get();
            [
                data.as_mut_ptr().cast(),
                data.as_mut_ptr().add(self.period).cast(),
            ]
        }
    }

    fn half(&self, index: usize) -> &[f32] {
        unsafe {
            let data = &*self.data.get();
            &data[index * self.period..(index + 1) * self.period]
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn half_mut(&self, index: usize) -> &mut [f32] {
        unsafe {
            let data = &mut *self.data.get();
            &mut data[index * self.period..(index + 1) * self.period]
        }
    }
}

struct HostBuffers {
    input: Box<[ChannelBuffer]>,
    output: Box<[ChannelBuffer]>,
}

impl HostBuffers {
    fn new(input_count: usize, output_count: usize, period: usize) -> Result<Self, AsioError> {
        let input = (0..input_count)
            .map(|_| ChannelBuffer::new(period))
            .collect::<Result<Vec<_>, _>>()?
            .into_boxed_slice();
        let output = (0..output_count)
            .map(|_| ChannelBuffer::new(period))
            .collect::<Result<Vec<_>, _>>()?
            .into_boxed_slice();
        Ok(Self { input, output })
    }
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
    let mut capture = vec![0_i32; input.capture_samples].into_boxed_slice();
    let mut playback = vec![0_i32; input.playback_samples].into_boxed_slice();
    let audio_fd = stream.notification_fd()?;
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
        Ok(()) => worker_loop(
            stream,
            audio_fd,
            &input.stop,
            &input.buffers,
            &input.active,
            input.callbacks,
            input.time_info,
            input.rate,
            input.period_frames,
            &mut capture,
            &mut playback,
            &input.sample_position,
            &input.time_stamp,
        ),
        Err(error) => Err(error.into()),
    };
    drop(realtime);
    let stop_result = stream.stop();
    unsafe {
        libc::close(audio_fd);
    }
    let error = result
        .err()
        .or_else(|| stop_result.err().map(AsioError::from));
    error.map_or(Ok(()), Err)
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    stream: &mut AudioStream,
    audio_fd: RawFd,
    stop: &StopSignal,
    buffers: &HostBuffers,
    active: &ActiveChannels,
    callbacks: AsioCallbacks,
    time_info: bool,
    rate: u32,
    period_frames: usize,
    capture: &mut [i32],
    playback: &mut [i32],
    sample_position: &AtomicU64,
    time_stamp: &AtomicU64,
) -> Result<(), AsioError> {
    let mut buffer_index = 0_usize;
    let mut previous_sequence = None;
    while !stop.is_requested() {
        let sequence = wait_cycle(stream, audio_fd, stop)?;
        let Some(sequence) = sequence else {
            break;
        };
        let Some(block_sequence) = stream.capture_buffer_for_sequence(sequence, capture)? else {
            continue;
        };
        let (next_buffer_index, samples) = advance_asio_cycle(
            previous_sequence,
            block_sequence,
            u64::try_from(period_frames).unwrap_or(0),
            buffer_index,
            sample_position.load(Ordering::Relaxed),
        );
        buffer_index = next_buffer_index;
        sample_position.store(samples, Ordering::Release);
        previous_sequence = Some(block_sequence);
        copy_capture_to_host(capture, buffers, active, buffer_index, period_frames);
        let stamp = monotonic_nanos();
        time_stamp.store(stamp, Ordering::Release);
        let callback_start = stamp;
        invoke_callback(callbacks, time_info, buffer_index, samples, stamp, rate)?;
        let callback_duration = monotonic_nanos().saturating_sub(callback_start);
        let period_nanos = u64::try_from(period_frames)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            / u64::from(rate);
        stream.record_callback_timing(callback_duration, period_nanos);
        copy_host_to_playback(buffers, active, buffer_index, playback, period_frames);
        if !stream.submit_playback(block_sequence, playback)? {
            continue;
        }
    }
    Ok(())
}

fn advance_asio_cycle(
    previous_sequence: Option<u64>,
    sequence: u64,
    period_frames: u64,
    buffer_index: usize,
    sample_position: u64,
) -> (usize, u64) {
    let Some(previous) = previous_sequence else {
        return (buffer_index, sample_position);
    };
    let elapsed_periods = sequence.wrapping_sub(previous).max(1);
    (
        buffer_index ^ (elapsed_periods as usize & 1),
        sample_position.saturating_add(period_frames.saturating_mul(elapsed_periods)),
    )
}

fn wait_cycle(
    stream: &mut AudioStream,
    audio_fd: RawFd,
    stop: &StopSignal,
) -> Result<Option<u64>, AsioError> {
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
    ];
    loop {
        if stop.is_requested() {
            return Ok(None);
        }
        match stream.wait_period(Duration::ZERO) {
            Ok(sequence) => return Ok(Some(sequence)),
            Err(ClientError::Timeout) => {}
            Err(error) => return Err(error.into()),
        }
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(ClientError::Io(error).into());
        }
        if fds[1].revents & libc::POLLIN != 0 || stop.is_requested() {
            return Ok(None);
        }
        if fds[0].revents & libc::POLLIN != 0 {
            continue;
        }
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

fn copy_host_to_playback(
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
    (f64::from(sample) * 2_147_483_648.0)
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
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
    *context
        .error
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = error;
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `thread_ops` must point to a valid function table and `out` must be writable.
pub unsafe extern "C" fn sidealsa_asio_new(
    thread_ops: *const AsioThreadOps,
    out: *mut *mut AsioDriver,
) -> c_int {
    if thread_ops.is_null() || out.is_null() {
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
    let result = unsafe { (&*driver).close() };
    if result.is_ok() {
        unsafe {
            drop(Box::from_raw(driver));
        }
    }
    status(result)
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
            Ok(mut result) => {
                result.is_active = request.is_active;
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

    #[test]
    fn configured_pro_lead_is_reported_as_output_latency() {
        let driver = AsioDriver::new();
        let mut inner = driver.inner.lock().expect("lock should work");
        inner.state = DriverState::Initialized;
        inner.device = Some(DeviceInfo {
            name: "Test".into(),
            rate: 48000,
            period_size: 64,
            hardware_period_size: 32,
            buffer_size: 192,
            shared_buffer_size: 256,
            pro_latency_periods: 2,
            pro_realtime_priority: 86,
            shared_latency_periods: 3,
            playback_channels: 8,
            capture_channels: 10,
            playback_ports: Vec::new(),
            capture_ports: Vec::new(),
        });
        drop(inner);

        assert_eq!(
            driver.get_buffer_size().expect("size should work"),
            (64, 64, 64, 0)
        );
        assert_eq!(
            driver.get_latencies().expect("latencies should work"),
            (64, 128)
        );
    }

    #[test]
    fn s32_float_conversion_clips_and_preserves_sign() {
        assert_eq!(s32_to_f32(i32::MIN), -1.0);
        assert_eq!(f32_to_s32(-1.0), i32::MIN);
        assert_eq!(f32_to_s32(1.0), i32::MAX);
        assert_eq!(f32_to_s32(f32::NAN), 0);
        assert_eq!(f32_to_s32(0.5), 1_073_741_824);
    }

    #[test]
    fn host_buffers_have_two_period_pointers() {
        let buffers = HostBuffers::new(1, 1, 64).expect("buffers should allocate");
        let pointers = buffers.input[0].pointers();
        assert!(!pointers[0].is_null());
        assert!(!pointers[1].is_null());
        assert_ne!(pointers[0], pointers[1]);
    }

    #[test]
    fn sequence_gap_advances_position_and_buffer_parity_before_callback() {
        let mut previous = None;
        let mut index = 0;
        let mut position = 0;
        let mut observed = Vec::new();
        for sequence in [10, 11, 13, 14] {
            (index, position) = advance_asio_cycle(previous, sequence, 64, index, position);
            observed.push((index, position));
            previous = Some(sequence);
        }
        assert_eq!(observed, [(0, 0), (1, 64), (1, 192), (0, 256)]);
    }
}
