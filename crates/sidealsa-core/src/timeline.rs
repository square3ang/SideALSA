use std::sync::atomic::{AtomicU64, Ordering};

use crate::StreamDirection;

const PRO_TIMING_SLOT_COUNT: usize = 8;

#[derive(Debug, Default)]
struct ProTimingSlot {
    sequence: AtomicU64,
    capture_read_nanos: AtomicU64,
}

#[derive(Debug, Default)]
pub struct HardwareTimeline {
    generation: AtomicU64,
    sample_position: AtomicU64,
    playback_position: AtomicU64,
    capture_position: AtomicU64,
    playback_xruns: AtomicU64,
    capture_xruns: AtomicU64,
    playback_delay_frames: AtomicU64,
    capture_delay_frames: AtomicU64,
    playback_delay_min_frames: AtomicU64,
    playback_delay_max_frames: AtomicU64,
    playback_ring_delay_frames: AtomicU64,
    playback_ring_delay_min_frames: AtomicU64,
    playback_ring_delay_max_frames: AtomicU64,
    playback_driver_delay_frames: AtomicU64,
    playback_driver_delay_min_frames: AtomicU64,
    playback_driver_delay_max_frames: AtomicU64,
    capture_delay_min_frames: AtomicU64,
    capture_delay_max_frames: AtomicU64,
    playback_target_overshoot_max_frames: AtomicU64,
    capture_clock_wait_max_nanos: AtomicU64,
    pro_wait_budget_min_nanos: AtomicU64,
    pro_wait_budget_max_nanos: AtomicU64,
    pro_ready_wait_max_nanos: AtomicU64,
    playback_write_max_nanos: AtomicU64,
    capture_to_playback_write_nanos: AtomicU64,
    capture_to_playback_write_min_nanos: AtomicU64,
    capture_to_playback_write_max_nanos: AtomicU64,
    linked_phase_result_epoch: AtomicU64,
    linked_phase_attempts: AtomicU64,
    linked_phase_rebases: AtomicU64,
    linked_phase_score_nanos: AtomicU64,
    linked_phase_target_met: AtomicU64,
    pro_timing: [ProTimingSlot; PRO_TIMING_SLOT_COUNT],
    playback_low_watermarks: AtomicU64,
    pro_deadline_misses: AtomicU64,
    pro_client_deadline_misses: AtomicU64,
    pro_core_deadline_misses: AtomicU64,
    pro_capture_overruns: AtomicU64,
    pro_playback_blocks: AtomicU64,
    pro_playback_nonzero_blocks: AtomicU64,
    shared_underruns: AtomicU64,
    shared_overruns: AtomicU64,
    timeline_resets: AtomicU64,
    periods_processed: AtomicU64,
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
    pub capture_clock_wait_max_nanos: u64,
    pub pro_wait_budget_min_nanos: u64,
    pub pro_wait_budget_max_nanos: u64,
    pub pro_ready_wait_max_nanos: u64,
    pub playback_write_max_nanos: u64,
    pub capture_to_playback_write_nanos: u64,
    pub capture_to_playback_write_min_nanos: u64,
    pub capture_to_playback_write_max_nanos: u64,
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
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> HardwareStats {
        let (linked_phase_attempts, linked_phase_score_nanos, linked_phase_target_met) =
            self.linked_phase_calibration();
        HardwareStats {
            generation: self.generation.load(Ordering::Relaxed),
            sample_position: self.sample_position.load(Ordering::Relaxed),
            playback_position: self.playback_position.load(Ordering::Relaxed),
            capture_position: self.capture_position.load(Ordering::Relaxed),
            hw_playback_xruns: self.playback_xruns.load(Ordering::Relaxed),
            hw_capture_xruns: self.capture_xruns.load(Ordering::Relaxed),
            playback_delay_frames: self.playback_delay_frames.load(Ordering::Relaxed),
            capture_delay_frames: self.capture_delay_frames.load(Ordering::Relaxed),
            playback_delay_min_frames: decode_minimum(
                self.playback_delay_min_frames.load(Ordering::Relaxed),
            ),
            playback_delay_max_frames: self.playback_delay_max_frames.load(Ordering::Relaxed),
            playback_ring_delay_frames: self.playback_ring_delay_frames.load(Ordering::Relaxed),
            playback_ring_delay_min_frames: decode_minimum(
                self.playback_ring_delay_min_frames.load(Ordering::Relaxed),
            ),
            playback_ring_delay_max_frames: self
                .playback_ring_delay_max_frames
                .load(Ordering::Relaxed),
            playback_driver_delay_frames: self.playback_driver_delay_frames.load(Ordering::Relaxed),
            playback_driver_delay_min_frames: decode_minimum(
                self.playback_driver_delay_min_frames
                    .load(Ordering::Relaxed),
            ),
            playback_driver_delay_max_frames: self
                .playback_driver_delay_max_frames
                .load(Ordering::Relaxed),
            capture_delay_min_frames: decode_minimum(
                self.capture_delay_min_frames.load(Ordering::Relaxed),
            ),
            capture_delay_max_frames: self.capture_delay_max_frames.load(Ordering::Relaxed),
            playback_target_overshoot_max_frames: self
                .playback_target_overshoot_max_frames
                .load(Ordering::Relaxed),
            capture_clock_wait_max_nanos: self.capture_clock_wait_max_nanos.load(Ordering::Relaxed),
            pro_wait_budget_min_nanos: decode_minimum(
                self.pro_wait_budget_min_nanos.load(Ordering::Relaxed),
            ),
            pro_wait_budget_max_nanos: self.pro_wait_budget_max_nanos.load(Ordering::Relaxed),
            pro_ready_wait_max_nanos: self.pro_ready_wait_max_nanos.load(Ordering::Relaxed),
            playback_write_max_nanos: self.playback_write_max_nanos.load(Ordering::Relaxed),
            capture_to_playback_write_nanos: self
                .capture_to_playback_write_nanos
                .load(Ordering::Relaxed),
            capture_to_playback_write_min_nanos: decode_minimum(
                self.capture_to_playback_write_min_nanos
                    .load(Ordering::Relaxed),
            ),
            capture_to_playback_write_max_nanos: self
                .capture_to_playback_write_max_nanos
                .load(Ordering::Relaxed),
            linked_phase_attempts,
            linked_phase_rebases: self.linked_phase_rebases.load(Ordering::Relaxed),
            linked_phase_score_nanos,
            linked_phase_target_met,
            playback_low_watermarks: self.playback_low_watermarks.load(Ordering::Relaxed),
            pro_deadline_misses: self.pro_deadline_misses.load(Ordering::Relaxed),
            pro_client_deadline_misses: self.pro_client_deadline_misses.load(Ordering::Relaxed),
            pro_core_deadline_misses: self.pro_core_deadline_misses.load(Ordering::Relaxed),
            pro_capture_overruns: self.pro_capture_overruns.load(Ordering::Relaxed),
            pro_playback_blocks: self.pro_playback_blocks.load(Ordering::Relaxed),
            pro_playback_nonzero_blocks: self.pro_playback_nonzero_blocks.load(Ordering::Relaxed),
            shared_underruns: self.shared_underruns.load(Ordering::Relaxed),
            shared_overruns: self.shared_overruns.load(Ordering::Relaxed),
            timeline_resets: self.timeline_resets.load(Ordering::Relaxed),
            periods_processed: self.periods_processed.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn update_playback_position(&self, playback_position: u64) {
        self.playback_position
            .store(playback_position, Ordering::Relaxed);
    }

    pub(crate) fn update_capture_position(&self, capture_position: u64) {
        self.capture_position
            .store(capture_position, Ordering::Relaxed);
    }

    pub(crate) fn processed_frames(&self, frames: u64, periods: u64) {
        self.sample_position.fetch_add(frames, Ordering::Relaxed);
        self.periods_processed.fetch_add(periods, Ordering::Relaxed);
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
                self.playback_delay_frames
                    .store(delay_frames, Ordering::Relaxed);
                update_minimum(&self.playback_delay_min_frames, delay_frames);
                update_maximum(&self.playback_delay_max_frames, delay_frames);
                if delay_frames < period_frames {
                    self.playback_low_watermarks.fetch_add(1, Ordering::Relaxed);
                }
            }
            StreamDirection::Capture => {
                self.capture_delay_frames
                    .store(delay_frames, Ordering::Relaxed);
                update_minimum(&self.capture_delay_min_frames, delay_frames);
                update_maximum(&self.capture_delay_max_frames, delay_frames);
            }
        }
    }

