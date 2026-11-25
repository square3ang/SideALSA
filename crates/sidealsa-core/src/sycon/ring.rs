//! Single-producer/single-consumer interleaved frame ring shared by the USB I/O
//! thread and the PRO cycle thread. All storage is allocated up front.

use std::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU64, Ordering},
};

pub(crate) struct FrameRing {
    samples: Box<[UnsafeCell<i32>]>,
    channels: usize,
    capacity: u64,
    /// Total frames ever written; only the producer stores it.
    tail: AtomicU64,
    /// Total frames ever consumed or skipped; only the consumer stores it.
    head: AtomicU64,
}

// SAFETY: producer and consumer touch disjoint frame ranges separated by the
// head/tail atomics (Release on publish, Acquire on observe).
unsafe impl Sync for FrameRing {}

impl FrameRing {
    pub(crate) fn new(channels: usize, capacity_frames: usize) -> Self {
        assert!(channels > 0 && capacity_frames > 0);
        Self {
            samples: (0..channels * capacity_frames)
                .map(|_| UnsafeCell::new(0))
                .collect(),
            channels,
            capacity: capacity_frames as u64,
            tail: AtomicU64::new(0),
            head: AtomicU64::new(0),
        }
    }

    /// Frames currently readable. Exact for either side's own view.
    pub(crate) fn len(&self) -> u64 {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);
        tail.saturating_sub(head)
    }

    #[cfg(test)]
    fn written(&self) -> u64 {
        self.tail.load(Ordering::Acquire)
    }

    /// Producer: appends whole frames from `samples`; returns frames written.
    pub(crate) fn push(&self, samples: &[i32]) -> u64 {
        self.push_with(
            (samples.len() / self.channels) as u64,
            |destination, offset| {
                destination.copy_from_slice(&samples[offset..offset + destination.len()]);
            },
        )
    }

    /// Producer: appends `frames` silent frames; returns frames written.
    pub(crate) fn push_silence(&self, frames: u64) -> u64 {
        self.push_with(frames, |destination, _| destination.fill(0))
    }

    /// Producer: appends frames decoded from little-endian S32 bytes.
    pub(crate) fn push_le_bytes(&self, bytes: &[u8]) -> u64 {
        self.push_with(
            (bytes.len() / (4 * self.channels)) as u64,
            |destination, offset| {
                let source = &bytes[offset * 4..(offset + destination.len()) * 4];
                for (sample, chunk) in destination.iter_mut().zip(source.as_chunks::<4>().0) {
                    *sample = i32::from_le_bytes(*chunk);
                }
            },
        )
    }

    fn push_with(&self, frames: u64, mut fill: impl FnMut(&mut [i32], usize)) -> u64 {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let free = self.capacity - (tail - head);
        let frames = frames.min(free);
        let mut done = 0_u64;
        while done < frames {
            let (start, count) = self.contiguous(tail + done, frames - done);
            // SAFETY: frames in [tail, head + capacity) belong to the producer.
            let destination = unsafe { self.slice_mut(start, count) };
            fill(destination, done as usize * self.channels);
            done += count;
        }
        self.tail.store(tail + frames, Ordering::Release);
        frames
    }

    /// Consumer: copies up to `destination.len() / channels` frames out.
    pub(crate) fn pop(&self, destination: &mut [i32]) -> u64 {
        self.pop_with(
            (destination.len() / self.channels) as u64,
            |source, offset| {
                destination[offset..offset + source.len()].copy_from_slice(source);
            },
        )
    }

    /// Consumer: encodes up to `destination.len() / (4 * channels)` frames as S32 LE.
    pub(crate) fn pop_le_bytes(&self, destination: &mut [u8]) -> u64 {
        self.pop_with(
            (destination.len() / (4 * self.channels)) as u64,
            |source, offset| {
                let target = &mut destination[offset * 4..(offset + source.len()) * 4];
                for (chunk, sample) in target.as_chunks_mut::<4>().0.iter_mut().zip(source) {
                    *chunk = sample.to_le_bytes();
                }
            },
        )
    }

    /// Consumer: drops up to `frames` readable frames.
    pub(crate) fn skip(&self, frames: u64) -> u64 {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let frames = frames.min(tail - head);
        self.head.store(head + frames, Ordering::Release);
        frames
    }

    fn pop_with(&self, frames: u64, mut drain: impl FnMut(&[i32], usize)) -> u64 {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let frames = frames.min(tail - head);
        let mut done = 0_u64;
        while done < frames {
            let (start, count) = self.contiguous(head + done, frames - done);
            // SAFETY: frames in [head, tail) were published by the producer.
            let source = unsafe { self.slice(start, count) };
            drain(source, done as usize * self.channels);
            done += count;
        }
        self.head.store(head + frames, Ordering::Release);
        frames
    }

    fn contiguous(&self, position: u64, frames: u64) -> (usize, u64) {
        let start = position % self.capacity;
        (start as usize, frames.min(self.capacity - start))
    }

    unsafe fn slice(&self, start: usize, frames: u64) -> &[i32] {
        let begin = start * self.channels;
        let length = frames as usize * self.channels;
        // SAFETY: caller guarantees exclusive-or-shared access per SPSC rules.
        unsafe {
            std::slice::from_raw_parts(
                UnsafeCell::raw_get(self.samples.as_ptr().add(begin)),
                length,
            )
        }
    }

    #[allow(clippy::mut_from_ref)]
    unsafe fn slice_mut(&self, start: usize, frames: u64) -> &mut [i32] {
        let begin = start * self.channels;
        let length = frames as usize * self.channels;
        // SAFETY: caller guarantees the producer owns this range.
        unsafe {
            std::slice::from_raw_parts_mut(
                UnsafeCell::raw_get(self.samples.as_ptr().add(begin)),
                length,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, thread};

    use super::*;

    #[test]
    fn wraps_and_preserves_order() {
        let ring = FrameRing::new(2, 5);
        assert_eq!(ring.push(&[1, 2, 3, 4, 5, 6]), 3);
        let mut out = [0; 4];
        assert_eq!(ring.pop(&mut out), 2);
        assert_eq!(out, [1, 2, 3, 4]);
        assert_eq!(ring.push(&[7, 8, 9, 10, 11, 12, 13, 14, 15, 16]), 4);
        assert_eq!(ring.len(), 5);
        let mut out = [0; 10];
        assert_eq!(ring.pop(&mut out), 5);
        assert_eq!(out, [5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
        assert_eq!(ring.written(), 7);
    }

    #[test]
    fn byte_codec_round_trips_and_skip_drops() {
        let ring = FrameRing::new(2, 8);
        let bytes: Vec<u8> = [-1_i32, 2, i32::MIN, i32::MAX]
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        assert_eq!(ring.push_le_bytes(&bytes), 2);
        assert_eq!(ring.push_silence(1), 1);
        let mut encoded = [0_u8; 16];
        assert_eq!(ring.pop_le_bytes(&mut encoded), 2);
        assert_eq!(&encoded[..], &bytes[..]);
        assert_eq!(ring.skip(4), 1);
        assert_eq!(ring.len(), 0);
    }

    #[test]
    fn byte_codec_wraps_across_boundary() {
        let ring = FrameRing::new(2, 5);
        assert_eq!(ring.push(&[1, 2, 3, 4, 5, 6]), 3);
        assert_eq!(ring.skip(2), 2);
        let wrapped_in: Vec<u8> = [7_i32, 8, -9, 10, i32::MIN, i32::MAX, 11, 12]
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        // Tail wraps from 3 -> 0 -> 2.
        assert_eq!(ring.push_le_bytes(&wrapped_in), 4);
        assert_eq!(ring.len(), 5);
        let expected: Vec<u8> = [5_i32, 6, 7, 8, -9, 10, i32::MIN, i32::MAX, 11, 12]
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let mut encoded = vec![0_u8; expected.len()];
        // Head wraps from 2 -> 0 -> 2.
        assert_eq!(ring.pop_le_bytes(&mut encoded), 5);
        assert_eq!(encoded, expected);
        assert_eq!(ring.skip(u64::MAX), 0);
    }

    #[test]
    fn concurrent_transfer_is_lossless() {
        let ring = Arc::new(FrameRing::new(1, 64));
        let producer = Arc::clone(&ring);
        let total = 100_000_i32;
        let writer = thread::spawn(move || {
            let mut next = 0;
            while next < total {
                let chunk: Vec<i32> = (next..(next + 7).min(total)).collect();
                next += producer.push(&chunk) as i32;
            }
        });
        let mut expected = 0;
        let mut out = [0; 13];
        while expected < total {
            let read = ring.pop(&mut out) as usize;
            for &value in &out[..read] {
                assert_eq!(value, expected);
                expected += 1;
            }
        }
        writer.join().unwrap();
    }
}
