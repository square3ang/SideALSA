use std::{
    ffi::CString,
    io,
    mem::size_of,
    os::fd::RawFd,
    ptr::{self, NonNull},
    slice,
    sync::atomic::Ordering,
};

use sidealsa_protocol::{
    SHARED_CLIENT_IDLE, SHARED_MAGIC, SHARED_SLOT_FREE, SHARED_SLOT_READY, SHARED_VERSION,
    SharedRegionHeader, SharedRegionInfo, SharedRegionLayout, SharedSlotHeader,
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

pub struct SharedRegion {
    fd: RawFd,
    ptr: NonNull<u8>,
    layout: SharedRegionLayout,
    capture_samples: usize,
    playback_samples: usize,
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
        let layout = SharedRegionLayout::new(
            period_frames,
            playback_channels,
            capture_channels,
            sidealsa_protocol::SHARED_SLOT_COUNT,
        )?;
        let name = CString::new("sidealsa-pro").expect("static name has no nul");
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
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
        let info = layout.info();
        let capture_samples = sample_count(info.period_frames, info.capture_channels)?;
        let playback_samples = sample_count(info.period_frames, info.playback_channels)?;
        Ok(Self {
            fd,
            ptr,
            layout,
            capture_samples,
            playback_samples,
        })
    }

    fn initialize(&self) {
        let header = SharedRegionHeader::new(&self.layout);
        unsafe {
            ptr::write(self.ptr.as_ptr() as *mut SharedRegionHeader, header);
            for index in 0..self.layout.slot_count() {
                ptr::write(
                    self.slot_ptr(self.layout.capture_offset(), index),
                    SharedSlotHeader::new(),
                );
                ptr::write(
                    self.slot_ptr(self.layout.playback_offset(), index),
                    SharedSlotHeader::new(),
                );
            }
        }
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    pub fn info(&self) -> SharedRegionInfo {
        self.layout.info()
    }

    pub fn set_cycle_sequence(&self, sequence: u64) {
        self.header()
            .cycle_sequence
            .store(sequence, Ordering::Release);
    }

    pub fn cycle_sequence(&self) -> u64 {
        self.header().cycle_sequence.load(Ordering::Acquire)
    }

    pub fn set_lifecycle_generation(&self, generation: u64) {
        self.header()
            .lifecycle_generation
            .store(generation, Ordering::Release);
    }

    pub fn lifecycle_generation(&self) -> u64 {
        self.header().lifecycle_generation.load(Ordering::Acquire)
    }

    pub fn set_client_state(&self, state: u32) {
        self.header().client_state.store(state, Ordering::Release);
    }

    pub fn client_state(&self) -> u32 {
        self.header().client_state.load(Ordering::Acquire)
    }

    pub fn reset_slots(&self) {
        self.set_client_state(SHARED_CLIENT_IDLE);
        for ring_offset in [self.layout.capture_offset(), self.layout.playback_offset()] {
            for index in 0..self.layout.slot_count() {
                let slot = unsafe { self.slot(ring_offset, index) };
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
            self.layout.capture_offset(),
            self.capture_samples,
            producer_index,
            sequence,
            samples,
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
            self.layout.playback_offset(),
            self.playback_samples,
            producer_index,
            sequence,
            samples,
        );
        if published {
            self.set_client_state(sidealsa_protocol::SHARED_CLIENT_RUNNING);
        }
        published
    }

    fn try_publish(
        &self,
        ring_offset: usize,
        sample_count: usize,
        producer_index: &mut usize,
        sequence: u64,
        samples: &[i32],
    ) -> bool {
        if sample_count == 0 {
            return false;
        }
        if samples.len() < sample_count {
            return false;
        }
        let slot_count = self.layout.slot_count();
        for offset in 0..slot_count {
            let index = (*producer_index + offset) % slot_count;
            let slot = unsafe { self.slot(ring_offset, index) };
            if slot.state.load(Ordering::Acquire) != SHARED_SLOT_FREE {
                continue;
            }
            unsafe {
                slice::from_raw_parts_mut(self.audio_ptr(ring_offset, index), sample_count)
                    .copy_from_slice(&samples[..sample_count]);
            }
            slot.sequence.store(sequence, Ordering::Relaxed);
            slot.state.store(SHARED_SLOT_READY, Ordering::Release);
            *producer_index = next_index(index, slot_count);
            return true;
        }
        false
    }

    pub fn try_consume_playback(&self, sequence: u64, samples: &mut [i32]) -> bool {
        self.try_consume_exact(
            self.layout.playback_offset(),
            self.playback_samples,
            sequence,
            samples,
        )
    }

    fn try_consume_exact(
        &self,
        ring_offset: usize,
        sample_count: usize,
        expected_sequence: u64,
        samples: &mut [i32],
    ) -> bool {
        if sample_count == 0 {
            return false;
        }
        if samples.len() < sample_count {
            return false;
        }
        for index in 0..self.layout.slot_count() {
            let slot = unsafe { self.slot(ring_offset, index) };
            if slot.state.load(Ordering::Acquire) != SHARED_SLOT_READY {
                continue;
            }
            let sequence = slot.sequence.load(Ordering::Relaxed);
            if sequence > expected_sequence {
                continue;
            }
            if sequence == expected_sequence {
                unsafe {
                    samples[..sample_count].copy_from_slice(self.audio(
                        ring_offset,
                        index,
                        sample_count,
                    ));
                }
                slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
                return true;
            }
            slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
        }
        false
    }

    pub fn try_client_read_capture(
        &self,
        consumer_index: &mut usize,
        samples: &mut [i32],
    ) -> Option<u64> {
        if self.capture_samples == 0 {
            return None;
        }
        let slot = unsafe { self.slot(self.layout.capture_offset(), *consumer_index) };
        if slot.state.load(Ordering::Acquire) != SHARED_SLOT_READY
            || samples.len() < self.capture_samples
        {
            return None;
        }
        let sequence = slot.sequence.load(Ordering::Relaxed);
        unsafe {
            samples[..self.capture_samples].copy_from_slice(self.audio(
                self.layout.capture_offset(),
                *consumer_index,
                self.capture_samples,
            ));
        }
        slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
        *consumer_index = next_index(*consumer_index, self.layout.slot_count());
        Some(sequence)
    }

    pub fn try_client_read_capture_at_least(
        &self,
        consumer_index: &mut usize,
        expected_sequence: u64,
        samples: &mut [i32],
    ) -> Option<u64> {
        if self.capture_samples == 0 || samples.len() < self.capture_samples {
            return None;
        }

        let slot_count = self.layout.slot_count();
        let mut selected = None;
        for offset in 0..slot_count {
            let index = (*consumer_index + offset) % slot_count;
            let slot = unsafe { self.slot(self.layout.capture_offset(), index) };
            if slot.state.load(Ordering::Acquire) != SHARED_SLOT_READY {
                continue;
            }
            let sequence = slot.sequence.load(Ordering::Relaxed);
            if sequence < expected_sequence {
                slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
                continue;
            }
            if selected.is_none_or(|(_, selected_sequence)| sequence < selected_sequence) {
                selected = Some((index, sequence));
            }
        }

        let (index, sequence) = selected?;
        unsafe {
            samples[..self.capture_samples].copy_from_slice(self.audio(
                self.layout.capture_offset(),
                index,
                self.capture_samples,
            ));
        }
        let slot = unsafe { self.slot(self.layout.capture_offset(), index) };
        slot.state.store(SHARED_SLOT_FREE, Ordering::Release);
        *consumer_index = next_index(index, slot_count);
        Some(sequence)
    }

    unsafe fn slot(&self, ring_offset: usize, index: usize) -> &SharedSlotHeader {
        unsafe { &*self.slot_ptr(ring_offset, index) }
    }

    fn header(&self) -> &SharedRegionHeader {
        unsafe { &*(self.ptr.as_ptr() as *const SharedRegionHeader) }
    }

    unsafe fn slot_ptr(&self, ring_offset: usize, index: usize) -> *mut SharedSlotHeader {
        let offset = self
            .layout
            .slot_offset(ring_offset, index)
            .expect("shared slot index is bounded");
        unsafe { self.ptr.as_ptr().add(offset).cast::<SharedSlotHeader>() }
    }

    unsafe fn audio(&self, ring_offset: usize, index: usize, samples: usize) -> &[i32] {
        let offset = self
            .layout
            .slot_offset(ring_offset, index)
            .expect("shared slot index is bounded")
            + size_of::<SharedSlotHeader>();
        unsafe { slice::from_raw_parts(self.ptr.as_ptr().add(offset).cast::<i32>(), samples) }
    }

    unsafe fn audio_ptr(&self, ring_offset: usize, index: usize) -> *mut i32 {
        let offset = self
            .layout
            .slot_offset(ring_offset, index)
            .expect("shared slot index is bounded")
            + size_of::<SharedSlotHeader>();
        unsafe { self.ptr.as_ptr().add(offset).cast::<i32>() }
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

fn next_index(index: usize, slot_count: usize) -> usize {
    (index + 1) % slot_count
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

fn close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn reset_slots_keeps_timeline_sequence() {
        let region = SharedRegion::create(1, 1, 1).expect("region should create");
        region.set_cycle_sequence(42);
        region.set_client_state(sidealsa_protocol::SHARED_CLIENT_RUNNING);
        let mut playback_index = 0;
        assert!(region.try_client_publish_playback(&mut playback_index, 7, &[7]));

        region.reset_slots();

        let mut samples = [0];
        assert!(!region.try_consume_playback(7, &mut samples));
        assert_eq!(region.cycle_sequence(), 42);
        assert_eq!(region.client_state(), sidealsa_protocol::SHARED_CLIENT_IDLE);
    }
}
