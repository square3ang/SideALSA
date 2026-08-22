use std::{
    cell::UnsafeCell,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    thread,
};

use crate::{EngineError, MAX_PRO_LATENCY_PERIODS};

const PRO_RING_SLOTS: usize = MAX_PRO_LATENCY_PERIODS as usize + 1;
const SLOT_FREE: u8 = 0;
const SLOT_READY: u8 = 1;

pub trait ProCallback: Send {
    fn process(&mut self, sequence: u64, capture: &[i32], playback: &mut [i32]);
}

struct AudioSlot {
    state: AtomicU8,
    sequence: AtomicU64,
    audio: UnsafeCell<Box<[i32]>>,
}

// State transitions publish exclusive ownership of each slot's audio buffer.
unsafe impl Sync for AudioSlot {}

impl AudioSlot {
    fn new(sample_count: usize) -> Self {
        Self {
            state: AtomicU8::new(SLOT_FREE),
            sequence: AtomicU64::new(0),
            audio: UnsafeCell::new(vec![0; sample_count].into_boxed_slice()),
        }
    }
}

pub(crate) struct ProRings {
    capture: SpscAudioRing,
    playback: SpscAudioRing,
}

impl ProRings {
    pub(crate) fn new(capture_samples: usize, playback_samples: usize) -> Self {
        Self {
            capture: SpscAudioRing::new(capture_samples),
            playback: SpscAudioRing::new(playback_samples),
        }
    }

    pub(crate) fn capture(&self) -> &SpscAudioRing {
        &self.capture
    }

    pub(crate) fn playback(&self) -> &SpscAudioRing {
        &self.playback
    }

    pub(crate) fn prime_playback(&self, samples: &[i32]) -> usize {
        let mut producer_index = 0;
        assert!(self.playback.try_push(&mut producer_index, 0, samples));
        producer_index
    }
}

pub(crate) fn playback_source_sequence(
    hardware_sequence: u64,
    latency_periods: u32,
) -> Option<u64> {
    hardware_sequence.checked_sub(u64::from(latency_periods))
}

pub(crate) enum PopResult {
    Ready,
    Missing,
}

pub(crate) struct SpscAudioRing {
    slots: Box<[AudioSlot]>,
}

impl SpscAudioRing {
    fn new(sample_count: usize) -> Self {
        let slots = (0..PRO_RING_SLOTS)
            .map(|_| AudioSlot::new(sample_count))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { slots }
    }

    pub(crate) fn try_push(
        &self,
        producer_index: &mut usize,
        sequence: u64,
        samples: &[i32],
    ) -> bool {
        let slot = &self.slots[*producer_index];
        if slot.state.load(Ordering::Acquire) != SLOT_FREE {
            return false;
        }

        unsafe {
            let audio = &mut *slot.audio.get();
            if samples.len() < audio.len() {
                return false;
            }
            audio.copy_from_slice(&samples[..audio.len()]);
        }
        slot.sequence.store(sequence, Ordering::Relaxed);
        slot.state.store(SLOT_READY, Ordering::Release);
        *producer_index = next_index(*producer_index, self.slots.len());
        true
    }

    pub(crate) fn try_pop_next(
        &self,
        consumer_index: &mut usize,
        samples: &mut [i32],
    ) -> Option<u64> {
        let slot = &self.slots[*consumer_index];
        if slot.state.load(Ordering::Acquire) != SLOT_READY {
            return None;
        }

        let sequence = slot.sequence.load(Ordering::Relaxed);
        unsafe {
            let audio = &*slot.audio.get();
            if samples.len() < audio.len() {
                return None;
            }
            samples[..audio.len()].copy_from_slice(audio);
        }
        slot.state.store(SLOT_FREE, Ordering::Release);
        *consumer_index = next_index(*consumer_index, self.slots.len());
        Some(sequence)
    }

