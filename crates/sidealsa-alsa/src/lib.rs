use std::{
    cmp::Ordering,
    ffi::{CStr, c_char, c_int, c_uint, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    time::Duration,
};

use sidealsa_client::{AudioStream, ClientError, SideAlsaClient, StreamMode};
use sidealsa_protocol::PortDirection;

const MODE_PRO: c_int = 0;
const MODE_SHARED: c_int = 1;
const STREAM_PLAYBACK: c_int = 0;
const STREAM_CAPTURE: c_int = 1;

#[repr(C)]
pub struct SideAlsaChannelArea {
    pub addr: *mut c_void,
    pub first: c_uint,
    pub step: c_uint,
}

unsafe extern "C" {
    fn sidealsa_plugin_open(
        pcm: *mut *mut c_void,
        name: *const c_char,
        root: *mut c_void,
        conf: *mut c_void,
        stream: c_int,
        mode: c_int,
    ) -> c_int;
}

#[unsafe(no_mangle)]
/// # Safety
///
/// Arguments must follow the ALSA external PCM plugin contract.
pub unsafe extern "C" fn _snd_pcm_sidealsa_open(
    pcm: *mut *mut c_void,
    name: *const c_char,
    root: *mut c_void,
    conf: *mut c_void,
    stream: c_int,
    mode: c_int,
) -> c_int {
    unsafe { sidealsa_plugin_open(pcm, name, root, conf, stream, mode) }
}

#[unsafe(no_mangle)]
pub static __snd_pcm_sidealsa_open_dlsym_pcm_001: c_char = 0;

pub struct SideAlsaStream {
    stream: AudioStream,
    pro: bool,
    playback: bool,
    channels: usize,
    period_frames: usize,
    buffer_frames: usize,
    scratch: Vec<i32>,
    capture_discard: Vec<i32>,
    playback_fifo: Vec<i32>,
    playback_fifo_frames: usize,
    capture_frames: usize,
    capture_offset: usize,
    nonblock: bool,
    playback_latency_periods: u64,
    next_playback_sequence: Option<u64>,
    playback_cycle_sequence: Option<u64>,
    last_observed_playback_sequence: Option<u64>,
    last_playback_sequence: Option<u64>,
    start_sequence: Option<u64>,
    position: u64,
    running: bool,
}

#[unsafe(no_mangle)]
/// # Safety
///
/// All pointers must be valid for the call and output pointers must be writable.
pub unsafe extern "C" fn sidealsa_stream_open(
    socket: *const c_char,
    mode: c_int,
    port_id: *const c_char,
    direction: c_int,
    nonblock: c_int,
    stream_out: *mut *mut SideAlsaStream,
    rate_out: *mut c_uint,
    channels_out: *mut c_uint,
    period_out: *mut c_uint,
    minimum_buffer_out: *mut c_uint,
    buffer_out: *mut c_uint,
    poll_fd_out: *mut c_int,
    control_fd_out: *mut c_int,
) -> c_int {
    ffi_status(|| unsafe {
        if socket.is_null()
            || stream_out.is_null()
            || rate_out.is_null()
            || channels_out.is_null()
            || period_out.is_null()
            || minimum_buffer_out.is_null()
            || buffer_out.is_null()
            || poll_fd_out.is_null()
            || control_fd_out.is_null()
        {
            return Err(libc::EINVAL);
        }
        if !matches!(direction, STREAM_PLAYBACK | STREAM_CAPTURE)
            || !matches!(mode, MODE_PRO | MODE_SHARED)
        {
            return Err(libc::EINVAL);
        }
        let socket = CStr::from_ptr(socket).to_str().map_err(|_| libc::EINVAL)?;
        let port = if mode == MODE_SHARED {
            if port_id.is_null() {
                return Err(libc::EINVAL);
            }
            Some(CStr::from_ptr(port_id).to_str().map_err(|_| libc::EINVAL)?)
        } else {
            None
        };

        let mut client = SideAlsaClient::connect(socket).map_err(client_error_code)?;
        let device_info = client.get_info().map_err(client_error_code)?;
        let stream = match (mode, port) {
            (MODE_PRO, None) => client.open_pro().map_err(client_error_code)?,
            (MODE_SHARED, Some(port)) => client.open_shared(port).map_err(client_error_code)?,
            _ => return Err(libc::EINVAL),
        };
        let info = stream.info();
        let expected_mode = match direction {
            STREAM_PLAYBACK => StreamMode::Shared(PortDirection::Playback),
            STREAM_CAPTURE => StreamMode::Shared(PortDirection::Capture),
            _ => unreachable!(),
        };
        if mode == MODE_SHARED && stream.mode() != expected_mode {
            return Err(libc::EINVAL);
        }
        let playback = direction == STREAM_PLAYBACK;
        let channels = if playback {
            info.playback_channels
        } else {
            info.capture_channels
        };
        if channels == 0 {
            return Err(libc::EINVAL);
        }
        let period_frames = usize::try_from(info.period_frames).map_err(|_| libc::EINVAL)?;
        let channels = usize::try_from(channels).map_err(|_| libc::EINVAL)?;
        let scratch_len = period_frames.checked_mul(channels).ok_or(libc::EINVAL)?;
        let capture_discard_len = if playback && mode == MODE_PRO {
            period_frames
                .checked_mul(usize::try_from(info.capture_channels).map_err(|_| libc::EINVAL)?)
                .ok_or(libc::EINVAL)?
        } else {
            0
        };
        let requested_buffer_size = requested_buffer_size(
            mode,
            device_info.buffer_size,
            device_info.shared_buffer_size,
        );
        let buffer_size = plugin_buffer_size(device_info.period_size, requested_buffer_size);
        let alsa_period_size = plugin_period_size(mode, device_info.period_size, buffer_size);
        let buffer_size = plugin_buffer_size(alsa_period_size, buffer_size);
        let buffer_size =
            playback_startup_buffer_size(mode, direction, alsa_period_size, buffer_size);
        let minimum_buffer_size = minimum_plugin_buffer_size(
            mode,
            direction,
            device_info.period_size,
            alsa_period_size,
            device_info.shared_latency_periods,
            buffer_size,
        );
        let buffer_frames = usize::try_from(buffer_size).map_err(|_| libc::EINVAL)?;
        let fifo_len = buffer_frames.checked_mul(channels).ok_or(libc::EINVAL)?;
        let playback_latency_periods = if playback && mode == MODE_SHARED {
            u64::from(device_info.shared_latency_periods)
        } else {
            0
        };
        let channels_out_value = u32::try_from(channels).map_err(|_| libc::EINVAL)?;
        let poll_fd = stream.notification_fd().map_err(client_error_code)?;
        let control_fd = stream.control_fd().map_err(|error| {
            libc::close(poll_fd);
            client_error_code(error)
        })?;
        let stream = Box::new(SideAlsaStream {
            stream,
            pro: mode == MODE_PRO,
            playback,
            channels,
            period_frames,
            buffer_frames,
            scratch: vec![0; scratch_len],
            capture_discard: vec![0; capture_discard_len],
            playback_fifo: vec![0; fifo_len],
            playback_fifo_frames: 0,
            capture_frames: 0,
            capture_offset: 0,
            nonblock: nonblock != 0,
            playback_latency_periods,
            next_playback_sequence: None,
            playback_cycle_sequence: None,
            last_observed_playback_sequence: None,
            last_playback_sequence: None,
            start_sequence: None,
            position: 0,
            running: false,
        });
        *stream_out = Box::into_raw(stream);
        *rate_out = device_info.rate;
        *channels_out = channels_out_value;
        *period_out = alsa_period_size;
        *minimum_buffer_out = minimum_buffer_size;
        *buffer_out = buffer_size;
        *poll_fd_out = poll_fd;
        *control_fd_out = control_fd;
        Ok(())
    })
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `stream` must be a live handle returned by `sidealsa_stream_open`.
pub unsafe extern "C" fn sidealsa_stream_start(stream: *mut SideAlsaStream) -> c_int {
    ffi_status(|| unsafe {
        let stream = stream.as_mut().ok_or(libc::EINVAL)?;
        stream.start()
    })
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `stream` must be a live handle returned by `sidealsa_stream_open`.
pub unsafe extern "C" fn sidealsa_stream_stop(stream: *mut SideAlsaStream) -> c_int {
    ffi_status(|| unsafe {
        let stream = stream.as_mut().ok_or(libc::EINVAL)?;
        let position = stream.position();
        stream.stream.stop().map_err(client_error_code)?;
        stream.reset_transfer_state();
        stream.position = position;
        stream.running = false;
        Ok(())
    })
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `stream` must be a live handle returned by `sidealsa_stream_open`.
pub unsafe extern "C" fn sidealsa_stream_prepare(stream: *mut SideAlsaStream) -> c_int {
    ffi_status(|| unsafe {
        let stream = stream.as_mut().ok_or(libc::EINVAL)?;
        stream.stream.prepare().map_err(client_error_code)?;
        stream.reset_transfer_state();
        stream.running = false;
        Ok(())
    })
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `stream` must be a live handle returned by `sidealsa_stream_open`.
pub unsafe extern "C" fn sidealsa_stream_set_nonblock(
    stream: *mut SideAlsaStream,
    nonblock: c_int,
) -> c_int {
    ffi_status(|| unsafe {
        let stream = stream.as_mut().ok_or(libc::EINVAL)?;
        stream.nonblock = nonblock != 0;
        Ok(())
    })
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `stream` must be a live handle returned by `sidealsa_stream_open`.
pub unsafe extern "C" fn sidealsa_stream_drain(stream: *mut SideAlsaStream) -> c_int {
    ffi_status(|| unsafe {
        let stream = stream.as_mut().ok_or(libc::EINVAL)?;
        stream.drain()
    })
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `stream` and `areas` must remain valid for the duration of the call.
pub unsafe extern "C" fn sidealsa_stream_transfer(
    stream: *mut SideAlsaStream,
    areas: *const SideAlsaChannelArea,
    offset: usize,
    frames: usize,
) -> isize {
    ffi_value(|| unsafe {
        let stream = stream.as_mut().ok_or(libc::EINVAL)?;
        stream.transfer(areas, offset, frames)
    })
    .unwrap_or_else(|error| -(error as isize))
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `stream` must be a live handle returned by `sidealsa_stream_open`.
pub unsafe extern "C" fn sidealsa_stream_record_playback_xrun(stream: *mut SideAlsaStream) {
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        if let Some(stream) = stream.as_ref()
            && stream.playback
            && !stream.pro
        {
            stream.stream.record_playback_xrun();
        }
    }));
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `stream` must be null or a live handle returned by `sidealsa_stream_open`.
pub unsafe extern "C" fn sidealsa_stream_position(stream: *const SideAlsaStream) -> u64 {
    catch_unwind(AssertUnwindSafe(|| unsafe {
        stream.as_ref().map_or(0, SideAlsaStream::position)
    }))
    .unwrap_or(0)
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `stream` must be a live handle returned by `sidealsa_stream_open` and must not
/// be used after this call.
pub unsafe extern "C" fn sidealsa_stream_close(stream: *mut SideAlsaStream) -> c_int {
    ffi_status(|| unsafe {
        let mut stream = Box::from_raw(stream.as_mut().ok_or(libc::EINVAL)?);
        let result = stream.stream.close().map_err(client_error_code);
        drop(stream);
        result
    })
}

impl SideAlsaStream {
    fn start(&mut self) -> Result<(), c_int> {
        self.stream.start().map_err(client_error_code)?;
        if self.running {
            return Ok(());
        }
        self.start_sequence = self.stream.activation_sequence();
        self.position = 0;
        if let Err(error) = self.flush_partial_playback() {
            let _ = self.stream.stop();
            self.reset_transfer_state();
            return Err(error);
        }
        self.running = true;
        Ok(())
    }

    fn position(&self) -> u64 {
        if !self.running {
            return self.position;
        }
        if let Some(origin) = self
            .start_sequence
            .or_else(|| self.stream.activation_sequence())
        {
            sequence_forward_distance(origin, self.stream.cycle_sequence())
                .and_then(|elapsed| elapsed.checked_mul(self.period_frames as u64))
                .unwrap_or(self.position)
        } else {
            self.position
        }
    }

    fn reset_transfer_state(&mut self) {
        self.playback_fifo_frames = 0;
        self.capture_frames = 0;
        self.capture_offset = 0;
        self.next_playback_sequence = None;
        self.playback_cycle_sequence = None;
        self.last_observed_playback_sequence = None;
        self.last_playback_sequence = None;
        self.start_sequence = None;
        self.position = 0;
        self.scratch.fill(0);
    }

    fn transfer(
        &mut self,
        areas: *const SideAlsaChannelArea,
        offset: usize,
        frames: usize,
    ) -> Result<isize, c_int> {
        if areas.is_null() || frames == 0 {
            return Err(libc::EINVAL);
        }
        if self.playback {
            self.transfer_playback(areas, offset, frames)
        } else {
            self.transfer_capture(areas, offset, frames)
        }
    }

    fn transfer_playback(
        &mut self,
        areas: *const SideAlsaChannelArea,
        offset: usize,
        frames: usize,
    ) -> Result<isize, c_int> {
        if !self.running {
            return self.queue_prepared_playback(areas, offset, frames);
        }

        let mut consumed = 0;
        while consumed < frames {
            if self.playback_fifo_frames == self.buffer_frames {
                if !self.flush_playback_blocks()? {
                    return if consumed > 0 {
                        isize::try_from(consumed).map_err(|_| libc::EOVERFLOW)
                    } else {
                        Err(libc::EAGAIN)
                    };
                }
                continue;
            }

            let source_offset = offset.checked_add(consumed).ok_or(libc::EOVERFLOW)?;
            let chunk = copy_into_fifo(
                &mut self.playback_fifo,
                &mut self.playback_fifo_frames,
                self.buffer_frames,
                areas,
                source_offset,
                frames - consumed,
                self.channels,
            )?;
            consumed += chunk;
            let _ = self.flush_playback_blocks()?;
        }
        isize::try_from(consumed).map_err(|_| libc::EOVERFLOW)
    }

    fn queue_prepared_playback(
        &mut self,
        areas: *const SideAlsaChannelArea,
        offset: usize,
        frames: usize,
    ) -> Result<isize, c_int> {
        let accepted = copy_into_fifo(
            &mut self.playback_fifo,
            &mut self.playback_fifo_frames,
            self.buffer_frames,
            areas,
            offset,
            frames,
            self.channels,
        )?;
        if accepted == 0 {
            return Err(libc::EAGAIN);
        }
        isize::try_from(accepted).map_err(|_| libc::EOVERFLOW)
    }

    fn flush_partial_playback(&mut self) -> Result<(), c_int> {
        if self.playback_fifo_frames > 0 && self.playback_fifo_frames < self.period_frames {
            pad_final_playback_period(
                &mut self.playback_fifo,
                &mut self.playback_fifo_frames,
                self.period_frames,
                self.channels,
            )?;
        }
        let _ = self.flush_playback_blocks()?;
        Ok(())
    }

    fn drain(&mut self) -> Result<(), c_int> {
        if !self.playback {
            return Ok(());
        }
        if !self.running {
            self.start()?;
        }
        pad_final_playback_period(
            &mut self.playback_fifo,
            &mut self.playback_fifo_frames,
            self.period_frames,
            self.channels,
        )?;

        while self.playback_fifo_frames >= self.period_frames {
            let progressed = self.flush_playback_blocks()?;
            if self.playback_fifo_frames < self.period_frames {
                break;
            }
            if self.nonblock && !progressed {
                return Err(libc::EAGAIN);
            }
        }

        let Some(last_sequence) = self.last_playback_sequence else {
            return Ok(());
        };
        loop {
            if let Some(observed) = self.playback_cycle_sequence
                && playback_sequence_consumed(
                    last_sequence,
                    observed,
                    self.playback_latency_periods,
                    self.pro,
                )?
            {
                self.last_playback_sequence = None;
                return Ok(());
            }
            let observed = self.wait_period()?;
            self.observe_playback_cycle(observed)?;
        }
    }

    fn flush_playback_blocks(&mut self) -> Result<bool, c_int> {
        let mut progressed = false;
        while self.playback_fifo_frames >= self.period_frames {
            let reuse_observed = match (self.playback_cycle_sequence, self.next_playback_sequence) {
                (Some(observed), Some(next)) => {
                    match sequence_order(self.stream.cycle_sequence(), observed) {
                        Some(Ordering::Equal) => {
                            match sequence_order(
                                next,
                                latest_publishable_playback_sequence(observed),
                            ) {
                                Some(Ordering::Less | Ordering::Equal) => true,
                                Some(Ordering::Greater) => false,
                                None => {
                                    self.clear_playback_fifo();
                                    return Err(libc::EPIPE);
                                }
                            }
                        }
                        Some(Ordering::Greater) => false,
                        Some(Ordering::Less) | None => {
                            self.clear_playback_fifo();
                            return Err(libc::EPIPE);
                        }
                    }
                }
                _ => false,
            };
            let observed = if reuse_observed {
                self.playback_cycle_sequence
                    .expect("an observed cycle is available")
            } else {
                match self.wait_period() {
                    Ok(sequence) => {
                        self.observe_playback_cycle(sequence)?;
                        sequence
                    }
                    Err(libc::EAGAIN) if self.nonblock => return Ok(progressed),
                    Err(error) => return Err(error),
                }
            };
            let queued_periods = self.playback_fifo_frames / self.period_frames;
            let plan = plan_playback_sequence_with_queue(
                self.next_playback_sequence,
                observed,
                self.playback_latency_periods,
                self.pro,
                queued_periods,
            )?;
            if plan.expired_periods > 0 {
                let action =
                    expired_playback_action(self.pro, plan.expired_periods, queued_periods);
                self.discard_playback_periods(plan.expired_periods)?;
                match action {
                    ExpiredPlaybackAction::Xrun => {
                        self.next_playback_sequence = Some(plan.sequence);
                        return Err(libc::EPIPE);
                    }
                    ExpiredPlaybackAction::Realign { discarded_periods } => {
                        self.stream.record_expired_playback_periods(
                            u64::try_from(discarded_periods).unwrap_or(u64::MAX),
                        );
                        self.next_playback_sequence =
                            realigned_playback_sequence(plan.sequence, self.playback_fifo_frames);
                        if self.next_playback_sequence.is_none() {
                            self.playback_cycle_sequence = None;
                        }
                        progressed = true;
                        continue;
                    }
                }
            }
            let sequence = plan.sequence;
            self.next_playback_sequence = Some(sequence);
            match sequence_order(sequence, latest_publishable_playback_sequence(observed)) {
                Some(Ordering::Greater) => {
                    self.playback_cycle_sequence = None;
                    continue;
                }
                Some(Ordering::Less | Ordering::Equal) => {}
                None => {
                    self.clear_playback_fifo();
                    return Err(libc::EPIPE);
                }
            }
            let samples = self
                .period_frames
                .checked_mul(self.channels)
                .ok_or(libc::EOVERFLOW)?;
            self.scratch[..samples].copy_from_slice(&self.playback_fifo[..samples]);
            if !self
                .stream
                .submit_playback(sequence, &self.scratch)
                .map_err(client_error_code)?
            {
                self.playback_cycle_sequence = None;
                return Ok(progressed);
            }
            self.discard_playback_periods(1)?;
            self.next_playback_sequence = Some(sequence.wrapping_add(1));
            self.last_playback_sequence = Some(sequence);
            self.update_position(sequence)?;
            progressed = true;
        }
        Ok(progressed)
    }

    fn observe_playback_cycle(&mut self, sequence: u64) -> Result<(), c_int> {
        if let Some(previous) = self.last_observed_playback_sequence
            && !matches!(
                sequence_order(sequence, previous),
                Some(Ordering::Equal | Ordering::Greater)
            )
        {
            self.clear_playback_fifo();
            return Err(libc::EPIPE);
        }
        self.last_observed_playback_sequence = Some(sequence);
        self.playback_cycle_sequence = Some(sequence);
        Ok(())
    }

    fn clear_playback_fifo(&mut self) {
        self.playback_fifo_frames = 0;
        self.next_playback_sequence = None;
        self.playback_cycle_sequence = None;
        self.last_observed_playback_sequence = None;
        self.last_playback_sequence = None;
    }

    fn discard_playback_periods(&mut self, periods: usize) -> Result<(), c_int> {
        discard_playback_fifo_periods(
            &mut self.playback_fifo,
            &mut self.playback_fifo_frames,
            periods,
            self.period_frames,
            self.channels,
        )
    }

    fn transfer_capture(
        &mut self,
        areas: *const SideAlsaChannelArea,
        offset: usize,
        frames: usize,
    ) -> Result<isize, c_int> {
        if !self.running {
            return Err(libc::EPIPE);
        }

        let mut consumed = 0;
        while consumed < frames {
            if self.capture_frames == 0 {
                let sequence = match self.wait_period() {
                    Ok(sequence) => sequence,
                    Err(error) if error == libc::EAGAIN && consumed > 0 => {
                        return isize::try_from(consumed).map_err(|_| libc::EOVERFLOW);
                    }
                    Err(error) => return Err(error),
                };
                let captured = self
                    .stream
                    .capture_buffer(&mut self.scratch)
                    .map_err(client_error_code)?;
                if captured.is_none() {
                    if consumed > 0 {
                        return isize::try_from(consumed).map_err(|_| libc::EOVERFLOW);
                    }
                    return Err(libc::EAGAIN);
                }
                self.capture_frames = self.period_frames;
                self.capture_offset = 0;
                self.update_position(captured.unwrap_or(sequence))?;
            }

            let chunk = self.capture_frames.min(frames - consumed);
            let destination_offset = offset.checked_add(consumed).ok_or(libc::EOVERFLOW)?;
            let source_start = self
                .capture_offset
                .checked_mul(self.channels)
                .ok_or(libc::EOVERFLOW)?;
            let source_end = source_start
                .checked_add(chunk.checked_mul(self.channels).ok_or(libc::EOVERFLOW)?)
                .ok_or(libc::EOVERFLOW)?;
            unsafe {
                copy_to_area(
                    areas,
                    destination_offset,
                    chunk,
                    self.channels,
                    &self.scratch[source_start..source_end],
                )?;
            }
            self.capture_offset += chunk;
            self.capture_frames -= chunk;
            consumed += chunk;
            if self.capture_frames == 0 {
                self.capture_offset = 0;
            }
        }
        isize::try_from(consumed).map_err(|_| libc::EOVERFLOW)
    }

    fn wait_period(&mut self) -> Result<u64, c_int> {
        loop {
            match self.stream.wait_period(wait_timeout(self.nonblock)) {
                Ok(sequence) => {
                    if self.start_sequence.is_none() {
                        self.start_sequence = self.stream.activation_sequence();
                    }
                    let sequence = if self.playback && self.pro {
                        self.stream
                            .capture_buffer_for_sequence(sequence, &mut self.capture_discard)
                            .map_err(client_error_code)?
                            .unwrap_or(sequence)
                    } else {
                        sequence
                    };
                    return Ok(sequence);
                }
                Err(ClientError::Timeout) if !self.nonblock => continue,
                Err(ClientError::Timeout) => return Err(libc::EAGAIN),
                Err(error) => return Err(client_error_code(error)),
            }
        }
    }

    fn update_position(&mut self, sequence: u64) -> Result<(), c_int> {
        let origin = self
            .start_sequence
            .or_else(|| self.stream.activation_sequence())
            .ok_or(libc::EIO)?;
        let elapsed = match sequence_order(sequence, origin) {
            Some(Ordering::Less | Ordering::Equal) => 0,
            Some(Ordering::Greater) => sequence.wrapping_sub(origin),
            None => return Err(libc::EPIPE),
        };
        self.position = elapsed
            .checked_mul(self.period_frames as u64)
            .ok_or(libc::EOVERFLOW)?;
        Ok(())
    }
}

fn wait_timeout(nonblock: bool) -> Duration {
    if nonblock {
        Duration::ZERO
    } else {
        Duration::from_secs(1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlaybackSequencePlan {
    sequence: u64,
    expired_periods: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExpiredPlaybackAction {
    Xrun,
    Realign { discarded_periods: usize },
}

fn expired_playback_action(
    pro: bool,
    expired_periods: usize,
    queued_periods: usize,
) -> ExpiredPlaybackAction {
    if pro {
        return ExpiredPlaybackAction::Xrun;
    }
    let discarded_periods = expired_periods.min(queued_periods);
    ExpiredPlaybackAction::Realign { discarded_periods }
}

fn realigned_playback_sequence(sequence: u64, queued_frames: usize) -> Option<u64> {
    (queued_frames != 0).then_some(sequence)
}

fn plan_playback_sequence_with_queue(
    next: Option<u64>,
    observed: u64,
    latency: u64,
    same_cycle: bool,
    queued_periods: usize,
) -> Result<PlaybackSequencePlan, c_int> {
    let newest = newest_playback_sequence(observed, same_cycle);
    let earliest = if same_cycle {
        observed
    } else {
        observed.wrapping_sub(latency).wrapping_add(1)
    };
    if let Some(next) = next {
        return match sequence_order(next, earliest) {
            Some(Ordering::Less) => Ok(PlaybackSequencePlan {
                sequence: earliest,
                expired_periods: usize::try_from(earliest.wrapping_sub(next)).unwrap_or(usize::MAX),
            }),
            Some(Ordering::Equal | Ordering::Greater) => Ok(PlaybackSequencePlan {
                sequence: next,
                expired_periods: 0,
            }),
            None => Err(libc::EPIPE),
        };
    }

    let queued_behind =
        u64::try_from(queued_periods.saturating_sub(1)).map_err(|_| libc::EOVERFLOW)?;
    let aligned = newest.wrapping_sub(queued_behind);
    let sequence = match sequence_order(aligned, earliest) {
        Some(Ordering::Less) => earliest,
        Some(Ordering::Equal | Ordering::Greater) => aligned,
        None => return Err(libc::EPIPE),
    };
    Ok(PlaybackSequencePlan {
        sequence,
        expired_periods: 0,
    })
}

fn newest_playback_sequence(observed: u64, same_cycle: bool) -> u64 {
    if same_cycle {
        observed
    } else {
        observed.wrapping_add(1)
    }
}

fn latest_publishable_playback_sequence(observed: u64) -> u64 {
    observed.wrapping_add(1)
}

fn playback_sequence_consumed(
    submitted: u64,
    observed: u64,
    latency: u64,
    same_cycle: bool,
) -> Result<bool, c_int> {
    let consumed_at = submitted.wrapping_add(if same_cycle { 1 } else { latency });
    match sequence_order(observed, consumed_at) {
        Some(Ordering::Less) => Ok(false),
        Some(Ordering::Equal | Ordering::Greater) => Ok(true),
        None => Err(libc::EPIPE),
    }
}

fn sequence_order(candidate: u64, reference: u64) -> Option<Ordering> {
    const HALF_RANGE: u64 = 1_u64 << 63;

    let distance = candidate.wrapping_sub(reference);
    if distance == 0 {
        Some(Ordering::Equal)
    } else if distance < HALF_RANGE {
        Some(Ordering::Greater)
    } else if distance == HALF_RANGE {
        None
    } else {
        Some(Ordering::Less)
    }
}

fn sequence_forward_distance(origin: u64, current: u64) -> Option<u64> {
    match sequence_order(current, origin) {
        Some(Ordering::Equal) => Some(0),
        Some(Ordering::Greater) => Some(current.wrapping_sub(origin)),
        Some(Ordering::Less) | None => None,
    }
}

fn pad_final_playback_period(
    fifo: &mut [i32],
    queued_frames: &mut usize,
    period_frames: usize,
    channels: usize,
) -> Result<(), c_int> {
    let remainder = *queued_frames % period_frames;
    if remainder == 0 {
        return Ok(());
    }
    let padded_frames = queued_frames
        .checked_add(period_frames - remainder)
        .ok_or(libc::EOVERFLOW)?;
    let start = queued_frames.checked_mul(channels).ok_or(libc::EOVERFLOW)?;
    let end = padded_frames.checked_mul(channels).ok_or(libc::EOVERFLOW)?;
    if end > fifo.len() {
        return Err(libc::EOVERFLOW);
    }
    fifo[start..end].fill(0);
    *queued_frames = padded_frames;
    Ok(())
}

fn discard_playback_fifo_periods(
    fifo: &mut [i32],
    queued_frames: &mut usize,
    periods: usize,
    period_frames: usize,
    channels: usize,
) -> Result<(), c_int> {
    let frames = periods.saturating_mul(period_frames).min(*queued_frames);
    let samples = frames.checked_mul(channels).ok_or(libc::EOVERFLOW)?;
    let remaining_frames = *queued_frames - frames;
    let remaining_samples = remaining_frames
        .checked_mul(channels)
        .ok_or(libc::EOVERFLOW)?;
    fifo.copy_within(samples..samples + remaining_samples, 0);
    *queued_frames = remaining_frames;
    Ok(())
}

fn copy_into_fifo(
    fifo: &mut [i32],
    queued_frames: &mut usize,
    capacity_frames: usize,
    areas: *const SideAlsaChannelArea,
    offset: usize,
    frames: usize,
    channels: usize,
) -> Result<usize, c_int> {
    let capacity = capacity_frames.saturating_sub(*queued_frames);
    let accepted = capacity.min(frames);
    if accepted == 0 {
        return Ok(0);
    }
    let destination_start = queued_frames.checked_mul(channels).ok_or(libc::EOVERFLOW)?;
    let destination_end = destination_start
        .checked_add(accepted.checked_mul(channels).ok_or(libc::EOVERFLOW)?)
        .ok_or(libc::EOVERFLOW)?;
    unsafe {
        copy_from_area(
            areas,
            offset,
            accepted,
            channels,
            &mut fifo[destination_start..destination_end],
        )?;
    }
    *queued_frames += accepted;
    Ok(accepted)
}

fn plugin_buffer_size(period_size: u32, buffer_size: u32) -> u32 {
    let periods = buffer_size.div_ceil(period_size).max(2);
    period_size.saturating_mul(periods)
}

fn requested_buffer_size(mode: c_int, hardware: u32, shared: u32) -> u32 {
    if mode == MODE_SHARED {
        shared
    } else {
        hardware
    }
}

fn plugin_period_size(mode: c_int, internal_period_size: u32, buffer_size: u32) -> u32 {
    if mode != MODE_SHARED || internal_period_size == 0 {
        return internal_period_size;
    }
    let internal_periods = buffer_size / internal_period_size;
    let mut aggregated_periods = (internal_periods / 2).max(1);
    while aggregated_periods > 1 && !internal_periods.is_multiple_of(aggregated_periods) {
        aggregated_periods -= 1;
    }
    // Keep daemon transfers aligned to the internal period while exposing two
    // buffered ALSA periods whenever the configured geometry permits it.
    internal_period_size.saturating_mul(aggregated_periods)
}

fn playback_startup_buffer_size(
    mode: c_int,
    direction: c_int,
    period_size: u32,
    buffer_size: u32,
) -> u32 {
    // PipeWire needs one extra period of capacity while its first graph block
    // follows the startup silence. The steady-state target remains one period.
    if mode == MODE_SHARED && direction == STREAM_PLAYBACK {
        buffer_size.max(period_size.saturating_mul(3))
    } else {
        buffer_size
    }
}

fn minimum_plugin_buffer_size(
    mode: c_int,
    direction: c_int,
    internal_period_size: u32,
    plugin_period_size: u32,
    shared_latency_periods: u32,
    maximum_buffer_size: u32,
) -> u32 {
    if mode != MODE_SHARED {
        return maximum_buffer_size;
    }
    let internal_periods = if direction == STREAM_PLAYBACK {
        shared_latency_periods.saturating_add(1).max(2)
    } else {
        2
    };
    let plugin_periods = if direction == STREAM_PLAYBACK { 3 } else { 2 };
    let required_frames = internal_period_size
        .saturating_mul(internal_periods)
        .max(plugin_period_size.saturating_mul(plugin_periods));
    plugin_buffer_size(plugin_period_size, required_frames).min(maximum_buffer_size)
}

unsafe fn copy_from_area(
    areas: *const SideAlsaChannelArea,
    offset: usize,
    frames: usize,
    channels: usize,
    destination: &mut [i32],
) -> Result<(), c_int> {
    let area = unsafe { &*areas };
    validate_area(area, channels)?;
    let base = area.addr.cast::<u8>();
    let first = usize::try_from(area.first / 8).map_err(|_| libc::EINVAL)?;
    let step = usize::try_from(area.step / 8).map_err(|_| libc::EINVAL)?;
    for frame in 0..frames {
        for channel in 0..channels {
            let byte_offset = first
                .checked_add((offset + frame).checked_mul(step).ok_or(libc::EOVERFLOW)?)
                .and_then(|value| value.checked_add(channel.checked_mul(size_of_i32())?))
                .ok_or(libc::EOVERFLOW)?;
            let source = unsafe { base.add(byte_offset) };
            let bytes = unsafe { ptr::read_unaligned(source.cast::<[u8; 4]>()) };
            destination[frame * channels + channel] = i32::from_le_bytes(bytes);
        }
    }
    Ok(())
}

unsafe fn copy_to_area(
    areas: *const SideAlsaChannelArea,
    offset: usize,
    frames: usize,
    channels: usize,
    source: &[i32],
) -> Result<(), c_int> {
    let area = unsafe { &*areas };
    validate_area(area, channels)?;
    let base = area.addr.cast::<u8>();
    let first = usize::try_from(area.first / 8).map_err(|_| libc::EINVAL)?;
    let step = usize::try_from(area.step / 8).map_err(|_| libc::EINVAL)?;
    for frame in 0..frames {
        for channel in 0..channels {
            let byte_offset = first
                .checked_add((offset + frame).checked_mul(step).ok_or(libc::EOVERFLOW)?)
                .and_then(|value| value.checked_add(channel.checked_mul(size_of_i32())?))
                .ok_or(libc::EOVERFLOW)?;
            let destination = unsafe { base.add(byte_offset) };
            let bytes = source[frame * channels + channel].to_le_bytes();
            unsafe { ptr::write_unaligned(destination.cast::<[u8; 4]>(), bytes) };
        }
    }
    Ok(())
}

fn validate_area(area: &SideAlsaChannelArea, channels: usize) -> Result<(), c_int> {
    if area.addr.is_null()
        || !area.first.is_multiple_of(8)
        || !area.step.is_multiple_of(8)
        || usize::try_from(area.step / 8).map_err(|_| libc::EINVAL)?
            < channels.checked_mul(size_of_i32()).ok_or(libc::EOVERFLOW)?
    {
        Err(libc::EINVAL)
    } else {
        Ok(())
    }
}

fn size_of_i32() -> usize {
    std::mem::size_of::<i32>()
}

fn client_error_code(error: ClientError) -> c_int {
    match error {
        ClientError::Io(error) => error.raw_os_error().unwrap_or(libc::EIO),
        ClientError::Protocol(_) | ClientError::UnexpectedResponse(_) => libc::EPROTO,
        ClientError::Shared(_) => libc::EIO,
        ClientError::Busy => libc::EBUSY,
        ClientError::Unsupported => libc::ENOTSUP,
        ClientError::Daemon { code, .. } => match code {
            sidealsa_protocol::ErrorCode::InvalidRequest => libc::EINVAL,
            sidealsa_protocol::ErrorCode::NotOwner => libc::EPERM,
            sidealsa_protocol::ErrorCode::BadState => libc::EPIPE,
            sidealsa_protocol::ErrorCode::Internal => libc::EIO,
        },
        ClientError::InvalidDescriptorCount(_) => libc::EPROTO,
        ClientError::Closed => libc::ENODEV,
        ClientError::NotStarted | ClientError::CaptureDiscontinuity => libc::EPIPE,
        ClientError::MissingDirection(_) | ClientError::BufferNotReady => libc::EINVAL,
        ClientError::Timeout => libc::EAGAIN,
    }
}

fn ffi_status<F>(function: F) -> c_int
where
    F: FnOnce() -> Result<(), c_int>,
{
    catch_unwind(AssertUnwindSafe(function))
        .unwrap_or(Err(libc::EIO))
        .map_or_else(|error| -error, |_| 0)
}

fn ffi_value<F>(function: F) -> Result<isize, c_int>
where
    F: FnOnce() -> Result<isize, c_int>,
{
    catch_unwind(AssertUnwindSafe(function)).unwrap_or(Err(libc::EIO))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playback_plan(
        next: Option<u64>,
        observed: u64,
        latency: u64,
        same_cycle: bool,
        queued_periods: usize,
    ) -> PlaybackSequencePlan {
        plan_playback_sequence_with_queue(next, observed, latency, same_cycle, queued_periods)
            .expect("test sequence should have an unambiguous order")
    }

    #[test]
    fn ioplug_buffer_has_at_least_two_periods() {
        assert_eq!(plugin_buffer_size(32, 64), 64);
        assert_eq!(plugin_buffer_size(64, 64), 128);
        assert_eq!(plugin_buffer_size(64, 160), 192);
        assert_eq!(plugin_buffer_size(64, 256), 256);
        assert_eq!(requested_buffer_size(MODE_PRO, 256, 512), 256);
        assert_eq!(requested_buffer_size(MODE_SHARED, 256, 512), 512);
        assert_eq!(plugin_period_size(MODE_PRO, 64, 256), 64);
        assert_eq!(plugin_period_size(MODE_SHARED, 64, 512), 256);
        assert_eq!(plugin_period_size(MODE_SHARED, 64, 1024), 512);
        assert_eq!(plugin_period_size(MODE_SHARED, 64, 384), 192);
        assert_eq!(plugin_period_size(MODE_SHARED, 64, 448), 64);
        assert_eq!(
            playback_startup_buffer_size(MODE_SHARED, STREAM_PLAYBACK, 256, 512),
            768
        );
        assert_eq!(
            playback_startup_buffer_size(MODE_SHARED, STREAM_CAPTURE, 256, 512),
            512
        );
        assert_eq!(
            playback_startup_buffer_size(MODE_PRO, STREAM_PLAYBACK, 64, 256),
            256
        );
        assert_eq!(
            minimum_plugin_buffer_size(MODE_SHARED, STREAM_PLAYBACK, 64, 256, 7, 768),
            768
        );
        assert_eq!(
            minimum_plugin_buffer_size(MODE_SHARED, STREAM_CAPTURE, 64, 256, 7, 512),
            512
        );
        assert_eq!(
            minimum_plugin_buffer_size(MODE_PRO, STREAM_PLAYBACK, 64, 64, 7, 256),
            256
        );
    }

    #[test]
    fn interleaved_area_round_trip_uses_s32_le() {
        let source = [1_i32, -2, 3, -4, 5, -6, 7, -8];
        let channels = 2;
        let frames = 4;
        let area = SideAlsaChannelArea {
            addr: source.as_ptr().cast_mut().cast(),
            first: 0,
            step: (channels * 32) as c_uint,
        };
        let mut decoded = [0_i32; 8];
        unsafe {
            copy_from_area(&area, 0, frames, channels, &mut decoded)
                .expect("interleaved input should decode");
        }
        assert_eq!(decoded, source);

        let mut destination = [0_i32; 8];
        let destination_area = SideAlsaChannelArea {
            addr: destination.as_mut_ptr().cast(),
            first: 0,
            step: (channels * 32) as c_uint,
        };
        unsafe {
            copy_to_area(&destination_area, 0, frames, channels, &source)
                .expect("interleaved output should encode");
        }
        assert_eq!(destination, source);
    }

    #[test]
    fn prepared_playback_queues_only_available_frames() {
        let source = [1_i32, -2, 3, -4, 5, -6];
        let area = SideAlsaChannelArea {
            addr: source.as_ptr().cast_mut().cast(),
            first: 0,
            step: 64,
        };
        let mut fifo = [0_i32; 8];
        let mut queued = 0;
        let accepted = copy_into_fifo(&mut fifo, &mut queued, 4, &area, 0, 3, 2)
            .expect("prepared frames should copy");

        assert_eq!(accepted, 3);
        assert_eq!(queued, 3);
        assert_eq!(&fifo[..6], &source[..6]);
        assert_eq!(
            copy_into_fifo(&mut fifo, &mut queued, 3, &area, 3, 1, 2)
                .expect("full FIFO should accept no additional frames"),
            0
        );
    }

    #[test]
    fn labeled_shared_fifo_sequence_is_never_moved_forward() {
        assert_eq!(
            playback_plan(Some(103), 103, 3, false, 1),
            PlaybackSequencePlan {
                sequence: 103,
                expired_periods: 0,
            }
        );
        assert_eq!(
            playback_plan(Some(105), 104, 3, false, 1),
            PlaybackSequencePlan {
                sequence: 105,
                expired_periods: 0,
            }
        );
    }

    #[test]
    fn unlabeled_shared_fifo_uses_available_catchup_blocks() {
        assert_eq!(
            playback_plan(None, 103, 3, false, 2),
            PlaybackSequencePlan {
                sequence: 103,
                expired_periods: 0,
            }
        );
        assert_eq!(
            playback_plan(Some(104), 103, 3, false, 1),
            PlaybackSequencePlan {
                sequence: 104,
                expired_periods: 0,
            }
        );
    }

    #[test]
    fn expired_fifo_periods_are_counted_instead_of_relabelled() {
        assert_eq!(
            playback_plan(Some(101), 104, 3, false, 3),
            PlaybackSequencePlan {
                sequence: 102,
                expired_periods: 1,
            }
        );
        assert_eq!(
            playback_plan(Some(101), 106, 3, false, 1),
            PlaybackSequencePlan {
                sequence: 104,
                expired_periods: 3,
            }
        );
        assert_eq!(
            playback_plan(Some(u64::MAX), 1, 2, false, 1),
            PlaybackSequencePlan {
                sequence: 0,
                expired_periods: 1,
            }
        );

        let mut fifo = [10, 11, 20, 21, 30, 31];
        let mut queued_frames = 6;
        discard_playback_fifo_periods(&mut fifo, &mut queued_frames, 2, 2, 1)
            .expect("expired periods should be discarded");
        assert_eq!(queued_frames, 2);
        assert_eq!(&fifo[..2], &[30, 31]);

        let mut exhausted_fifo = [10, 11];
        let mut exhausted_frames = 2;
        discard_playback_fifo_periods(&mut exhausted_fifo, &mut exhausted_frames, 3, 2, 1)
            .expect("expiry beyond the queue should discard the whole queue");
        assert_eq!(exhausted_frames, 0);

        assert_eq!(
            expired_playback_action(false, 1, 3),
            ExpiredPlaybackAction::Realign {
                discarded_periods: 1,
            }
        );
        assert_eq!(
            expired_playback_action(false, 3, 1),
            ExpiredPlaybackAction::Realign {
                discarded_periods: 1,
            }
        );
        assert_eq!(realigned_playback_sequence(104, 0), None);
        assert_eq!(realigned_playback_sequence(104, 1), Some(104));

        let mut partial_fifo = [10, 11, 20, 21, 30];
        let mut partial_frames = 5;
        discard_playback_fifo_periods(&mut partial_fifo, &mut partial_frames, 2, 2, 1)
            .expect("complete expired periods should leave a partial period queued");
        assert_eq!(partial_frames, 1);
        assert_eq!(realigned_playback_sequence(104, partial_frames), Some(104));
    }

    #[test]
    fn pro_expired_playback_still_requires_xrun_recovery() {
        assert_eq!(
            expired_playback_action(true, 1, 3),
            ExpiredPlaybackAction::Xrun
        );
    }

    #[test]
    fn pro_expiry_uses_same_cycle_deadline_across_wrap() {
        assert_eq!(
            playback_plan(None, 100, 0, true, 1),
            PlaybackSequencePlan {
                sequence: 100,
                expired_periods: 0,
            }
        );
        assert_eq!(
            playback_plan(Some(101), 101, 0, true, 1),
            PlaybackSequencePlan {
                sequence: 101,
                expired_periods: 0,
            }
        );
        assert_eq!(
            playback_plan(Some(u64::MAX), 1, 0, true, 1),
            PlaybackSequencePlan {
                sequence: 1,
                expired_periods: 2,
            }
        );
    }

    #[test]
    fn sequence_order_uses_wrapping_half_range() {
        assert_eq!(sequence_order(0, u64::MAX), Some(Ordering::Greater));
        assert_eq!(sequence_order(u64::MAX, 0), Some(Ordering::Less));
        assert_eq!(sequence_order(1_u64 << 63, 0), None);
        assert_eq!(sequence_forward_distance(u64::MAX - 1, 1), Some(3));
    }

    #[test]
    fn drain_padding_preserves_samples_and_silences_final_period() {
        let mut fifo = [-1_i32; 16];
        let mut queued_frames = 6;

        pad_final_playback_period(&mut fifo, &mut queued_frames, 4, 2)
            .expect("final playback period should fit");

        assert_eq!(queued_frames, 8);
        assert_eq!(&fifo[..12], &[-1; 12]);
        assert_eq!(&fifo[12..], &[0; 4]);
    }

    #[test]
    fn drain_consumption_deadline_wraps_with_sequence() {
        assert!(
            !playback_sequence_consumed(u64::MAX, 1, 3, false)
                .expect("nearby sequences should be ordered")
        );
        assert!(
            playback_sequence_consumed(u64::MAX, 2, 3, false)
                .expect("nearby sequences should be ordered")
        );
        assert!(
            playback_sequence_consumed(u64::MAX, 0, 0, true)
                .expect("nearby sequences should be ordered")
        );
    }

    #[test]
    fn daemon_disconnect_is_terminal_for_alsa() {
        assert_eq!(client_error_code(ClientError::Closed), libc::ENODEV);
        assert_eq!(client_error_code(ClientError::Timeout), libc::EAGAIN);
    }
}
