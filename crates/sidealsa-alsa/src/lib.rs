use std::{
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
    buffer_out: *mut c_uint,
    poll_fd_out: *mut c_int,
) -> c_int {
    ffi_status(|| unsafe {
        if socket.is_null()
            || stream_out.is_null()
            || rate_out.is_null()
            || channels_out.is_null()
            || period_out.is_null()
            || buffer_out.is_null()
            || poll_fd_out.is_null()
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
        let poll_fd = stream.notification_fd().map_err(client_error_code)?;
        let buffer_size = plugin_buffer_size(device_info.period_size, device_info.buffer_size);
        let buffer_frames = usize::try_from(buffer_size).map_err(|_| libc::EINVAL)?;
        let fifo_len = buffer_frames.checked_mul(channels).ok_or(libc::EINVAL)?;
        let playback_latency_periods = if playback && mode == MODE_SHARED {
            u64::from(device_info.shared_latency_periods)
        } else {
            0
        };
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
            start_sequence: None,
            position: 0,
            running: false,
        });
        *stream_out = Box::into_raw(stream);
        *rate_out = device_info.rate;
        *channels_out = u32::try_from(channels).map_err(|_| libc::EINVAL)?;
        *period_out = device_info.period_size;
        *buffer_out = buffer_size;
        *poll_fd_out = poll_fd;
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
        stream.stream.start().map_err(client_error_code)?;
        if !stream.running {
            stream.start_sequence = Some(stream.stream.cycle_sequence());
            stream.position = 0;
            stream.running = true;
            stream.flush_partial_playback()?;
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
/// # Safety
///
/// `stream` must be a live handle returned by `sidealsa_stream_open`.
pub unsafe extern "C" fn sidealsa_stream_stop(stream: *mut SideAlsaStream) -> c_int {
    ffi_status(|| unsafe {
        let stream = stream.as_mut().ok_or(libc::EINVAL)?;
        stream.stream.stop().map_err(client_error_code)?;
        stream.reset_transfer_state();
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
    fn position(&self) -> u64 {
        if let Some(origin) = self.start_sequence {
            self.stream
                .cycle_sequence()
                .saturating_sub(origin)
                .checked_mul(self.period_frames as u64)
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
            let start = self
                .playback_fifo_frames
                .checked_mul(self.channels)
                .ok_or(libc::EOVERFLOW)?;
            let end = self
                .period_frames
                .checked_mul(self.channels)
                .ok_or(libc::EOVERFLOW)?;
            self.playback_fifo[start..end].fill(0);
            self.playback_fifo_frames = self.period_frames;
        }
        let _ = self.flush_playback_blocks()?;
        Ok(())
    }

    fn flush_playback_blocks(&mut self) -> Result<bool, c_int> {
        let mut progressed = false;
        while self.playback_fifo_frames >= self.period_frames {
            let observed = match (self.playback_cycle_sequence, self.next_playback_sequence) {
                (Some(observed), Some(next)) if next <= observed.saturating_add(1) => observed,
                _ => match self.wait_period() {
                    Ok(sequence) => {
                        if self
                            .playback_cycle_sequence
                            .is_some_and(|previous| sequence < previous)
                        {
                            self.next_playback_sequence = None;
                            self.playback_fifo_frames = 0;
                            return Ok(progressed);
                        }
                        self.playback_cycle_sequence = Some(sequence);
                        sequence
                    }
                    Err(libc::EAGAIN) if self.nonblock => return Ok(progressed),
                    Err(error) => return Err(error),
                },
            };
            let (sequence, expired) = plan_playback_sequence(
                self.next_playback_sequence,
                observed,
                self.playback_latency_periods,
                self.pro,
            );
            if expired > 0 {
                self.discard_playback_periods(expired)?;
                self.next_playback_sequence = Some(sequence);
                if self.playback_fifo_frames < self.period_frames {
                    return Ok(progressed);
                }
            }
            self.next_playback_sequence = Some(sequence);
            if sequence > observed.saturating_add(1) {
                self.playback_cycle_sequence = None;
                continue;
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
                return Ok(progressed);
            }
            self.discard_playback_periods(1)?;
            self.next_playback_sequence = Some(sequence.saturating_add(1));
            self.update_position(sequence)?;
            progressed = true;
        }
        Ok(progressed)
    }

    fn discard_playback_periods(&mut self, periods: usize) -> Result<(), c_int> {
        let frames = periods
            .saturating_mul(self.period_frames)
            .min(self.playback_fifo_frames);
        let samples = frames.checked_mul(self.channels).ok_or(libc::EOVERFLOW)?;
        let remaining_frames = self.playback_fifo_frames - frames;
        let remaining_samples = remaining_frames
            .checked_mul(self.channels)
            .ok_or(libc::EOVERFLOW)?;
        self.playback_fifo
            .copy_within(samples..samples + remaining_samples, 0);
        self.playback_fifo_frames = remaining_frames;
        Ok(())
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
                        self.start_sequence = Some(sequence);
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
        let origin = self.start_sequence.unwrap_or(sequence);
        self.position = sequence
            .saturating_sub(origin)
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

fn plan_playback_sequence(
    next: Option<u64>,
    observed: u64,
    latency: u64,
    same_cycle: bool,
) -> (u64, usize) {
    let next = next.unwrap_or_else(|| {
        if same_cycle {
            observed
        } else {
            observed.saturating_add(1)
        }
    });
    let earliest = if same_cycle {
        observed
    } else {
        observed
            .checked_sub(latency)
            .map_or(0, |sequence| sequence.saturating_add(1))
    };
    let sequence = next.max(earliest);
    let expired = usize::try_from(sequence - next).unwrap_or(usize::MAX);
    (sequence, expired)
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
    buffer_size.max(period_size.saturating_mul(2))
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
        ClientError::Closed | ClientError::NotStarted => libc::EPIPE,
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

    #[test]
    fn ioplug_buffer_has_at_least_two_periods() {
        assert_eq!(plugin_buffer_size(32, 64), 64);
        assert_eq!(plugin_buffer_size(64, 64), 128);
        assert_eq!(plugin_buffer_size(64, 256), 256);
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
    fn coalesced_cycles_keep_viable_sequences_and_drop_only_expired_blocks() {
        assert_eq!(plan_playback_sequence(None, 100, 3, false), (101, 0));
        assert_eq!(plan_playback_sequence(Some(101), 102, 3, false), (101, 0));
        assert_eq!(plan_playback_sequence(Some(101), 104, 3, false), (102, 1));
        assert_eq!(plan_playback_sequence(Some(101), 106, 3, false), (104, 3));
    }

    #[test]
    fn pro_submits_the_observed_same_cycle_sequence() {
        assert_eq!(plan_playback_sequence(None, 100, 0, true), (100, 0));
        assert_eq!(plan_playback_sequence(Some(101), 101, 0, true), (101, 0));
        assert_eq!(plan_playback_sequence(Some(101), 103, 0, true), (103, 2));
    }
}