    pub(crate) fn record_playback_target_overshoot(&self, frames: u64) {
        update_maximum(&self.playback_target_overshoot_max_frames, frames);
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

        self.playback_ring_delay_frames
            .store(ring_delay, Ordering::Relaxed);
        update_minimum(&self.playback_ring_delay_min_frames, ring_delay);
        update_maximum(&self.playback_ring_delay_max_frames, ring_delay);
        self.playback_driver_delay_frames
            .store(driver_delay, Ordering::Relaxed);
        update_minimum(&self.playback_driver_delay_min_frames, driver_delay);
        update_maximum(&self.playback_driver_delay_max_frames, driver_delay);
    }

    pub(crate) fn record_pro_wait_budget(&self, nanos: u64) {
        update_minimum(&self.pro_wait_budget_min_nanos, nanos);
        update_maximum(&self.pro_wait_budget_max_nanos, nanos);
    }

    pub fn record_pro_ready_wait(&self, nanos: u64) {
        update_maximum(&self.pro_ready_wait_max_nanos, nanos);
    }

    pub(crate) fn record_playback_write(&self, nanos: u64) {
        update_maximum(&self.playback_write_max_nanos, nanos);
    }

    pub(crate) fn record_linked_phase_calibration(
        &self,
        attempts: u64,
        score_nanos: u64,
        target_met: bool,
    ) {
        self.linked_phase_result_epoch
            .fetch_add(1, Ordering::AcqRel);
        self.linked_phase_attempts
            .store(attempts, Ordering::Relaxed);
        self.linked_phase_score_nanos
            .store(score_nanos, Ordering::Relaxed);
        self.linked_phase_target_met
            .store(u64::from(target_met), Ordering::Relaxed);
        self.linked_phase_result_epoch
            .fetch_add(1, Ordering::Release);
    }

