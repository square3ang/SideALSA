use std::sync::atomic::{AtomicI64, AtomicU64, Ordering, fence};

use crate::StreamDirection;

const PRO_TIMING_SLOT_COUNT: usize = 8;
const DUPLEX_POINTER_PHASE_SAMPLE_COUNT: usize = 128;

#[repr(align(64))]
#[derive(Debug)]
struct DuplexPointerPhase {
    sample_count: AtomicU64,
    samples: [AtomicI64; DUPLEX_POINTER_PHASE_SAMPLE_COUNT],
}

impl Default for DuplexPointerPhase {
    fn default() -> Self {
        Self {
            sample_count: AtomicU64::new(0),
            samples: [const { AtomicI64::new(0) }; DUPLEX_POINTER_PHASE_SAMPLE_COUNT],
        }
    }
}

#[repr(align(64))]
#[derive(Debug, Default)]
struct ProTimingSlot {
    sequence: AtomicU64,
    capture_read_nanos: AtomicU64,
}

// Read-mostly and restart-only state kept off hot RT write cache lines so
// frequent `generation()` loads from control/client threads never bounce.
#[repr(align(64))]
#[derive(Debug, Default)]
struct ControlCounters {
    generation: AtomicU64,
    timeline_resets: AtomicU64,
    linked_phase_rebases: AtomicU64,
    playback_xruns: AtomicU64,
    capture_xruns: AtomicU64,
    linked_phase_result_epoch: AtomicU64,
    linked_phase_attempts: AtomicU64,
    linked_phase_score_nanos: AtomicU64,
    linked_phase_target_met: AtomicU64,
}

// Hot playback/linked RT thread counters, with per-period fields packed first.
#[repr(align(64))]
#[derive(Debug, Default)]
struct PlaybackCounters {
    sample_position: AtomicU64,
    periods_processed: AtomicU64,
    playback_position: AtomicU64,
    pro_wait_budget_min_nanos: AtomicU64,
    pro_wait_budget_max_nanos: AtomicU64,
    playback_write_max_nanos: AtomicU64,
    capture_to_playback_write_nanos: AtomicU64,
    capture_to_playback_write_min_nanos: AtomicU64,
    capture_to_playback_write_max_nanos: AtomicU64,
    pro_playback_blocks: AtomicU64,
    pro_playback_nonzero_blocks: AtomicU64,
    pro_ready_wait_max_nanos: AtomicU64,
    playback_delay_frames: AtomicU64,
    playback_delay_min_frames: AtomicU64,
    playback_delay_max_frames: AtomicU64,
    playback_ring_delay_frames: AtomicU64,
    playback_ring_delay_min_frames: AtomicU64,
    playback_ring_delay_max_frames: AtomicU64,
    playback_driver_delay_frames: AtomicU64,
    playback_driver_delay_min_frames: AtomicU64,
    playback_driver_delay_max_frames: AtomicU64,
    playback_target_overshoot_max_frames: AtomicU64,
    playback_low_watermarks: AtomicU64,
    pro_deadline_misses: AtomicU64,
    pro_client_deadline_misses: AtomicU64,
    pro_core_deadline_misses: AtomicU64,
    shared_underruns: AtomicU64,
}

// Hot capture RT thread counters separated from playback counters for split-worker mode.
#[repr(align(64))]
#[derive(Debug, Default)]
struct CaptureCounters {
    capture_position: AtomicU64,
    capture_delay_frames: AtomicU64,
    capture_delay_min_frames: AtomicU64,
    capture_delay_max_frames: AtomicU64,
    pro_capture_overruns: AtomicU64,
    shared_overruns: AtomicU64,
}

