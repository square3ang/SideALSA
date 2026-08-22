use std::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU8, Ordering},
};

const SLOT_FREE: u8 = 0;
const SLOT_READY: u8 = 1;

pub trait ProCaptureSink: Send {
    fn process_capture(&mut self, sequence: u64, capture: &[i32]);
}

pub trait ProPlaybackSource: Send {
    fn process_playback(&mut self, sequence: u64, playback: &mut [i32]);
}

pub(crate) struct CaptureRing {
    slots: Box<[CaptureSlot]>,
}

impl CaptureRing {
    pub(crate) fn new(slot_count: usize, samples_per_slot: usize) -> Self {
        let slots = (0..slot_count)
            .map(|_| CaptureSlot {
                state: AtomicU8::new(SLOT_FREE),
                audio: UnsafeCell::new(vec![0; samples_per_slot].into_boxed_slice()),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { slots }
    }

    pub(crate) fn try_push(&self, index: &mut usize, audio: &[i32]) -> bool {
        let slot = &self.slots[*index];
        if slot.state.load(Ordering::Acquire) != SLOT_FREE {
            return false;
        }
        let destination = unsafe { &mut *slot.audio.get() };
        destination.copy_from_slice(audio);
        slot.state.store(SLOT_READY, Ordering::Release);
        *index = (*index + 1) % self.slots.len();
        true
    }

    pub(crate) fn try_pop(&self, index: &mut usize, audio: &mut [i32]) -> bool {
        let slot = &self.slots[*index];
        if slot.state.load(Ordering::Acquire) != SLOT_READY {
            return false;
        }
        let source = unsafe { &*slot.audio.get() };
        audio.copy_from_slice(source);
        slot.state.store(SLOT_FREE, Ordering::Release);
        *index = (*index + 1) % self.slots.len();
        true
    }
}

struct CaptureSlot {
    state: AtomicU8,
    audio: UnsafeCell<Box<[i32]>>,
}

unsafe impl Sync for CaptureSlot {}

#[cfg(test)]
mod tests {
    use super::CaptureRing;

    #[test]
    fn capture_ring_preserves_period_order() {
        let ring = CaptureRing::new(2, 2);
        let mut producer = 0;
        let mut consumer = 0;
        let mut output = [0; 2];

        assert!(ring.try_push(&mut producer, &[1, 2]));
        assert!(ring.try_push(&mut producer, &[3, 4]));
        assert!(!ring.try_push(&mut producer, &[5, 6]));
        assert!(ring.try_pop(&mut consumer, &mut output));
        assert_eq!(output, [1, 2]);
        assert!(ring.try_push(&mut producer, &[5, 6]));
        assert!(ring.try_pop(&mut consumer, &mut output));
        assert_eq!(output, [3, 4]);
        assert!(ring.try_pop(&mut consumer, &mut output));
        assert_eq!(output, [5, 6]);
        assert!(!ring.try_pop(&mut consumer, &mut output));
    }
}