    pub(crate) fn try_pop_exact(&self, expected_sequence: u64, samples: &mut [i32]) -> PopResult {
        for slot in self.slots.iter() {
            if slot.state.load(Ordering::Acquire) != SLOT_READY {
                continue;
            }

            let sequence = slot.sequence.load(Ordering::Relaxed);
            if sequence > expected_sequence {
                continue;
            }
            if sequence == expected_sequence {
                unsafe {
                    let audio = &*slot.audio.get();
                    if samples.len() < audio.len() {
                        return PopResult::Missing;
                    }
                    samples[..audio.len()].copy_from_slice(audio);
                }
                slot.state.store(SLOT_FREE, Ordering::Release);
                return PopResult::Ready;
            }

            slot.state.store(SLOT_FREE, Ordering::Release);
        }
        PopResult::Missing
    }
}

fn next_index(index: usize, length: usize) -> usize {
    (index + 1) % length
}

pub(crate) fn run_callback<C: ProCallback>(
    mut callback: C,
    rings: &ProRings,
    stop: &AtomicBool,
    done: &AtomicBool,
    capture: &mut [i32],
    playback: &mut [i32],
    playback_start_index: usize,
) -> Result<(), EngineError> {
    let mut capture_index = 0;
    let mut playback_index = playback_start_index;

    while !stop.load(Ordering::Relaxed) && !done.load(Ordering::Acquire) {
        let Some(sequence) = rings.capture().try_pop_next(&mut capture_index, capture) else {
            thread::yield_now();
            continue;
        };

        playback.fill(0);
        if catch_unwind(AssertUnwindSafe(|| {
            callback.process(sequence, capture, playback);
        }))
        .is_err()
        {
            done.store(true, Ordering::Release);
            return Err(EngineError::WorkerPanic);
        }
        while !rings
            .playback()
            .try_push(&mut playback_index, sequence, playback)
        {
            if stop.load(Ordering::Relaxed) || done.load(Ordering::Acquire) {
                return Ok(());
            }
            thread::yield_now();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_output_does_not_satisfy_new_sequence() {
        let ring = SpscAudioRing::new(2);
        let mut producer_index = 0;
        let mut samples = [0; 2];

        assert!(ring.try_push(&mut producer_index, 3, &[3, 4]));
        assert!(matches!(
            ring.try_pop_exact(2, &mut samples),
            PopResult::Missing
        ));
        assert!(ring.try_push(&mut producer_index, 4, &[4, 5]));
        assert!(matches!(
            ring.try_pop_exact(4, &mut samples),
            PopResult::Ready
        ));
        assert_eq!(samples, [4, 5]);
    }

    #[test]
    fn future_output_waits_for_matching_sequence() {
        let ring = SpscAudioRing::new(2);
        let mut producer_index = 0;
        let mut samples = [0; 2];

        assert!(ring.try_push(&mut producer_index, 4, &[4, 5]));
        assert!(matches!(
            ring.try_pop_exact(3, &mut samples),
            PopResult::Missing
        ));
        assert!(matches!(
            ring.try_pop_exact(4, &mut samples),
            PopResult::Ready
        ));
        assert_eq!(samples, [4, 5]);
    }

    #[test]
    fn playback_source_sequence_applies_configured_latency() {
        assert_eq!(playback_source_sequence(0, 1), None);
        assert_eq!(playback_source_sequence(1, 1), Some(0));
        assert_eq!(playback_source_sequence(4, 2), Some(2));
    }

    #[test]
    fn playback_prime_contains_only_zero_sequence() {
        let rings = ProRings::new(2, 2);
        let producer_index = rings.prime_playback(&[0, 0]);
        let mut samples = [1, 1];

        assert_eq!(producer_index, 1);
        assert!(matches!(
            rings.playback().try_pop_exact(0, &mut samples),
            PopResult::Ready
        ));
        assert_eq!(samples, [0, 0]);
        assert!(matches!(
            rings.playback().try_pop_exact(1, &mut samples),
            PopResult::Missing
        ));
    }
}