#[derive(Debug, Default)]
pub struct HardwareTimeline {
    control: ControlCounters,
    playback: PlaybackCounters,
    capture: CaptureCounters,
    pro_timing: [ProTimingSlot; PRO_TIMING_SLOT_COUNT],
    duplex_pointer_phase: DuplexPointerPhase,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HardwareStats {
    pub generation: u64,
    pub sample_position: u64,
    pub playback_position: u64,
    pub capture_position: u64,
    pub hw_playback_xruns: u64,
    pub hw_capture_xruns: u64,
    pub playback_delay_frames: u64,
    pub capture_delay_frames: u64,
    pub playback_delay_min_frames: u64,
    pub playback_delay_max_frames: u64,
    pub playback_ring_delay_frames: u64,
    pub playback_ring_delay_min_frames: u64,
    pub playback_ring_delay_max_frames: u64,
    pub playback_driver_delay_frames: u64,
    pub playback_driver_delay_min_frames: u64,
    pub playback_driver_delay_max_frames: u64,
    pub capture_delay_min_frames: u64,
    pub capture_delay_max_frames: u64,
    pub playback_target_overshoot_max_frames: u64,
    pub pro_wait_budget_min_nanos: u64,
    pub pro_wait_budget_max_nanos: u64,
    pub pro_ready_wait_max_nanos: u64,
    pub playback_write_max_nanos: u64,
    pub capture_to_playback_write_nanos: u64,
    pub capture_to_playback_write_min_nanos: u64,
    pub capture_to_playback_write_max_nanos: u64,
    pub duplex_pointer_phase_nanos: i64,
    pub duplex_pointer_phase_min_nanos: i64,
    pub duplex_pointer_phase_max_nanos: i64,
    pub duplex_pointer_phase_samples: u64,
    pub linked_phase_attempts: u64,
    pub linked_phase_rebases: u64,
    pub linked_phase_score_nanos: u64,
    pub linked_phase_target_met: bool,
    pub playback_low_watermarks: u64,
    pub pro_deadline_misses: u64,
    pub pro_client_deadline_misses: u64,
    pub pro_core_deadline_misses: u64,
    pub pro_capture_overruns: u64,
    pub pro_playback_blocks: u64,
    pub pro_playback_nonzero_blocks: u64,
    pub shared_underruns: u64,
    pub shared_overruns: u64,
    pub timeline_resets: u64,
    pub periods_processed: u64,
}

impl HardwareTimeline {
    #[inline]
    pub fn generation(&self) -> u64 {
        self.control.generation.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> HardwareStats {
        let (linked_phase_attempts, linked_phase_score_nanos, linked_phase_target_met) =
            self.linked_phase_calibration();
        let (
            duplex_pointer_phase_nanos,
            duplex_pointer_phase_min_nanos,
            duplex_pointer_phase_max_nanos,
            duplex_pointer_phase_samples,
        ) = self.duplex_pointer_phase();
        HardwareStats {
            generation: self.control.generation.load(Ordering::Relaxed),
            sample_position: self.playback.sample_position.load(Ordering::Relaxed),
            playback_position: self.playback.playback_position.load(Ordering::Relaxed),
            capture_position: self.capture.capture_position.load(Ordering::Relaxed),
            hw_playback_xruns: self.control.playback_xruns.load(Ordering::Relaxed),
            hw_capture_xruns: self.control.capture_xruns.load(Ordering::Relaxed),
            playback_delay_frames: self.playback.playback_delay_frames.load(Ordering::Relaxed),
            capture_delay_frames: self.capture.capture_delay_frames.load(Ordering::Relaxed),
            playback_delay_min_frames: decode_minimum(
                self.playback
                    .playback_delay_min_frames
                    .load(Ordering::Relaxed),
            ),
            playback_delay_max_frames: self
                .playback
                .playback_delay_max_frames
                .load(Ordering::Relaxed),
            playback_ring_delay_frames: self
                .playback
                .playback_ring_delay_frames
                .load(Ordering::Relaxed),
            playback_ring_delay_min_frames: decode_minimum(
                self.playback
                    .playback_ring_delay_min_frames
                    .load(Ordering::Relaxed),
            ),
            playback_ring_delay_max_frames: self
                .playback
                .playback_ring_delay_max_frames
                .load(Ordering::Relaxed),
            playback_driver_delay_frames: self
                .playback
                .playback_driver_delay_frames
                .load(Ordering::Relaxed),
            playback_driver_delay_min_frames: decode_minimum(
                self.playback
                    .playback_driver_delay_min_frames
                    .load(Ordering::Relaxed),
            ),
            playback_driver_delay_max_frames: self
                .playback
                .playback_driver_delay_max_frames
                .load(Ordering::Relaxed),
            capture_delay_min_frames: decode_minimum(
                self.capture
                    .capture_delay_min_frames
                    .load(Ordering::Relaxed),
            ),
            capture_delay_max_frames: self
                .capture
                .capture_delay_max_frames
                .load(Ordering::Relaxed),
            playback_target_overshoot_max_frames: self
                .playback
                .playback_target_overshoot_max_frames
                .load(Ordering::Relaxed),
            pro_wait_budget_min_nanos: decode_minimum(
                self.playback
                    .pro_wait_budget_min_nanos
                    .load(Ordering::Relaxed),
            ),
            pro_wait_budget_max_nanos: self
                .playback
                .pro_wait_budget_max_nanos
                .load(Ordering::Relaxed),
            pro_ready_wait_max_nanos: self
                .playback
                .pro_ready_wait_max_nanos
                .load(Ordering::Relaxed),
            playback_write_max_nanos: self
                .playback
                .playback_write_max_nanos
                .load(Ordering::Relaxed),
            capture_to_playback_write_nanos: self
                .playback
                .capture_to_playback_write_nanos
                .load(Ordering::Relaxed),
            capture_to_playback_write_min_nanos: decode_minimum(
                self.playback
                    .capture_to_playback_write_min_nanos
                    .load(Ordering::Relaxed),
            ),
            capture_to_playback_write_max_nanos: self
                .playback
                .capture_to_playback_write_max_nanos
                .load(Ordering::Relaxed),
            duplex_pointer_phase_nanos,
            duplex_pointer_phase_min_nanos,
            duplex_pointer_phase_max_nanos,
            duplex_pointer_phase_samples,
            linked_phase_attempts,
            linked_phase_rebases: self.control.linked_phase_rebases.load(Ordering::Relaxed),
            linked_phase_score_nanos,
            linked_phase_target_met,
            playback_low_watermarks: self
                .playback
                .playback_low_watermarks
                .load(Ordering::Relaxed),
            pro_deadline_misses: self.playback.pro_deadline_misses.load(Ordering::Relaxed),
            pro_client_deadline_misses: self
                .playback
                .pro_client_deadline_misses
                .load(Ordering::Relaxed),
            pro_core_deadline_misses: self
                .playback
                .pro_core_deadline_misses
                .load(Ordering::Relaxed),
            pro_capture_overruns: self.capture.pro_capture_overruns.load(Ordering::Relaxed),
            pro_playback_blocks: self.playback.pro_playback_blocks.load(Ordering::Relaxed),
            pro_playback_nonzero_blocks: self
                .playback
                .pro_playback_nonzero_blocks
                .load(Ordering::Relaxed),
            shared_underruns: self.playback.shared_underruns.load(Ordering::Relaxed),
            shared_overruns: self.capture.shared_overruns.load(Ordering::Relaxed),
            timeline_resets: self.control.timeline_resets.load(Ordering::Relaxed),
            periods_processed: self.playback.periods_processed.load(Ordering::Relaxed),
        }
    }

    #[inline]
    pub(crate) fn update_playback_position(&self, playback_position: u64) {
        self.playback
            .playback_position
            .store(playback_position, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn update_capture_position(&self, capture_position: u64) {
        self.capture
            .capture_position
            .store(capture_position, Ordering::Relaxed);
    }

    // Single-writer (playback/linked RT thread): plain load+add+store avoids two
    // locked RMW (`LOCK XADD`) full barriers per audio period.
    #[inline]
    pub(crate) fn processed_frames(&self, frames: u64, periods: u64) {
        let next_samples = self
            .playback
            .sample_position
            .load(Ordering::Relaxed)
            .wrapping_add(frames);
        self.playback
            .sample_position
            .store(next_samples, Ordering::Relaxed);
        let next_periods = self
            .playback
            .periods_processed
            .load(Ordering::Relaxed)
            .wrapping_add(periods);
        self.playback
            .periods_processed
            .store(next_periods, Ordering::Relaxed);
    }

    pub(crate) fn update_pcm_delay(
        &self,
        direction: StreamDirection,
        delay_frames: i64,
        period_frames: u64,
    ) {
        let delay_frames = u64::try_from(delay_frames).unwrap_or(0);
        match direction {
            StreamDirection::Playback => {
                self.playback
                    .playback_delay_frames
                    .store(delay_frames, Ordering::Relaxed);
                update_minimum_single_writer(
                    &self.playback.playback_delay_min_frames,
                    delay_frames,
                );
                update_maximum_single_writer(
                    &self.playback.playback_delay_max_frames,
                    delay_frames,
                );
                if delay_frames < period_frames {
                    let next = self
                        .playback
                        .playback_low_watermarks
                        .load(Ordering::Relaxed)
                        .wrapping_add(1);
                    self.playback
                        .playback_low_watermarks
                        .store(next, Ordering::Relaxed);
                }
            }
            StreamDirection::Capture => {
                self.capture
                    .capture_delay_frames
                    .store(delay_frames, Ordering::Relaxed);
                update_minimum_single_writer(&self.capture.capture_delay_min_frames, delay_frames);
                update_maximum_single_writer(&self.capture.capture_delay_max_frames, delay_frames);
            }
        }
    }

    #[inline]
    pub(crate) fn record_playback_target_overshoot(&self, frames: u64) {
        update_maximum_single_writer(&self.playback.playback_target_overshoot_max_frames, frames);
    }

    pub(crate) fn update_playback_delay_breakdown(
        &self,
        total_delay: i64,
        available: i64,
        buffer: i64,
    ) {
        let ring_delay = u64::try_from(buffer.saturating_sub(available.min(buffer))).unwrap_or(0);
        let total_delay = u64::try_from(total_delay).unwrap_or(0);
        let driver_delay = total_delay.saturating_sub(ring_delay);

        self.playback
            .playback_ring_delay_frames
            .store(ring_delay, Ordering::Relaxed);
        update_minimum_single_writer(&self.playback.playback_ring_delay_min_frames, ring_delay);
        update_maximum_single_writer(&self.playback.playback_ring_delay_max_frames, ring_delay);
        self.playback
            .playback_driver_delay_frames
            .store(driver_delay, Ordering::Relaxed);
        update_minimum_single_writer(
            &self.playback.playback_driver_delay_min_frames,
            driver_delay,
        );
        update_maximum_single_writer(
            &self.playback.playback_driver_delay_max_frames,
            driver_delay,
        );
    }

    #[inline]
    pub(crate) fn record_pro_wait_budget(&self, nanos: u64) {
        update_minimum_single_writer(&self.playback.pro_wait_budget_min_nanos, nanos);
        update_maximum_single_writer(&self.playback.pro_wait_budget_max_nanos, nanos);
    }

    pub fn record_pro_ready_wait(&self, nanos: u64) {
        update_maximum(&self.playback.pro_ready_wait_max_nanos, nanos);
    }

    #[inline]
    pub(crate) fn record_playback_write(&self, nanos: u64) {
        update_maximum_single_writer(&self.playback.playback_write_max_nanos, nanos);
    }

    pub(crate) fn record_linked_phase_calibration(
        &self,
        attempts: u64,
        score_nanos: u64,
        target_met: bool,
    ) {
        // Both duplex workers can reset after a restart; keep the writer side exclusive.
        loop {
            let epoch = self
                .control
                .linked_phase_result_epoch
                .load(Ordering::Relaxed);
            if epoch.is_multiple_of(2)
                && self
                    .control
                    .linked_phase_result_epoch
                    .compare_exchange_weak(epoch, epoch + 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                break;
            }
            std::hint::spin_loop();
        }
        fence(Ordering::Release);
        self.control
            .linked_phase_attempts
            .store(attempts, Ordering::Relaxed);
        self.control
            .linked_phase_score_nanos
            .store(score_nanos, Ordering::Relaxed);
        self.control
            .linked_phase_target_met
            .store(u64::from(target_met), Ordering::Relaxed);
        self.control
            .linked_phase_result_epoch
            .fetch_add(1, Ordering::Release);
    }

    fn linked_phase_calibration(&self) -> (u64, u64, bool) {
        loop {
            let start = self
                .control
                .linked_phase_result_epoch
                .load(Ordering::Acquire);
            if !start.is_multiple_of(2) {
                std::hint::spin_loop();
                continue;
            }
            let attempts = self.control.linked_phase_attempts.load(Ordering::Relaxed);
            let score_nanos = self
                .control
                .linked_phase_score_nanos
                .load(Ordering::Relaxed);
            let target_met = self.control.linked_phase_target_met.load(Ordering::Relaxed) != 0;
            fence(Ordering::Acquire);
            if start
                == self
                    .control
                    .linked_phase_result_epoch
                    .load(Ordering::Relaxed)
            {
                return (attempts, score_nanos, target_met);
            }
        }
    }

    #[inline]
    pub(crate) fn record_pro_capture_read(&self, sequence: u64, nanos: u64) {
        let slot = &self.pro_timing[sequence as usize % PRO_TIMING_SLOT_COUNT];
        // Invalidate first so a reader on another thread cannot pair the old sequence
        // with the new timestamp.
        slot.sequence.store(u64::MAX, Ordering::Relaxed);
        fence(Ordering::Release);
        slot.capture_read_nanos.store(nanos, Ordering::Relaxed);
        slot.sequence.store(sequence, Ordering::Release);
    }

    /// Matches an endpoint timestamp to the recent capture read. Direct duplex
    /// callers use this on the same RT thread that records capture timing.
    pub fn pro_capture_elapsed_nanos(&self, sequence: u64, nanos: u64) -> Option<u64> {
        let slot = &self.pro_timing[sequence as usize % PRO_TIMING_SLOT_COUNT];
        if slot.sequence.load(Ordering::Acquire) != sequence {
            return None;
        }
        let capture_read_nanos = slot.capture_read_nanos.load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        if slot.sequence.load(Ordering::Relaxed) != sequence {
            return None;
        }
        if capture_read_nanos == 0 || nanos < capture_read_nanos {
            return None;
        }
        Some(nanos - capture_read_nanos)
    }

    #[inline]
    pub(crate) fn record_capture_to_playback_write_duration(
        &self,
        capture_read_nanos: u64,
        write_finished_nanos: u64,
    ) {
        if capture_read_nanos == 0 || write_finished_nanos < capture_read_nanos {
            return;
        }
        self.record_capture_to_playback_write_elapsed(write_finished_nanos - capture_read_nanos);
    }

    pub(crate) fn record_pro_playback_write(&self, sequence: u64, nanos: u64) {
        let Some(duration) = self.pro_capture_elapsed_nanos(sequence, nanos) else {
            return;
        };
        self.record_capture_to_playback_write_elapsed(duration);
    }

    #[inline]
    fn record_capture_to_playback_write_elapsed(&self, duration: u64) {
        self.playback
            .capture_to_playback_write_nanos
            .store(duration, Ordering::Relaxed);
        update_minimum_single_writer(&self.playback.capture_to_playback_write_min_nanos, duration);
        update_maximum_single_writer(&self.playback.capture_to_playback_write_max_nanos, duration);
    }

    pub(crate) fn record_duplex_pointer_phase(&self, nanos: i64) {
        let count = self
            .duplex_pointer_phase
            .sample_count
            .load(Ordering::Relaxed);
        let slot = count as usize % DUPLEX_POINTER_PHASE_SAMPLE_COUNT;
        self.duplex_pointer_phase.samples[slot].store(nanos, Ordering::Relaxed);
        self.duplex_pointer_phase
            .sample_count
            .store(count.wrapping_add(1), Ordering::Release);
    }

    fn duplex_pointer_phase(&self) -> (i64, i64, i64, u64) {
        let total = self
            .duplex_pointer_phase
            .sample_count
            .load(Ordering::Acquire);
        let count = usize::try_from(total)
            .unwrap_or(usize::MAX)
            .min(DUPLEX_POINTER_PHASE_SAMPLE_COUNT);
        if count == 0 {
            return (0, 0, 0, 0);
        }

        let start = total.saturating_sub(count as u64);
        let mut samples = [0_i64; DUPLEX_POINTER_PHASE_SAMPLE_COUNT];
        let mut min_value = i64::MAX;
        let mut max_value = i64::MIN;
        for (offset, sample) in samples[..count].iter_mut().enumerate() {
            let index =
                start.wrapping_add(offset as u64) as usize % DUPLEX_POINTER_PHASE_SAMPLE_COUNT;
            let value = self.duplex_pointer_phase.samples[index].load(Ordering::Relaxed);
            *sample = value;
            min_value = min_value.min(value);
            max_value = max_value.max(value);
        }
        let (_, median, _) = samples[..count].select_nth_unstable(count / 2);
        (*median, min_value, max_value, total)
    }

    pub fn record_pro_deadline_miss(&self) {
        self.playback
            .pro_deadline_misses
            .fetch_add(1, Ordering::Relaxed);
        self.playback
            .pro_client_deadline_misses
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_pro_core_deadline_miss(&self) {
        self.playback
            .pro_deadline_misses
            .fetch_add(1, Ordering::Relaxed);
        self.playback
            .pro_core_deadline_misses
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_pro_capture_overrun(&self) {
        self.capture
            .pro_capture_overruns
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_pro_playback_block(&self, nonzero: bool) {
        self.playback
            .pro_playback_blocks
            .fetch_add(1, Ordering::Relaxed);
        if nonzero {
            self.playback
                .pro_playback_nonzero_blocks
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_shared_underrun(&self) {
        self.playback
            .shared_underruns
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_shared_overrun(&self) {
        self.capture.shared_overruns.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_hardware_xrun(&self, direction: StreamDirection) {
        match direction {
            StreamDirection::Playback => {
                self.control.playback_xruns.fetch_add(1, Ordering::Relaxed);
            }
            StreamDirection::Capture => {
                self.control.capture_xruns.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // Split workers restart each stream on its own FIFO thread and never calibrate linked phase,
    // so they must not contend on the calibration seqlock.
    pub(crate) fn reset_after_stream_restart(&self) {
        self.control.generation.fetch_add(1, Ordering::AcqRel);
        self.control.timeline_resets.fetch_add(1, Ordering::Relaxed);
        self.duplex_pointer_phase
            .sample_count
            .store(0, Ordering::Release);
    }

    pub(crate) fn reset_after_hardware_restart(&self) {
        self.reset_after_stream_restart();
        self.record_linked_phase_calibration(0, 0, false);
    }

    pub(crate) fn reset_after_hardware_rebase(&self) {
        self.control.generation.fetch_add(1, Ordering::AcqRel);
        self.control.timeline_resets.fetch_add(1, Ordering::Relaxed);
        self.control
            .linked_phase_rebases
            .fetch_add(1, Ordering::Relaxed);
        self.duplex_pointer_phase
            .sample_count
            .store(0, Ordering::Release);
    }
}

#[inline]
fn update_minimum_single_writer(target: &AtomicU64, value: u64) {
    let encoded = value.saturating_add(1);
    let current = target.load(Ordering::Relaxed);
    if current == 0 || encoded < current {
        target.store(encoded, Ordering::Relaxed);
    }
}

#[inline]
fn update_maximum_single_writer(target: &AtomicU64, value: u64) {
    if value > target.load(Ordering::Relaxed) {
        target.store(value, Ordering::Relaxed);
    }
}

fn update_maximum(target: &AtomicU64, value: u64) {
    let mut current = target.load(Ordering::Relaxed);
    while value > current {
        match target.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

fn decode_minimum(encoded: u64) -> u64 {
    encoded.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_position_stays_monotonic_across_xrun() {
        let timeline = HardwareTimeline::default();

        timeline.update_playback_position(64);
        timeline.update_capture_position(64);
        timeline.processed_frames(64, 1);
        timeline.record_hardware_xrun(StreamDirection::Capture);
        timeline.reset_after_hardware_restart();
        timeline.update_playback_position(0);
        timeline.update_capture_position(0);
        timeline.processed_frames(64, 1);

        let stats = timeline.snapshot();
        assert_eq!(stats.sample_position, 128);
        assert_eq!(stats.generation, 1);
        assert_eq!(stats.timeline_resets, 1);
        assert_eq!(stats.hw_capture_xruns, 1);
    }

    #[test]
    fn failed_xrun_recovery_is_counted_without_publishing_reset() {
        let timeline = HardwareTimeline::default();

        timeline.record_hardware_xrun(StreamDirection::Playback);

        let stats = timeline.snapshot();
        assert_eq!(stats.hw_playback_xruns, 1);
        assert_eq!(stats.generation, 0);
        assert_eq!(stats.timeline_resets, 0);
    }

    #[test]
    fn non_xrun_hardware_restart_resets_timeline_without_counting_xrun() {
        let timeline = HardwareTimeline::default();

        timeline.reset_after_hardware_restart();

        let stats = timeline.snapshot();
        assert_eq!(stats.generation, 1);
        assert_eq!(stats.timeline_resets, 1);
        assert_eq!(stats.hw_playback_xruns, 0);
        assert_eq!(stats.hw_capture_xruns, 0);
    }

    #[test]
    fn intentional_rebase_resets_timeline_without_counting_xrun() {
        let timeline = HardwareTimeline::default();

        timeline.reset_after_hardware_rebase();

        let stats = timeline.snapshot();
        assert_eq!(stats.generation, 1);
        assert_eq!(stats.timeline_resets, 1);
        assert_eq!(stats.linked_phase_rebases, 1);
        assert_eq!(stats.hw_playback_xruns, 0);
        assert_eq!(stats.hw_capture_xruns, 0);
    }

    #[test]
    fn xrun_invalidates_linked_phase_score() {
        let timeline = HardwareTimeline::default();
        timeline.record_linked_phase_calibration(3, 300_000, true);

        timeline.reset_after_hardware_restart();

        let stats = timeline.snapshot();
        assert_eq!(stats.linked_phase_attempts, 0);
        assert_eq!(stats.linked_phase_score_nanos, 0);
        assert!(!stats.linked_phase_target_met);
    }

    #[test]
    fn split_stream_restart_leaves_linked_phase_score_alone() {
        let timeline = HardwareTimeline::default();
        timeline.record_linked_phase_calibration(3, 300_000, true);

        timeline.reset_after_stream_restart();

        let stats = timeline.snapshot();
        assert_eq!(stats.generation, 1);
        assert_eq!(stats.linked_phase_attempts, 3);
        assert!(stats.linked_phase_target_met);
    }

    #[test]
    fn pro_deadline_miss_does_not_reset_hardware_timeline() {
        let timeline = HardwareTimeline::default();

        timeline.record_pro_deadline_miss();

        let stats = timeline.snapshot();
        assert_eq!(stats.pro_deadline_misses, 1);
        assert_eq!(stats.pro_client_deadline_misses, 1);
        assert_eq!(stats.pro_core_deadline_misses, 0);
        assert_eq!(stats.generation, 0);
        assert_eq!(stats.timeline_resets, 0);
        assert_eq!(stats.hw_playback_xruns, 0);
        assert_eq!(stats.hw_capture_xruns, 0);
    }

    #[test]
    fn shared_misses_do_not_reset_hardware_timeline() {
        let timeline = HardwareTimeline::default();

        timeline.record_shared_underrun();
        timeline.record_shared_overrun();

        let stats = timeline.snapshot();
        assert_eq!(stats.shared_underruns, 1);
        assert_eq!(stats.shared_overruns, 1);
        assert_eq!(stats.generation, 0);
        assert_eq!(stats.timeline_resets, 0);
    }

    #[test]
    fn playback_delay_tracks_low_watermarks_separately_from_xruns() {
        let timeline = HardwareTimeline::default();

        timeline.update_pcm_delay(StreamDirection::Playback, 128, 64);
        timeline.update_pcm_delay(StreamDirection::Capture, 32, 64);
        timeline.update_pcm_delay(StreamDirection::Playback, 63, 64);

        let stats = timeline.snapshot();
        assert_eq!(stats.playback_delay_frames, 63);
        assert_eq!(stats.capture_delay_frames, 32);
        assert_eq!(stats.playback_delay_min_frames, 63);
        assert_eq!(stats.playback_delay_max_frames, 128);
        assert_eq!(stats.capture_delay_min_frames, 32);
        assert_eq!(stats.capture_delay_max_frames, 32);
        assert_eq!(stats.playback_low_watermarks, 1);
        assert_eq!(stats.hw_playback_xruns, 0);
    }

    #[test]
    fn playback_delay_breakdown_separates_ring_and_driver() {
        let timeline = HardwareTimeline::default();

        timeline.update_playback_delay_breakdown(256, 128, 192);
        timeline.update_playback_delay_breakdown(200, 160, 192);

        let stats = timeline.snapshot();
        assert_eq!(stats.playback_ring_delay_frames, 32);
        assert_eq!(stats.playback_ring_delay_min_frames, 32);
        assert_eq!(stats.playback_ring_delay_max_frames, 64);
        assert_eq!(stats.playback_driver_delay_frames, 168);
        assert_eq!(stats.playback_driver_delay_min_frames, 168);
        assert_eq!(stats.playback_driver_delay_max_frames, 192);
    }

    #[test]
    fn capture_timestamp_correlation_rejects_unknown_reversed_and_overwritten_samples() {
        let timeline = HardwareTimeline::default();
        assert_eq!(timeline.pro_capture_elapsed_nanos(0, 100), None);
        timeline.record_pro_capture_read(5, 100);
        assert_eq!(timeline.pro_capture_elapsed_nanos(5, 150), Some(50));
        assert_eq!(timeline.pro_capture_elapsed_nanos(5, 99), None);
        assert_eq!(timeline.pro_capture_elapsed_nanos(6, 150), None);
        timeline.record_pro_capture_read(13, 200);
        assert_eq!(timeline.pro_capture_elapsed_nanos(5, 250), None);
        assert_eq!(timeline.pro_capture_elapsed_nanos(13, 250), Some(50));
    }

    #[test]
    fn exact_sequence_timing_tracks_capture_to_playback() {
        let timeline = HardwareTimeline::default();

        timeline.record_pro_capture_read(9, 100);
        timeline.record_pro_playback_write(8, 140);
        timeline.record_pro_playback_write(9, 160);

        let stats = timeline.snapshot();
        assert_eq!(stats.capture_to_playback_write_nanos, 60);
        assert_eq!(stats.capture_to_playback_write_min_nanos, 60);
        assert_eq!(stats.capture_to_playback_write_max_nanos, 60);
    }

    #[test]
    fn duplex_pointer_phase_reports_the_recent_window() {
        let timeline = HardwareTimeline::default();
        for nanos in 1..=130 {
            timeline.record_duplex_pointer_phase(nanos);
        }

        let stats = timeline.snapshot();
        assert_eq!(stats.duplex_pointer_phase_nanos, 67);
        assert_eq!(stats.duplex_pointer_phase_min_nanos, 3);
        assert_eq!(stats.duplex_pointer_phase_max_nanos, 130);
        assert_eq!(stats.duplex_pointer_phase_samples, 130);

        timeline.reset_after_hardware_restart();
        assert_eq!(timeline.snapshot().duplex_pointer_phase_samples, 0);
    }

    #[test]
    fn timing_diagnostics_track_bounds() {
        let timeline = HardwareTimeline::default();

        timeline.record_playback_target_overshoot(4);
        timeline.record_playback_target_overshoot(2);
        timeline.record_pro_wait_budget(0);
        timeline.record_pro_wait_budget(50);
        timeline.record_pro_ready_wait(30);
        timeline.record_playback_write(40);

        let stats = timeline.snapshot();
        assert_eq!(stats.playback_target_overshoot_max_frames, 4);
        assert_eq!(stats.pro_wait_budget_min_nanos, 0);
        assert_eq!(stats.pro_wait_budget_max_nanos, 50);
        assert_eq!(stats.pro_ready_wait_max_nanos, 30);
        assert_eq!(stats.playback_write_max_nanos, 40);
    }

    #[test]
    fn direct_capture_to_playback_duration_matches_exact_correlation_rules() {
        let timeline = HardwareTimeline::default();
        timeline.record_capture_to_playback_write_duration(0, 100);
        timeline.record_capture_to_playback_write_duration(200, 150);
        let stats = timeline.snapshot();
        assert_eq!(stats.capture_to_playback_write_nanos, 0);
        assert_eq!(stats.capture_to_playback_write_min_nanos, 0);
        assert_eq!(stats.capture_to_playback_write_max_nanos, 0);

        timeline.record_capture_to_playback_write_duration(100, 180);
        timeline.record_capture_to_playback_write_duration(200, 240);
        let stats = timeline.snapshot();
        assert_eq!(stats.capture_to_playback_write_nanos, 40);
        assert_eq!(stats.capture_to_playback_write_min_nanos, 40);
        assert_eq!(stats.capture_to_playback_write_max_nanos, 80);
    }

    #[test]
    fn duplex_pointer_phase_handles_small_and_unsorted_windows() {
        let timeline = HardwareTimeline::default();
        assert_eq!(timeline.snapshot().duplex_pointer_phase_samples, 0);

        timeline.record_duplex_pointer_phase(-42);
        let s1 = timeline.snapshot();
        assert_eq!(s1.duplex_pointer_phase_nanos, -42);
        assert_eq!(s1.duplex_pointer_phase_min_nanos, -42);
        assert_eq!(s1.duplex_pointer_phase_max_nanos, -42);
        assert_eq!(s1.duplex_pointer_phase_samples, 1);

        timeline.record_duplex_pointer_phase(100);
        timeline.record_duplex_pointer_phase(-100);
        timeline.record_duplex_pointer_phase(25);
        let s4 = timeline.snapshot();
        // Sorted: [-100, -42, 25, 100], index 4 / 2 = 2 -> 25
        assert_eq!(s4.duplex_pointer_phase_nanos, 25);
        assert_eq!(s4.duplex_pointer_phase_min_nanos, -100);
        assert_eq!(s4.duplex_pointer_phase_max_nanos, 100);
        assert_eq!(s4.duplex_pointer_phase_samples, 4);
    }

    #[test]
    fn hot_counter_groups_sit_on_distinct_cache_lines() {
        let timeline = HardwareTimeline::default();
        let line = |ptr: *const AtomicU64| (ptr as usize) / 64;

        let gen_line = line(&timeline.control.generation);
        let sample_line = line(&timeline.playback.sample_position);
        let capture_line = line(&timeline.capture.capture_position);
        let timing0_line = line(&timeline.pro_timing[0].sequence);
        let timing1_line = line(&timeline.pro_timing[1].sequence);
        let phase_line = line(&timeline.duplex_pointer_phase.sample_count);

        assert_ne!(gen_line, sample_line);
        assert_ne!(gen_line, capture_line);
        assert_ne!(sample_line, capture_line);
        assert_ne!(sample_line, timing0_line);
        assert_ne!(capture_line, timing0_line);
        assert_ne!(timing0_line, timing1_line);
        assert_ne!(phase_line, sample_line);
    }

    #[test]
    #[ignore = "microbenchmark"]
    fn benchmark_timeline_rt_period_updates() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let timeline = Arc::new(HardwareTimeline::default());
        let stop = Arc::new(AtomicBool::new(false));
        let reader_timeline = Arc::clone(&timeline);
        let reader_stop = Arc::clone(&stop);
        let reader = std::thread::spawn(move || {
            let mut sum = 0_u64;
            while !reader_stop.load(Ordering::Relaxed) {
                sum = sum.wrapping_add(reader_timeline.generation());
            }
            std::hint::black_box(sum);
        });

        let iterations = 1_000_000_u64;
        let t0 = std::time::Instant::now();
        for seq in 0..iterations {
            let t_cap = 1_000_000 + seq * 1_333_333;
            let t_wr = t_cap + 450_000;
            timeline.update_capture_position(seq * 64);
            timeline.record_pro_capture_read(seq, t_cap);
            timeline.record_pro_wait_budget(800_000);
            timeline.record_playback_write(12_000);
            timeline.update_playback_position(seq * 64);
            timeline.record_capture_to_playback_write_duration(t_cap, t_wr);
            timeline.processed_frames(64, 1);
        }
        let elapsed_ns = t0.elapsed().as_nanos() as f64 / iterations as f64;
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();
        let snap = timeline.snapshot();
        assert_eq!(snap.periods_processed, iterations);
        eprintln!(
            "timeline RT period updates (with concurrent generation() reader): {elapsed_ns:.1}ns/period"
        );
    }
}
