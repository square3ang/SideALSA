use std::sync::atomic::{AtomicU64, Ordering};

use crate::StreamDirection;

#[derive(Debug, Default)]
pub struct HardwareTimeline {
    generation: AtomicU64,
    sample_position: AtomicU64,
    playback_position: AtomicU64,
    capture_position: AtomicU64,
    playback_xruns: AtomicU64,
    capture_xruns: AtomicU64,
    pro_deadline_misses: AtomicU64,
    pro_client_deadline_misses: AtomicU64,
    pro_core_deadline_misses: AtomicU64,
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
    pub pro_deadline_misses: u64,
    pub pro_client_deadline_misses: u64,
    pub pro_core_deadline_misses: u64,
    pub shared_underruns: u64,
    pub shared_overruns: u64,
    pub timeline_resets: u64,
    pub periods_processed: u64,
}

impl HardwareTimeline {
    pub fn snapshot(&self) -> HardwareStats {
        HardwareStats {
            generation: self.generation.load(Ordering::Relaxed),
            sample_position: self.sample_position.load(Ordering::Relaxed),
            playback_position: self.playback_position.load(Ordering::Relaxed),
            capture_position: self.capture_position.load(Ordering::Relaxed),
            hw_playback_xruns: self.playback_xruns.load(Ordering::Relaxed),
            hw_capture_xruns: self.capture_xruns.load(Ordering::Relaxed),
            pro_deadline_misses: self.pro_deadline_misses.load(Ordering::Relaxed),
            pro_client_deadline_misses: self.pro_client_deadline_misses.load(Ordering::Relaxed),
            pro_core_deadline_misses: self.pro_core_deadline_misses.load(Ordering::Relaxed),
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

    pub(crate) fn periods_processed(&self) -> u64 {
        self.periods_processed.load(Ordering::Relaxed)
    }

    pub(crate) fn record_pro_core_deadline_miss(&self) {
        self.pro_core_deadline_misses
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_pro_deadline_miss(&self) {
        self.pro_deadline_misses.fetch_add(1, Ordering::Relaxed);
        self.pro_client_deadline_misses
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_shared_underrun(&self) {
        self.shared_underruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_shared_overrun(&self) {
        self.shared_overruns.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn hardware_xrun(&self, direction: StreamDirection) {
        match direction {
            StreamDirection::Playback => {
                self.playback_xruns.fetch_add(1, Ordering::Relaxed);
            }
            StreamDirection::Capture => {
                self.capture_xruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.timeline_resets.fetch_add(1, Ordering::Relaxed);
    }
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
        timeline.hardware_xrun(StreamDirection::Capture);
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
    fn core_pro_miss_is_separate_from_client_miss() {
        let timeline = HardwareTimeline::default();

        timeline.record_pro_core_deadline_miss();

        let stats = timeline.snapshot();
        assert_eq!(stats.pro_deadline_misses, 0);
        assert_eq!(stats.pro_client_deadline_misses, 0);
        assert_eq!(stats.pro_core_deadline_misses, 1);
    }
}