    fn linked_phase_calibration(&self) -> (u64, u64, bool) {
        loop {
            let start = self.linked_phase_result_epoch.load(Ordering::Acquire);
            if !start.is_multiple_of(2) {
                std::hint::spin_loop();
                continue;
            }
            let attempts = self.linked_phase_attempts.load(Ordering::Relaxed);
            let score_nanos = self.linked_phase_score_nanos.load(Ordering::Relaxed);
            let target_met = self.linked_phase_target_met.load(Ordering::Relaxed) != 0;
            if start == self.linked_phase_result_epoch.load(Ordering::Acquire) {
                return (attempts, score_nanos, target_met);
            }
        }
    }

    pub(crate) fn record_pro_capture_read(&self, sequence: u64, nanos: u64) {
        let slot = &self.pro_timing[sequence as usize % PRO_TIMING_SLOT_COUNT];
        slot.capture_read_nanos.store(nanos, Ordering::Relaxed);
        slot.sequence.store(sequence, Ordering::Release);
    }

    pub(crate) fn record_pro_playback_write(&self, sequence: u64, nanos: u64) {
        let slot = &self.pro_timing[sequence as usize % PRO_TIMING_SLOT_COUNT];
        if slot.sequence.load(Ordering::Acquire) != sequence {
            return;
        }
        let capture_read_nanos = slot.capture_read_nanos.load(Ordering::Relaxed);
        if capture_read_nanos == 0 || nanos < capture_read_nanos {
            return;
        }
        let duration = nanos - capture_read_nanos;
        self.capture_to_playback_write_nanos
            .store(duration, Ordering::Relaxed);
        update_minimum(&self.capture_to_playback_write_min_nanos, duration);
        update_maximum(&self.capture_to_playback_write_max_nanos, duration);
    }

    pub fn record_pro_deadline_miss(&self) {
        self.pro_deadline_misses.fetch_add(1, Ordering::Relaxed);
        self.pro_client_deadline_misses
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_pro_core_deadline_miss(&self) {
        self.pro_deadline_misses.fetch_add(1, Ordering::Relaxed);
        self.pro_core_deadline_misses
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_pro_capture_overrun(&self) {
        self.pro_capture_overruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_pro_playback_block(&self, nonzero: bool) {
        self.pro_playback_blocks.fetch_add(1, Ordering::Relaxed);
        if nonzero {
            self.pro_playback_nonzero_blocks
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_shared_underrun(&self) {
        self.shared_underruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_shared_overrun(&self) {
        self.shared_overruns.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_hardware_xrun(&self, direction: StreamDirection) {
        match direction {
            StreamDirection::Playback => {
                self.playback_xruns.fetch_add(1, Ordering::Relaxed);
            }
            StreamDirection::Capture => {
                self.capture_xruns.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub(crate) fn reset_after_hardware_xrun(&self) {
        self.reset_after_hardware_restart();
    }

    pub(crate) fn reset_after_hardware_restart(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.timeline_resets.fetch_add(1, Ordering::Relaxed);
        self.record_linked_phase_calibration(0, 0, false);
    }

    pub(crate) fn reset_after_hardware_rebase(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.timeline_resets.fetch_add(1, Ordering::Relaxed);
        self.linked_phase_rebases.fetch_add(1, Ordering::Relaxed);
    }
}

fn update_minimum(target: &AtomicU64, value: u64) {
    let encoded = value.saturating_add(1);
    let mut current = target.load(Ordering::Relaxed);
    while current == 0 || encoded < current {
        match target.compare_exchange_weak(current, encoded, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
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
        timeline.reset_after_hardware_xrun();
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

        timeline.reset_after_hardware_xrun();

        let stats = timeline.snapshot();
        assert_eq!(stats.linked_phase_attempts, 0);
        assert_eq!(stats.linked_phase_score_nanos, 0);
        assert!(!stats.linked_phase_target_met);
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
        assert_eq!(stats.capture_clock_wait_max_nanos, 0);
        assert_eq!(stats.pro_wait_budget_min_nanos, 0);
        assert_eq!(stats.pro_wait_budget_max_nanos, 50);
        assert_eq!(stats.pro_ready_wait_max_nanos, 30);
        assert_eq!(stats.playback_write_max_nanos, 40);
    }
}
