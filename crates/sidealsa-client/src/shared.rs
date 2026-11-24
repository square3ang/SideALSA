use std::{
    ffi::CString,
    io,
    mem::size_of,
    os::fd::RawFd,
    ptr::{self, NonNull},
    slice,
    sync::atomic::{AtomicUsize, Ordering, fence},
};

use sidealsa_protocol::{
    SHARED_ACTIVATION_CLAIMED, SHARED_ACTIVATION_PENDING, SHARED_ACTIVATION_READY,
    SHARED_CLIENT_IDLE, SHARED_MAGIC, SHARED_SLOT_FREE, SHARED_SLOT_READING, SHARED_SLOT_READY,
    SHARED_SLOT_WRITING, SHARED_VERSION, SharedRegionHeader, SharedRegionInfo, SharedRegionLayout,
    SharedSlotHeader, sequence_is_after, sequence_is_before,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SharedError {
    #[error("shared memory I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("shared memory protocol error: {0}")]
    Protocol(#[from] sidealsa_protocol::ProtocolError),
    #[error("shared memory mapping failed")]
    Map,
    #[error("shared memory header is invalid")]
    InvalidHeader,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackConsume {
    Ready,
    Late,
    Missing,
}

pub struct SharedRegion {
    fd: RawFd,
    ptr: NonNull<u8>,
    layout: SharedRegionLayout,
    capture_samples: usize,
    playback_samples: usize,
    slot_count: usize,
    slot_stride: usize,
    capture_base: NonNull<u8>,
    playback_base: NonNull<u8>,
    playback_hint: AtomicUsize,
}

// SharedRegion owns one mmap. Slot state release/acquire operations protect audio access.
unsafe impl Send for SharedRegion {}
unsafe impl Sync for SharedRegion {}

impl SharedRegion {
    pub fn create(
        period_frames: u32,
        playback_channels: u32,
        capture_channels: u32,
    ) -> Result<Self, SharedError> {
        Self::create_with_slot_count(
            period_frames,
            playback_channels,
            capture_channels,
            sidealsa_protocol::SHARED_SLOT_COUNT,
        )
    }

    pub fn create_with_slot_count(
        period_frames: u32,
        playback_channels: u32,
        capture_channels: u32,
        slot_count: u32,
    ) -> Result<Self, SharedError> {
        let layout = SharedRegionLayout::new(
            period_frames,
            playback_channels,
            capture_channels,
            slot_count,
        )?;
        let name = CString::new("sidealsa-pro").expect("static name has no nul");
        let fd = unsafe {
            libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if let Err(error) = set_size(fd, layout.size()) {
            unsafe { libc::close(fd) };
            return Err(error.into());
        }
        let region = match Self::map(fd, layout) {
            Ok(region) => region,
            Err(error) => {
                unsafe { libc::close(fd) };
                return Err(error);
            }
        };
        region.initialize();
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        if unsafe { libc::fcntl(region.fd, libc::F_ADD_SEALS, seals) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(region)
    }

    pub fn map_fd(fd: RawFd, info: SharedRegionInfo) -> Result<Self, SharedError> {
        let layout = match SharedRegionLayout::new(
            info.period_frames,
            info.playback_channels,
            info.capture_channels,
            info.slot_count,
        ) {
            Ok(layout) => layout,
            Err(error) => {
                close_fd(fd);
                return Err(error.into());
            }
        };
        if layout.info() != info {
            close_fd(fd);
            return Err(SharedError::InvalidHeader);
        }
        // A short or still-shrinkable file would SIGBUS on access instead of failing here.
        if let Err(error) = check_sealed_size(fd, layout.size()) {
            close_fd(fd);
            return Err(error);
        }
        let region = match Self::map(fd, layout) {
            Ok(region) => region,
            Err(error) => {
                close_fd(fd);
                return Err(error);
            }
        };
        let header = unsafe { &*(region.ptr.as_ptr() as *const SharedRegionHeader) };
        if header.magic != SHARED_MAGIC
            || header.version != SHARED_VERSION
            || header.header_size != size_of::<SharedRegionHeader>() as u16
            || header.total_size != info.size
            || header.period_frames != info.period_frames
            || header.playback_channels != info.playback_channels
            || header.capture_channels != info.capture_channels
            || header.slot_count != info.slot_count
            || header.slot_stride != info.slot_stride
            || header.capture_offset != info.capture_offset
            || header.playback_offset != info.playback_offset
        {
            drop(region);
            return Err(SharedError::InvalidHeader);
        }
        Ok(region)
    }

    fn map(fd: RawFd, layout: SharedRegionLayout) -> Result<Self, SharedError> {
        let info = layout.info();
        let capture_samples = sample_count(info.period_frames, info.capture_channels)?;
        let playback_samples = sample_count(info.period_frames, info.playback_channels)?;
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                layout.size(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(SharedError::Map);
        }
        let ptr = NonNull::new(ptr.cast::<u8>()).ok_or(SharedError::Map)?;
        if unsafe { libc::mlock(ptr.as_ptr().cast(), layout.size()) } != 0 {
            let error = io::Error::last_os_error();
            unsafe {
                libc::munmap(ptr.as_ptr().cast(), layout.size());
            }
            return Err(error.into());
        }
        let slot_count = layout.slot_count();
        let slot_stride = layout.slot_stride();
        let capture_base =
            unsafe { NonNull::new_unchecked(ptr.as_ptr().add(layout.capture_offset())) };
        let playback_base =
            unsafe { NonNull::new_unchecked(ptr.as_ptr().add(layout.playback_offset())) };
        Ok(Self {
            fd,
            ptr,
            layout,
            capture_samples,
            playback_samples,
            slot_count,
            slot_stride,
            capture_base,
            playback_base,
            playback_hint: AtomicUsize::new(0),
        })
    }

    fn initialize(&self) {
        let header = SharedRegionHeader::new(&self.layout);
        unsafe {
            ptr::write(self.ptr.as_ptr() as *mut SharedRegionHeader, header);
            for index in 0..self.slot_count {
                ptr::write(
                    self.slot_ptr_from_base(self.capture_base, index),
                    SharedSlotHeader::new(),
                );
                ptr::write(
                    self.slot_ptr_from_base(self.playback_base, index),
                    SharedSlotHeader::new(),
                );
            }
        }
    }

    #[inline]
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    #[inline]
    pub fn info(&self) -> SharedRegionInfo {
        self.layout.info()
    }

    #[inline]
    pub fn set_cycle_sequence(&self, sequence: u64) {
        self.header()
            .cycle_sequence
            .store(sequence, Ordering::Release);
    }

    #[inline]
    pub fn cycle_sequence(&self) -> u64 {
        self.header().cycle_sequence.load(Ordering::Acquire)
    }

    #[inline]
    pub fn set_playback_sequence(&self, sequence: u64) {
        self.header()
            .playback_sequence
            .store(sequence, Ordering::Release);
    }

    #[inline]
    pub fn playback_sequence(&self) -> u64 {
        self.header().playback_sequence.load(Ordering::Acquire)
    }

    #[inline]
    pub fn client_expired_capture_blocks(&self) -> u64 {
        self.header()
            .client_expired_capture_blocks
            .load(Ordering::Relaxed)
    }

    #[inline]
    pub fn record_client_expired_capture_block(&self) {
        self.header()
            .client_expired_capture_blocks
            .fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn client_expired_playback_periods(&self) -> u64 {
        self.header()
            .client_expired_playback_periods
            .load(Ordering::Relaxed)
    }

    #[inline]
    pub fn record_client_expired_playback_periods(&self, periods: u64) {
        self.header()
            .client_expired_playback_periods
            .fetch_add(periods, Ordering::Relaxed);
    }

    #[inline]
    pub fn client_playback_submit_failures(&self) -> u64 {
        self.header()
            .client_playback_submit_failures
            .load(Ordering::Relaxed)
    }

    #[inline]
    pub fn record_client_playback_xrun(&self) {
        self.header()
            .client_playback_xruns
            .fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn client_playback_xruns(&self) -> u64 {
        self.header().client_playback_xruns.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn client_playback_sequence(&self) -> u64 {
        self.header()
            .client_playback_sequence
            .load(Ordering::Acquire)
    }

    #[inline]
    pub fn record_client_realtime_failure(&self) {
        self.header()
            .client_realtime_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn client_realtime_failures(&self) -> u64 {
        self.header()
            .client_realtime_failures
            .load(Ordering::Relaxed)
    }

    #[inline]
    pub fn record_client_callback_timing(&self, duration_nanos: u64, period_nanos: u64) {
        let header = self.header();
        if duration_nanos > header.client_callback_max_nanos.load(Ordering::Relaxed) {
            header
                .client_callback_max_nanos
                .fetch_max(duration_nanos, Ordering::Relaxed);
        }
        if duration_nanos > period_nanos {
            header
                .client_callback_overruns
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn client_callback_overruns(&self) -> u64 {
        self.header()
            .client_callback_overruns
            .load(Ordering::Relaxed)
    }

    #[inline]
    pub fn client_callback_max_nanos(&self) -> u64 {
        self.header()
            .client_callback_max_nanos
            .load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_lifecycle_generation(&self, generation: u64) {
        self.header()
            .lifecycle_generation
            .store(generation, Ordering::Release);
    }

    #[inline]
    pub fn lifecycle_generation(&self) -> u64 {
        self.header().lifecycle_generation.load(Ordering::Acquire)
    }

    #[inline]
    pub fn set_hardware_generation(&self, generation: u64) {
        self.header()
            .hardware_generation
            .store(generation, Ordering::Release);
    }

    #[inline]
    pub fn hardware_generation(&self) -> u64 {
        self.header().hardware_generation.load(Ordering::Acquire)
    }

    #[inline]
    pub fn set_client_state(&self, state: u32) {
        self.header().client_state.store(state, Ordering::Release);
    }

    #[inline]
    pub fn client_state(&self) -> u32 {
        self.header().client_state.load(Ordering::Acquire)
    }

    #[inline]
    pub fn set_start_sequence(&self, sequence: u64) {
        self.header()
            .start_sequence
            .store(sequence, Ordering::Release);
    }

    #[inline]
    pub fn start_sequence(&self) -> u64 {
        self.header().start_sequence.load(Ordering::Acquire)
    }

    pub fn reset_activation(&self) {
        let pending = activation_token(self.lifecycle_generation(), SHARED_ACTIVATION_PENDING);
        // The peer can write this word, so a stuck CLAIMED value must not hang the caller.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);
        loop {
            let current = self.header().activation_state.load(Ordering::Acquire);
            if current & 3 == SHARED_ACTIVATION_CLAIMED {
                if std::time::Instant::now() >= deadline {
                    self.header()
                        .activation_state
                        .store(pending, Ordering::Release);
                    return;
                }
                std::thread::yield_now();
                continue;
            }
            if self
                .header()
                .activation_state
                .compare_exchange(current, pending, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    pub fn establish_activation(&self, sequence: u64) -> bool {
        let generation = self.lifecycle_generation();
        let pending = activation_token(generation, SHARED_ACTIVATION_PENDING);
        let claimed = activation_token(generation, SHARED_ACTIVATION_CLAIMED);
        let ready = activation_token(generation, SHARED_ACTIVATION_READY);
        let Err(current) = self.header().activation_state.compare_exchange(
            pending,
            claimed,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) else {
            self.set_start_sequence(sequence);
            self.header()
                .activation_state
                .store(ready, Ordering::Release);
            return true;
        };
        current != ready
    }

    #[inline]
    pub fn activation_ready(&self) -> bool {
        self.header().activation_state.load(Ordering::Acquire)
            == activation_token(self.lifecycle_generation(), SHARED_ACTIVATION_READY)
    }

    #[inline]
    pub fn record_capture_discontinuity(&self) {
        self.header()
            .capture_discontinuities
            .fetch_add(1, Ordering::Release);
    }

    #[inline]
    pub fn capture_discontinuities(&self) -> u64 {
        self.header()
            .capture_discontinuities
            .load(Ordering::Acquire)
    }

    pub fn reset_slots(&self) {
        self.set_client_state(SHARED_CLIENT_IDLE);
        self.playback_hint.store(0, Ordering::Relaxed);
        for ring_base in [self.capture_base, self.playback_base] {
            for index in 0..self.slot_count {
                let slot = unsafe { &*self.slot_ptr_from_base(ring_base, index) };
                slot.sequence.store(0, Ordering::Relaxed);
                slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
            }
        }
    }

    pub fn try_publish_capture(
        &self,
        producer_index: &mut usize,
        sequence: u64,
        samples: &[i32],
    ) -> bool {
        if self.capture_samples == 0 {
            return false;
        }
        self.try_publish(
            self.capture_base,
            self.capture_samples,
            producer_index,
            sequence,
            samples,
            false,
        )
    }

    pub fn try_client_publish_playback(
        &self,
        producer_index: &mut usize,
        sequence: u64,
        samples: &[i32],
    ) -> bool {
        if self.playback_samples == 0 {
            return false;
        }
        let published = self.try_publish(
            self.playback_base,
            self.playback_samples,
            producer_index,
            sequence,
            samples,
            true,
        );
        if !published {
            self.header()
                .client_playback_submit_failures
                .fetch_add(1, Ordering::Relaxed);
        }
        published
    }

    fn try_publish(
        &self,
        ring_base: NonNull<u8>,
        sample_count: usize,
        producer_index: &mut usize,
        sequence: u64,
        samples: &[i32],
        timestamp_publication: bool,
    ) -> bool {
        if sample_count == 0 || samples.len() < sample_count {
            return false;
        }
        let slot_count = self.slot_count;
        let mut index = normalize_index(*producer_index, slot_count);
        for _ in 0..slot_count {
            let slot_ptr = unsafe { self.slot_ptr_from_base(ring_base, index) };
            let slot = unsafe { &*slot_ptr };
            if slot.state.load(Ordering::Relaxed) != SHARED_SLOT_FREE
                || slot
                    .state
                    .compare_exchange(
                        SHARED_SLOT_FREE,
                        SHARED_SLOT_WRITING,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_err()
            {
                index = next_index(index, slot_count);
                continue;
            }
            unsafe {
                Self::slot_audio_mut_from_ptr(slot_ptr, sample_count)
                    .copy_from_slice(&samples[..sample_count]);
            }
            slot.sequence.store(sequence, Ordering::Relaxed);
            if timestamp_publication {
                let Some(published_nanos) = monotonic_nanos() else {
                    slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
                    return false;
                };
                slot.published_nanos
                    .store(published_nanos, Ordering::Relaxed);
                let header = self.header();
                header
                    .client_playback_sequence
                    .store(sequence, Ordering::Release);
                // Avoid dirtying the daemon-written header cache line once RUNNING.
                if header.client_state.load(Ordering::Relaxed)
                    != sidealsa_protocol::SHARED_CLIENT_RUNNING
                {
                    header
                        .client_state
                        .store(sidealsa_protocol::SHARED_CLIENT_RUNNING, Ordering::Release);
                }
            }
            slot.state.store(SHARED_SLOT_READY, Ordering::Release);
            *producer_index = next_index(index, slot_count);
            return true;
        }
        false
    }

    pub fn try_consume_playback(&self, sequence: u64, samples: &mut [i32]) -> bool {
        self.try_consume_exact(
            self.playback_base,
            self.playback_samples,
            sequence,
            None,
            samples,
        )
        .0 == PlaybackConsume::Ready
    }

    #[inline]
    pub fn has_ready_playback(&self, sequence: u64) -> bool {
        if self.playback_samples == 0 {
            return false;
        }
        let slot_count = self.slot_count;
        let hint = normalize_index(self.playback_hint.load(Ordering::Relaxed), slot_count);
        let mut index = hint;
        for _ in 0..slot_count {
            let slot = unsafe { &*self.slot_ptr_from_base(self.playback_base, index) };
            if slot.state.load(Ordering::Relaxed) == SHARED_SLOT_READY {
                fence(Ordering::Acquire);
                if slot.sequence.load(Ordering::Relaxed) == sequence {
                    if index != hint {
                        self.playback_hint.store(index, Ordering::Relaxed);
                    }
                    return true;
                }
            }
            index = next_index(index, slot_count);
        }
        false
    }

    pub fn try_consume_playback_before(
        &self,
        sequence: u64,
        cutoff_nanos: u64,
        samples: &mut [i32],
    ) -> PlaybackConsume {
        self.try_consume_playback_before_with_timestamp(sequence, cutoff_nanos, samples)
            .0
    }

    /// Consumes the exact playback sequence if published at or before the cutoff.
    /// Returns its nonzero publication timestamp for Ready or Late, and None for
    /// Missing. A zero timestamp is Late with None metadata. Late and Missing
    /// leave samples unchanged; Ready and Late both release the matching slot.
    pub fn try_consume_playback_before_with_timestamp(
        &self,
        sequence: u64,
        cutoff_nanos: u64,
        samples: &mut [i32],
    ) -> (PlaybackConsume, Option<u64>) {
        self.try_consume_exact(
            self.playback_base,
            self.playback_samples,
            sequence,
            Some(cutoff_nanos),
            samples,
        )
    }

    fn try_consume_exact(
        &self,
        ring_base: NonNull<u8>,
        sample_count: usize,
        expected_sequence: u64,
        cutoff_nanos: Option<u64>,
        samples: &mut [i32],
    ) -> (PlaybackConsume, Option<u64>) {
        if sample_count == 0 || samples.len() < sample_count {
            return (PlaybackConsume::Missing, None);
        }
        let slot_count = self.slot_count;
        let stride = self.slot_stride;
        let mut exact: Option<(usize, *mut SharedSlotHeader)> = None;
        let mut raw = ring_base.as_ptr();
        for index in 0..slot_count {
            let slot_ptr = raw.cast::<SharedSlotHeader>();
            let slot = unsafe { &*slot_ptr };
            if slot.state.load(Ordering::Relaxed) == SHARED_SLOT_READY {
                fence(Ordering::Acquire);
                let sequence = slot.sequence.load(Ordering::Relaxed);
                if sequence == expected_sequence {
                    if exact.is_none() {
                        exact = Some((index, slot_ptr));
                    } else {
                        reclaim_ready_slot(slot);
                    }
                } else if !sequence_is_after(sequence, expected_sequence) {
                    reclaim_ready_slot(slot);
                }
            }
            raw = unsafe { raw.add(stride) };
        }
        let Some((index, slot_ptr)) = exact else {
            return (PlaybackConsume::Missing, None);
        };
        let slot = unsafe { &*slot_ptr };
        if slot
            .state
            .compare_exchange(
                SHARED_SLOT_READY,
                SHARED_SLOT_READING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return (PlaybackConsume::Missing, None);
        }
        if slot.sequence.load(Ordering::Relaxed) != expected_sequence {
            slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
            return (PlaybackConsume::Missing, None);
        }
        let timestamp = if let Some(cutoff) = cutoff_nanos {
            let published = slot.published_nanos.load(Ordering::Relaxed);
            let timestamp = (published != 0).then_some(published);
            if published == 0 || published > cutoff {
                slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
                self.playback_hint
                    .store(next_index(index, slot_count), Ordering::Relaxed);
                return (PlaybackConsume::Late, timestamp);
            }
            timestamp
        } else {
            None
        };
        unsafe {
            samples[..sample_count]
                .copy_from_slice(Self::slot_audio_from_ptr(slot_ptr, sample_count));
        }
        slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
        self.playback_hint
            .store(next_index(index, slot_count), Ordering::Relaxed);
        (PlaybackConsume::Ready, timestamp)
    }

    pub fn try_client_read_capture(
        &self,
        consumer_index: &mut usize,
        samples: &mut [i32],
    ) -> Option<u64> {
        self.try_client_read_capture_with_remaining(consumer_index, samples)
            .0
    }

    pub(crate) fn try_client_read_capture_with_remaining(
        &self,
        consumer_index: &mut usize,
        samples: &mut [i32],
    ) -> (Option<u64>, bool) {
        if self.capture_samples == 0 {
            return (None, false);
        }
        if samples.len() < self.capture_samples {
            return (None, self.has_ready_capture_since(*consumer_index, None));
        }
        let Some((index, slot_ptr, sequence, has_other_ready)) = self.next_ready_capture_slot()
        else {
            return (None, false);
        };
        let slot = unsafe { &*slot_ptr };
        if slot
            .state
            .compare_exchange(
                SHARED_SLOT_READY,
                SHARED_SLOT_READING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return (None, self.has_ready_capture_since(*consumer_index, None));
        }
        if slot.sequence.load(Ordering::Relaxed) != sequence {
            slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
            return (None, self.has_ready_capture_since(*consumer_index, None));
        }
        unsafe {
            samples[..self.capture_samples]
                .copy_from_slice(Self::slot_audio_from_ptr(slot_ptr, self.capture_samples));
        }
        slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
        *consumer_index = next_index(index, self.slot_count);
        (Some(sequence), has_other_ready)
    }

    pub fn next_client_capture_sequence(&self, consumer_index: usize) -> Option<u64> {
        self.next_client_capture_sequence_with_remaining(consumer_index)
            .0
    }

    pub(crate) fn next_client_capture_sequence_with_remaining(
        &self,
        _consumer_index: usize,
    ) -> (Option<u64>, bool) {
        match self.next_ready_capture_slot() {
            Some((_, _, sequence, has_more)) => (Some(sequence), has_more),
            None => (None, false),
        }
    }

    fn next_ready_capture_slot(&self) -> Option<(usize, *mut SharedSlotHeader, u64, bool)> {
        if self.capture_samples == 0 {
            return None;
        }
        let stride = self.slot_stride;
        let mut raw = self.capture_base.as_ptr();
        let mut selected: Option<(usize, *mut SharedSlotHeader, u64)> = None;
        let mut ready_count = 0_usize;
        for index in 0..self.slot_count {
            let slot_ptr = raw.cast::<SharedSlotHeader>();
            let slot = unsafe { &*slot_ptr };
            if slot.state.load(Ordering::Relaxed) == SHARED_SLOT_READY {
                fence(Ordering::Acquire);
                ready_count += 1;
                let sequence = slot.sequence.load(Ordering::Relaxed);
                if selected.is_none_or(|(_, _, current)| sequence_is_before(sequence, current)) {
                    selected = Some((index, slot_ptr, sequence));
                }
            }
            raw = unsafe { raw.add(stride) };
        }
        selected.map(|(index, slot_ptr, sequence)| (index, slot_ptr, sequence, ready_count > 1))
    }

    #[inline]
    pub fn has_multiple_ready_capture_slots(&self) -> bool {
        if self.capture_samples == 0 {
            return false;
        }
        let stride = self.slot_stride;
        let mut raw = self.capture_base.as_ptr();
        let mut ready = 0_usize;
        for _ in 0..self.slot_count {
            let slot = unsafe { &*raw.cast::<SharedSlotHeader>() };
            if slot.state.load(Ordering::Relaxed) == SHARED_SLOT_READY {
                ready += 1;
                if ready > 1 {
                    return true;
                }
            }
            raw = unsafe { raw.add(stride) };
        }
        false
    }

    #[inline]
    pub fn ready_capture_slots(&self) -> usize {
        if self.capture_samples == 0 {
            return 0;
        }
        let stride = self.slot_stride;
        let mut raw = self.capture_base.as_ptr();
        let mut ready = 0;
        for _ in 0..self.slot_count {
            let slot = unsafe { &*raw.cast::<SharedSlotHeader>() };
            if slot.state.load(Ordering::Relaxed) == SHARED_SLOT_READY {
                ready += 1;
            }
            raw = unsafe { raw.add(stride) };
        }
        ready
    }

    pub fn oldest_valid_client_capture_sequence(&self, minimum_sequence: u64) -> Option<u64> {
        self.oldest_valid_client_capture_sequence_with_remaining(minimum_sequence)
            .0
    }

    pub(crate) fn oldest_valid_client_capture_sequence_with_remaining(
        &self,
        minimum_sequence: u64,
    ) -> (Option<u64>, bool) {
        if self.capture_samples == 0 {
            return (None, false);
        }
        let stride = self.slot_stride;
        let mut raw = self.capture_base.as_ptr();
        let mut selected = None;
        let mut valid_count = 0_usize;
        let mut expired = 0_u64;
        for _ in 0..self.slot_count {
            let slot = unsafe { &*raw.cast::<SharedSlotHeader>() };
            if slot.state.load(Ordering::Relaxed) == SHARED_SLOT_READY {
                fence(Ordering::Acquire);
                let sequence = slot.sequence.load(Ordering::Relaxed);
                if sequence_is_before(sequence, minimum_sequence) {
                    reclaim_ready_slot(slot);
                    expired += 1;
                } else {
                    valid_count += 1;
                    let distance = sequence.wrapping_sub(minimum_sequence);
                    if selected.is_none_or(|(_, current_distance)| distance < current_distance) {
                        selected = Some((sequence, distance));
                    }
                }
            }
            raw = unsafe { raw.add(stride) };
        }
        if expired != 0 {
            self.header()
                .client_expired_capture_blocks
                .fetch_add(expired, Ordering::Relaxed);
        }
        (selected.map(|(sequence, _)| sequence), valid_count > 1)
    }

    #[inline]
    pub(crate) fn has_ready_capture_since(
        &self,
        start_index: usize,
        minimum_sequence: Option<u64>,
    ) -> bool {
        if self.capture_samples == 0 {
            return false;
        }
        let slot_count = self.slot_count;
        let mut index = normalize_index(start_index, slot_count);
        for _ in 0..slot_count {
            let slot = unsafe { &*self.slot_ptr_from_base(self.capture_base, index) };
            if slot.state.load(Ordering::Relaxed) == SHARED_SLOT_READY {
                let Some(minimum) = minimum_sequence else {
                    return true;
                };
                fence(Ordering::Acquire);
                if !sequence_is_before(slot.sequence.load(Ordering::Relaxed), minimum) {
                    return true;
                }
            }
            index = next_index(index, slot_count);
        }
        false
    }

    pub fn try_client_read_valid_capture(
        &self,
        consumer_index: &mut usize,
        minimum_sequence: u64,
        samples: &mut [i32],
    ) -> Option<u64> {
        self.try_client_read_valid_capture_with_remaining(consumer_index, minimum_sequence, samples)
            .0
    }

    pub(crate) fn try_client_read_valid_capture_with_remaining(
        &self,
        consumer_index: &mut usize,
        minimum_sequence: u64,
        samples: &mut [i32],
    ) -> (Option<u64>, bool) {
        if self.capture_samples == 0 {
            return (None, false);
        }
        if samples.len() < self.capture_samples {
            return (
                None,
                self.oldest_valid_client_capture_sequence(minimum_sequence)
                    .is_some(),
            );
        }

        let slot_count = self.slot_count;
        let stride = self.slot_stride;
        let mut raw = self.capture_base.as_ptr();
        let mut selected: Option<(usize, *mut SharedSlotHeader, u64, u64)> = None;
        let mut valid_count = 0_usize;
        let mut expired = 0_u64;
        for index in 0..slot_count {
            let slot_ptr = raw.cast::<SharedSlotHeader>();
            let slot = unsafe { &*slot_ptr };
            if slot.state.load(Ordering::Relaxed) == SHARED_SLOT_READY {
                fence(Ordering::Acquire);
                let sequence = slot.sequence.load(Ordering::Relaxed);
                if sequence_is_before(sequence, minimum_sequence) {
                    reclaim_ready_slot(slot);
                    expired += 1;
                } else {
                    valid_count += 1;
                    let distance = sequence.wrapping_sub(minimum_sequence);
                    if selected
                        .is_none_or(|(_, _, _, current_distance)| distance < current_distance)
                    {
                        selected = Some((index, slot_ptr, sequence, distance));
                    }
                }
            }
            raw = unsafe { raw.add(stride) };
        }
        if expired != 0 {
            self.header()
                .client_expired_capture_blocks
                .fetch_add(expired, Ordering::Relaxed);
        }

        let Some((index, slot_ptr, sequence, _)) = selected else {
            return (None, false);
        };
        let slot = unsafe { &*slot_ptr };
        if slot
            .state
            .compare_exchange(
                SHARED_SLOT_READY,
                SHARED_SLOT_READING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return (
                None,
                self.has_ready_capture_since(*consumer_index, Some(minimum_sequence)),
            );
        }
        if slot.sequence.load(Ordering::Relaxed) != sequence {
            slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
            return (
                None,
                self.has_ready_capture_since(*consumer_index, Some(minimum_sequence)),
            );
        }
        unsafe {
            samples[..self.capture_samples]
                .copy_from_slice(Self::slot_audio_from_ptr(slot_ptr, self.capture_samples));
        }
        slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
        *consumer_index = next_index(index, slot_count);
        (Some(sequence), valid_count > 1)
    }

    pub(crate) fn discard_capture_through(&self, through: u64, minimum: u64) {
        if self.capture_samples == 0 {
            return;
        }
        let stride = self.slot_stride;
        let mut raw = self.capture_base.as_ptr();
        let mut expired = 0;
        for _ in 0..self.slot_count {
            let slot = unsafe { &*raw.cast::<SharedSlotHeader>() };
            if slot.state.load(Ordering::Relaxed) == SHARED_SLOT_READY {
                fence(Ordering::Acquire);
                let sequence = slot.sequence.load(Ordering::Relaxed);
                if !sequence_is_after(sequence, through)
                    && slot
                        .state
                        .compare_exchange(
                            SHARED_SLOT_READY,
                            SHARED_SLOT_READING,
                            Ordering::Acquire,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                {
                    let sequence = slot.sequence.load(Ordering::Relaxed);
                    if sequence_is_after(sequence, through) {
                        slot.state.store(SHARED_SLOT_READY, Ordering::Release);
                    } else {
                        expired += u64::from(sequence_is_before(sequence, minimum));
                        slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
                    }
                }
            }
            raw = unsafe { raw.add(stride) };
        }
        if expired != 0 {
            self.header()
                .client_expired_capture_blocks
                .fetch_add(expired, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    unsafe fn slot(&self, ring_offset: usize, index: usize) -> &SharedSlotHeader {
        let offset = self
            .layout
            .slot_offset(ring_offset, index)
            .expect("shared slot index is bounded");
        unsafe { &*self.ptr.as_ptr().add(offset).cast::<SharedSlotHeader>() }
    }

    #[inline]
    fn header(&self) -> &SharedRegionHeader {
        unsafe { &*(self.ptr.as_ptr() as *const SharedRegionHeader) }
    }

    #[inline]
    unsafe fn slot_ptr_from_base(
        &self,
        ring_base: NonNull<u8>,
        index: usize,
    ) -> *mut SharedSlotHeader {
        debug_assert!(index < self.slot_count);
        unsafe {
            ring_base
                .as_ptr()
                .add(index * self.slot_stride)
                .cast::<SharedSlotHeader>()
        }
    }

    #[inline]
    unsafe fn slot_audio_from_ptr<'a>(
        slot_ptr: *const SharedSlotHeader,
        samples: usize,
    ) -> &'a [i32] {
        unsafe { slice::from_raw_parts(slot_ptr.add(1).cast::<i32>(), samples) }
    }

    #[inline]
    unsafe fn slot_audio_mut_from_ptr<'a>(
        slot_ptr: *mut SharedSlotHeader,
        samples: usize,
    ) -> &'a mut [i32] {
        unsafe { slice::from_raw_parts_mut(slot_ptr.add(1).cast::<i32>(), samples) }
    }
}

impl Drop for SharedRegion {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.layout.size());
            libc::close(self.fd);
        }
    }
}

#[inline]
fn normalize_index(index: usize, slot_count: usize) -> usize {
    if index < slot_count {
        index
    } else {
        index % slot_count
    }
}

#[inline]
fn next_index(index: usize, slot_count: usize) -> usize {
    let next = index + 1;
    if next < slot_count { next } else { 0 }
}

#[inline]
fn activation_token(generation: u64, state: u64) -> u64 {
    generation.wrapping_shl(2) | state
}

#[inline]
fn reclaim_ready_slot(slot: &SharedSlotHeader) {
    let _ = slot.state.compare_exchange(
        SHARED_SLOT_READY,
        SHARED_SLOT_FREE,
        Ordering::Release,
        Ordering::Relaxed,
    );
}

fn monotonic_nanos() -> Option<u64> {
    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timestamp) } != 0 {
        return None;
    }
    Some(
        u64::try_from(timestamp.tv_sec)
            .ok()?
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::try_from(timestamp.tv_nsec).ok()?),
    )
}

fn sample_count(frames: u32, channels: u32) -> Result<usize, SharedError> {
    usize::try_from(u64::from(frames) * u64::from(channels))
        .map_err(|_| SharedError::Protocol(sidealsa_protocol::ProtocolError::LayoutOverflow))
}

fn set_size(fd: RawFd, size: usize) -> Result<(), io::Error> {
    let result = unsafe { libc::ftruncate(fd, size as libc::off_t) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn check_sealed_size(fd: RawFd, size: usize) -> Result<(), SharedError> {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    if seals < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if usize::try_from(stat.st_size).map_or(true, |actual| actual < size)
        || seals & libc::F_SEAL_SHRINK == 0
    {
        return Err(SharedError::InvalidHeader);
    }
    Ok(())
}

fn close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn capture_discard_never_touches_a_playback_only_ring() {
        let region = super::SharedRegion::create(1, 1, 0).unwrap();
        let mut index = 0;
        assert!(region.try_client_publish_playback(&mut index, 11, &[17]));
        region.discard_capture_through(11, 12);
        let mut output = [0];
        assert!(region.try_consume_playback(11, &mut output));
        assert_eq!(output, [17]);
    }
    use super::*;

    #[test]
    fn map_fd_rejects_short_or_unsealed_memfd() {
        let info = SharedRegion::create(1, 0, 1).unwrap().info();
        let name = CString::new("sidealsa-test").unwrap();
        for size in [info.size as usize - 1, info.size as usize] {
            let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
            assert!(fd >= 0);
            set_size(fd, size).unwrap();
            assert!(matches!(
                SharedRegion::map_fd(fd, info),
                Err(SharedError::InvalidHeader)
            ));
        }
    }

    #[test]
    fn sixteen_slot_layout_maps_and_reuses_full_capture_reserve() {
        assert_eq!(SharedRegion::create(1, 0, 1).unwrap().info().slot_count, 8);
        let server = SharedRegion::create_with_slot_count(1, 0, 1, 16).unwrap();
        let fd = unsafe { libc::dup(server.fd()) };
        assert!(fd >= 0);
        let client = SharedRegion::map_fd(fd, server.info()).unwrap();
        assert_eq!(client.info().slot_count, 16);
        let mut producer = 0;
        let mut consumer = 0;
        let mut samples = [0];
        for base in [0, 16] {
            for sequence in base..base + 16 {
                assert!(server.try_publish_capture(&mut producer, sequence, &[sequence as i32]));
            }
            assert_eq!(client.ready_capture_slots(), 16);
            assert!(!server.try_publish_capture(&mut producer, base + 16, &[0]));
            for sequence in base..base + 16 {
                assert_eq!(
                    client.try_client_read_capture(&mut consumer, &mut samples),
                    Some(sequence)
                );
                assert_eq!(samples, [sequence as i32]);
            }
            assert_eq!(client.ready_capture_slots(), 0);
        }
    }

    #[test]
    fn mapped_region_exchanges_capture_and_playback_slots() {
        let server = SharedRegion::create(4, 2, 2).expect("region should create");
        let client_fd = unsafe { libc::dup(server.fd()) };
        assert!(client_fd >= 0, "region fd should duplicate");
        let client = SharedRegion::map_fd(client_fd, server.info()).expect("region should map");
        server.set_cycle_sequence(11);
        assert_eq!(client.cycle_sequence(), 11);
        server.set_lifecycle_generation(3);
        assert_eq!(client.lifecycle_generation(), 3);
        server.reset_activation();
        assert!(!client.activation_ready());
        assert!(server.establish_activation(10));
        assert!(client.activation_ready());
        assert_eq!(client.start_sequence(), 10);
        assert!(!server.establish_activation(11));
        assert_eq!(client.start_sequence(), 10);
        server.set_client_state(sidealsa_protocol::SHARED_CLIENT_RUNNING);
        assert_eq!(
            client.client_state(),
            sidealsa_protocol::SHARED_CLIENT_RUNNING
        );

        let capture = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut producer_index = 0;
        assert!(server.try_publish_capture(&mut producer_index, 12, &capture));
        let mut consumer_index = 0;
        let mut received_capture = [0; 8];
        assert_eq!(
            client.try_client_read_capture(&mut consumer_index, &mut received_capture),
            Some(12)
        );
        assert_eq!(received_capture, capture);

        let playback = [8, 7, 6, 5, 4, 3, 2, 1];
        let mut playback_producer = 0;
        assert!(client.try_client_publish_playback(&mut playback_producer, 13, &playback));
        let mut received_playback = [0; 8];
        assert!(server.try_consume_playback(13, &mut received_playback));
        assert_eq!(received_playback, playback);
    }

    #[test]
    fn playback_cutoff_rejects_blocks_published_after_deadline() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        let mut producer_index = 0;
        let mut samples = [0];

        assert!(region.try_client_publish_playback(&mut producer_index, 7, &[7]));
        assert_eq!(
            region.try_consume_playback_before(7, 0, &mut samples),
            PlaybackConsume::Late
        );
        assert!(region.try_client_publish_playback(&mut producer_index, 8, &[8]));
        assert_eq!(
            region.try_consume_playback_before(8, u64::MAX, &mut samples),
            PlaybackConsume::Ready
        );
        assert_eq!(samples, [8]);
    }

    #[test]
    fn playback_cutoff_returns_ready_late_and_missing_metadata() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        let mut producer_index = 0;
        let mut samples = [-1];

        for (sequence, published, cutoff, expected) in [
            (7, 100, 100, PlaybackConsume::Ready),
            (8, 200, 199, PlaybackConsume::Late),
        ] {
            let index = producer_index;
            assert!(region.try_client_publish_playback(
                &mut producer_index,
                sequence,
                &[sequence as i32]
            ));
            let slot = unsafe { region.slot(region.layout.playback_offset(), index) };
            slot.published_nanos.store(published, Ordering::Relaxed);
            assert_eq!(
                region.try_consume_playback_before_with_timestamp(sequence, cutoff, &mut samples),
                (expected, Some(published))
            );
            assert_eq!(samples, [7]);
            assert_eq!(
                region.try_consume_playback_before_with_timestamp(sequence, cutoff, &mut samples),
                (PlaybackConsume::Missing, None)
            );
            assert_eq!(samples, [7]);
        }
    }

    #[test]
    fn playback_zero_timestamp_is_late_without_metadata() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        let mut producer_index = 0;
        let mut samples = [-1];
        for sequence in 7..10 {
            let index = producer_index;
            assert!(region.try_client_publish_playback(&mut producer_index, sequence, &[7]));
            let slot = unsafe { region.slot(region.layout.playback_offset(), index) };
            slot.published_nanos.store(0, Ordering::Relaxed);
            match sequence {
                7 => assert_eq!(
                    region.try_consume_playback_before_with_timestamp(
                        sequence,
                        u64::MAX,
                        &mut samples
                    ),
                    (PlaybackConsume::Late, None)
                ),
                8 => assert_eq!(
                    region.try_consume_playback_before(sequence, u64::MAX, &mut samples),
                    PlaybackConsume::Late
                ),
                _ => assert!(region.try_consume_playback(sequence, &mut samples)),
            }
            assert_eq!(samples, if sequence == 9 { [7] } else { [-1] });
        }
    }

    #[test]
    fn playback_timestamp_survives_later_stale_reclamation_and_reuse() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        let mut producer_index = 0;
        assert!(region.try_client_publish_playback(&mut producer_index, 10, &[10]));
        assert!(region.try_client_publish_playback(&mut producer_index, 9, &[9]));
        let exact = unsafe { region.slot(region.layout.playback_offset(), 0) };
        let stale = unsafe { region.slot(region.layout.playback_offset(), 1) };
        exact.published_nanos.store(100, Ordering::Relaxed);
        stale.published_nanos.store(1, Ordering::Relaxed);
        let mut samples = [0];
        assert_eq!(
            region.try_consume_playback_before_with_timestamp(10, 100, &mut samples),
            (PlaybackConsume::Ready, Some(100))
        );
        assert_eq!(samples, [10]);
        assert_eq!(stale.state.load(Ordering::Acquire), SHARED_SLOT_FREE);

        producer_index = 1;
        assert!(region.try_client_publish_playback(&mut producer_index, 11, &[11]));
        let published = stale.published_nanos.load(Ordering::Relaxed);
        assert!(published > 1);
        assert_eq!(
            region.try_consume_playback_before_with_timestamp(11, published, &mut samples),
            (PlaybackConsume::Ready, Some(published))
        );
        assert_eq!(samples, [11]);
    }

    #[test]
    fn playback_ready_slot_carries_publication_timestamp() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        let mut producer_index = 0;
        let before = monotonic_nanos().expect("monotonic clock should be available");

        assert!(!region.has_ready_playback(7));
        assert!(region.try_client_publish_playback(&mut producer_index, 7, &[7]));
        assert!(region.has_ready_playback(7));
        assert!(!region.has_ready_playback(8));

        let after = monotonic_nanos().expect("monotonic clock should be available");
        let slot = unsafe { region.slot(region.layout.playback_offset(), 0) };
        assert_eq!(slot.state.load(Ordering::Acquire), SHARED_SLOT_READY);
        let published = slot.published_nanos.load(Ordering::Relaxed);
        assert!((before..=after).contains(&published));
    }

    #[test]
    fn shared_memfd_cannot_be_resized_by_client() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        let size = region.info().size as libc::off_t;

        assert_eq!(unsafe { libc::ftruncate(region.fd(), size - 1) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
        assert_eq!(unsafe { libc::ftruncate(region.fd(), size + 1) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
    }

    #[test]
    fn publisher_skips_busy_slot() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        let mut producer_index = 0;
        assert!(region.try_client_publish_playback(&mut producer_index, 10, &[10]));

        producer_index = 0;
        assert!(region.try_client_publish_playback(&mut producer_index, 11, &[11]));
        let mut samples = [0];
        assert!(region.try_consume_playback(10, &mut samples));
        assert_eq!(samples, [10]);
        assert!(region.try_consume_playback(11, &mut samples));
        assert_eq!(samples, [11]);
    }

    #[test]
    fn exact_playback_consume_reclaims_later_stale_slots() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        let mut producer_index = 0;
        assert!(region.try_client_publish_playback(&mut producer_index, 10, &[10]));
        assert!(region.try_client_publish_playback(&mut producer_index, 9, &[9]));

        let mut samples = [0];
        assert!(region.try_consume_playback(10, &mut samples));
        assert_eq!(samples, [10]);
        for sequence in 20..20 + u64::from(sidealsa_protocol::SHARED_SLOT_COUNT) {
            assert!(region.try_client_publish_playback(
                &mut producer_index,
                sequence,
                &[sequence as i32]
            ));
        }
    }

    #[test]
    fn exact_playback_consume_reclaims_stale_slots_across_wrap() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        let mut producer_index = 0;
        assert!(region.try_client_publish_playback(&mut producer_index, 0, &[0]));
        assert!(region.try_client_publish_playback(&mut producer_index, u64::MAX, &[-1]));
        assert!(region.try_client_publish_playback(&mut producer_index, 1, &[1]));

        let mut samples = [0];
        assert!(region.try_consume_playback(0, &mut samples));
        assert_eq!(samples, [0]);
        assert!(region.try_consume_playback(1, &mut samples));
        assert_eq!(samples, [1]);
        for sequence in 20..20 + u64::from(sidealsa_protocol::SHARED_SLOT_COUNT) {
            assert!(region.try_client_publish_playback(
                &mut producer_index,
                sequence,
                &[sequence as i32]
            ));
        }
    }

    #[test]
    fn capture_reader_observes_ready_sequences_in_order() {
        let region = SharedRegion::create(1, 0, 1).expect("region should create");
        let mut producer_index = 0;
        assert!(region.try_publish_capture(&mut producer_index, 10, &[10]));
        assert!(region.try_publish_capture(&mut producer_index, 11, &[11]));

        let mut consumer_index = 0;
        let mut samples = [0];
        assert_eq!(
            region.next_client_capture_sequence(consumer_index),
            Some(10)
        );
        assert_eq!(
            region.try_client_read_capture(&mut consumer_index, &mut samples),
            Some(10)
        );
        assert_eq!(
            region.next_client_capture_sequence(consumer_index),
            Some(11)
        );
        assert_eq!(
            region.try_client_read_capture(&mut consumer_index, &mut samples),
            Some(11)
        );
        assert_eq!(region.next_client_capture_sequence(consumer_index), None);
    }

    #[test]
    fn capture_reader_uses_sequence_order_across_physical_wrap() {
        let region = SharedRegion::create(1, 0, 1).expect("region should create");
        let mut producer_index = sidealsa_protocol::SHARED_SLOT_COUNT as usize - 1;
        assert!(region.try_publish_capture(&mut producer_index, 10, &[10]));
        assert!(region.try_publish_capture(&mut producer_index, 11, &[11]));

        let mut consumer_index = 0;
        let mut samples = [0];
        assert_eq!(
            region.try_client_read_capture(&mut consumer_index, &mut samples),
            Some(10)
        );
        assert_eq!(samples, [10]);
        assert_eq!(
            region.try_client_read_capture(&mut consumer_index, &mut samples),
            Some(11)
        );
        assert_eq!(samples, [11]);
    }

    #[test]
    fn capture_reader_recovers_when_consumer_index_points_to_free_slot() {
        let region = SharedRegion::create(1, 0, 1).expect("region should create");
        let mut producer_index = 0;
        assert!(region.try_publish_capture(&mut producer_index, 10, &[10]));
        assert!(region.try_publish_capture(&mut producer_index, 11, &[11]));

        let mut consumer_index = 0;
        let mut samples = [0];
        assert_eq!(
            region.try_client_read_capture(&mut consumer_index, &mut samples),
            Some(10)
        );
        consumer_index = 0;

        assert_eq!(
            region.next_client_capture_sequence(consumer_index),
            Some(11)
        );
        assert_eq!(
            region.try_client_read_capture(&mut consumer_index, &mut samples),
            Some(11)
        );
        assert_eq!(samples, [11]);
    }

    #[test]
    fn valid_capture_reader_keeps_oldest_unexpired_block() {
        let region = SharedRegion::create(1, 0, 1).expect("region should create");
        let mut producer_index = 0;
        for sequence in 100..=105 {
            assert!(region.try_publish_capture(&mut producer_index, sequence, &[sequence as i32]));
        }

        assert_eq!(region.oldest_valid_client_capture_sequence(103), Some(103));
        let mut consumer_index = 0;
        let mut samples = [0];
        assert_eq!(
            region.try_client_read_valid_capture(&mut consumer_index, 103, &mut samples),
            Some(103)
        );
        assert_eq!(samples, [103]);
        assert_eq!(region.client_expired_capture_blocks(), 3);
        assert_eq!(region.oldest_valid_client_capture_sequence(103), Some(104));
    }

    #[test]
    fn valid_capture_reader_handles_sequence_wrap() {
        let region = SharedRegion::create(1, 0, 1).expect("region should create");
        let mut producer_index = 0;
        for sequence in [u64::MAX - 1, u64::MAX, 0, 1] {
            assert!(region.try_publish_capture(&mut producer_index, sequence, &[sequence as i32]));
        }

        assert_eq!(
            region.oldest_valid_client_capture_sequence(u64::MAX),
            Some(u64::MAX)
        );
        let mut consumer_index = 0;
        let mut samples = [0];
        assert_eq!(
            region.try_client_read_valid_capture(&mut consumer_index, u64::MAX, &mut samples),
            Some(u64::MAX)
        );
        assert_eq!(samples, [-1]);
        assert_eq!(region.client_expired_capture_blocks(), 1);
        assert_eq!(region.oldest_valid_client_capture_sequence(0), Some(0));
    }

    #[test]
    fn reset_slots_keeps_timeline_sequence() {
        let region = SharedRegion::create(1, 1, 1).expect("region should create");
        region.set_cycle_sequence(42);
        region.set_playback_sequence(41);
        region.set_client_state(sidealsa_protocol::SHARED_CLIENT_RUNNING);
        let mut playback_index = 0;
        assert!(region.try_client_publish_playback(&mut playback_index, 7, &[7]));

        region.reset_slots();

        let mut samples = [0];
        assert!(!region.try_consume_playback(7, &mut samples));
        assert_eq!(region.cycle_sequence(), 42);
        assert_eq!(region.playback_sequence(), 41);
        assert_eq!(region.client_state(), sidealsa_protocol::SHARED_CLIENT_IDLE);
    }

    #[test]
    fn client_diagnostics_accumulate_without_resetting() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        region.record_client_realtime_failure();
        region.record_client_callback_timing(500, 1_000);
        region.record_client_callback_timing(1_500, 1_000);
        region.record_client_expired_playback_periods(3);
        region.record_client_playback_xrun();
        region.record_capture_discontinuity();

        let mut producer_index = 0;
        for sequence in 0..u64::from(sidealsa_protocol::SHARED_SLOT_COUNT) {
            assert!(region.try_client_publish_playback(
                &mut producer_index,
                sequence,
                &[sequence as i32]
            ));
        }
        assert!(!region.try_client_publish_playback(&mut producer_index, 99, &[99]));
        region.reset_slots();

        assert_eq!(region.client_realtime_failures(), 1);
        assert_eq!(region.client_callback_overruns(), 1);
        assert_eq!(region.client_callback_max_nanos(), 1_500);
        assert_eq!(region.capture_discontinuities(), 1);
        assert_eq!(region.client_expired_playback_periods(), 3);
        assert_eq!(region.client_playback_submit_failures(), 1);
        assert_eq!(region.client_playback_xruns(), 1);
        assert_eq!(region.client_playback_sequence(), 7);
    }

    #[test]
    fn playback_sequence_tracker_follows_wrapping_publications() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        let mut producer_index = 0;
        let mut samples = [0];

        assert!(region.try_client_publish_playback(&mut producer_index, u64::MAX, &[1]));
        assert_eq!(region.client_playback_sequence(), u64::MAX);
        assert!(region.try_consume_playback(u64::MAX, &mut samples));
        assert!(region.try_client_publish_playback(&mut producer_index, 0, &[2]));
        assert_eq!(region.client_playback_sequence(), 0);
    }

    #[test]
    fn playback_hint_handles_sequential_out_of_order_late_and_reset() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        let mut producer_index = 0;
        let mut samples = [0];

        // Sequential publish/consume across multiple full ring wraps.
        for sequence in 0..(u64::from(sidealsa_protocol::SHARED_SLOT_COUNT) * 3) {
            assert!(region.try_client_publish_playback(
                &mut producer_index,
                sequence,
                &[sequence as i32],
            ));
            assert!(region.has_ready_playback(sequence));
            assert!(!region.has_ready_playback(sequence + 1));
            assert!(region.try_consume_playback(sequence, &mut samples));
            assert_eq!(samples, [sequence as i32]);
            assert!(!region.has_ready_playback(sequence));
        }

        // Out-of-order slot placement and late cutoff still advance hint and reclaim.
        producer_index = sidealsa_protocol::SHARED_SLOT_COUNT as usize - 1;
        assert!(region.try_client_publish_playback(&mut producer_index, 100, &[100]));
        assert!(region.has_ready_playback(100));
        assert_eq!(
            region.try_consume_playback_before(100, 0, &mut samples),
            PlaybackConsume::Late
        );
        assert!(!region.has_ready_playback(100));

        // Reset restores hint and client_state transition on next publish.
        region.reset_slots();
        assert_eq!(region.client_state(), sidealsa_protocol::SHARED_CLIENT_IDLE);
        assert!(region.try_client_publish_playback(&mut producer_index, 101, &[101]));
        assert_eq!(
            region.client_state(),
            sidealsa_protocol::SHARED_CLIENT_RUNNING
        );
        assert!(region.has_ready_playback(101));
    }

    #[test]
    fn callback_timing_keeps_monotonic_maximum_without_regressing() {
        let region = SharedRegion::create(1, 1, 0).expect("region should create");
        region.record_client_callback_timing(800, 1_000);
        region.record_client_callback_timing(300, 1_000);
        region.record_client_callback_timing(1_200, 1_000);
        region.record_client_callback_timing(900, 1_000);
        assert_eq!(region.client_callback_max_nanos(), 1_200);
        assert_eq!(region.client_callback_overruns(), 1);
    }

    #[test]
    fn discard_capture_through_preserves_future_and_counts_only_expired() {
        let region = SharedRegion::create(1, 0, 1).expect("region should create");
        let mut producer_index = 0;
        for sequence in [8_u64, 9, 10, 11] {
            assert!(region.try_publish_capture(&mut producer_index, sequence, &[sequence as i32]));
        }

        region.discard_capture_through(10, 10);
        assert_eq!(region.client_expired_capture_blocks(), 2);
        assert_eq!(region.ready_capture_slots(), 1);
        assert!(!region.has_multiple_ready_capture_slots());

        let mut consumer_index = 0;
        let mut samples = [0];
        assert_eq!(
            region.try_client_read_capture(&mut consumer_index, &mut samples),
            Some(11)
        );
        assert_eq!(samples, [11]);
    }

    #[test]
    fn short_capture_slice_preserves_remaining_flag() {
        let region = SharedRegion::create(2, 0, 2).expect("region should create");
        let mut producer_index = 0;
        assert!(region.try_publish_capture(&mut producer_index, 5, &[1, 2, 3, 4]));
        let mut consumer_index = 0;
        let mut short = [0; 2];
        assert_eq!(
            region.try_client_read_capture_with_remaining(&mut consumer_index, &mut short),
            (None, true)
        );
        assert_eq!(
            region.try_client_read_valid_capture_with_remaining(&mut consumer_index, 5, &mut short),
            (None, true)
        );
        assert_eq!(region.ready_capture_slots(), 1);
    }

    #[test]
    #[ignore = "release-mode shared ring hot-path microbenchmark; no audio hardware"]
    fn benchmark_shared_ring_hot_paths() {
        const ITERATIONS: u64 = 100_000;
        let region = SharedRegion::create(64, 8, 10).expect("region should create");
        let playback_src = [17_i32; 512];
        let mut playback_dst = [0_i32; 512];
        let capture_src = [42_i32; 640];
        let mut capture_dst = [0_i32; 640];
        let mut pb_prod = 0;
        let mut cap_prod = 0;
        let mut cap_cons = 0;

        let started = std::time::Instant::now();
        for sequence in 0..ITERATIONS {
            assert!(region.try_client_publish_playback(&mut pb_prod, sequence, &playback_src));
            assert!(region.has_ready_playback(sequence));
            let (outcome, _) = region.try_consume_playback_before_with_timestamp(
                sequence,
                u64::MAX,
                &mut playback_dst,
            );
            assert_eq!(outcome, PlaybackConsume::Ready);
            std::hint::black_box(&playback_dst);
        }
        let pb_ns = started.elapsed().as_nanos() / u128::from(ITERATIONS);

        let started = std::time::Instant::now();
        for sequence in 0..ITERATIONS {
            assert!(region.try_publish_capture(&mut cap_prod, sequence, &capture_src));
            assert!(region.has_ready_capture_since(cap_cons, Some(sequence)));
            let (read_seq, has_more) = region.try_client_read_valid_capture_with_remaining(
                &mut cap_cons,
                sequence,
                &mut capture_dst,
            );
            assert_eq!(read_seq, Some(sequence));
            assert!(!has_more);
            std::hint::black_box(&capture_dst);
        }
        let cap_ns = started.elapsed().as_nanos() / u128::from(ITERATIONS);

        let started = std::time::Instant::now();
        for sequence in 0..ITERATIONS {
            region.discard_capture_through(sequence, sequence);
        }
        let discard_idle_ns = started.elapsed().as_nanos() / u128::from(ITERATIONS);

        println!(
            "playback_roundtrip_ns={pb_ns} capture_shm_roundtrip_ns={cap_ns} discard_idle_ns={discard_idle_ns}"
        );
    }
}
