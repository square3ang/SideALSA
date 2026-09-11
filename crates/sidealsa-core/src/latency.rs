//! Detect retained direct-duplex buffering after a hardware-worker stall.
//! Fixed storage only; client deadline misses are deliberately not inputs.

const WINDOW: usize = 9;
const WARMUP_NANOS: u64 = 1_000_000_000;
const STALL_WINDOW_NANOS: u64 = 10_000_000_000;

pub(crate) struct LatencyGuard {
    period_frames: i64,
    period_nanos: u64,
    sample_periods: u64,
    stall_window_nanos: u64,
    generation: Option<u64>,
    warm_until: u64,
    last_ready: Option<u64>,
    stalled_until: u64,
    samples: [i64; WINDOW],
    count: usize,
    baseline: Option<i64>,
    high_windows: u8,
}

impl LatencyGuard {
    pub(crate) fn new(period_frames: i64, rate: u32) -> Self {
        let period_nanos = period_frames as u64 * 1_000_000_000 / u64::from(rate);
        let sample_periods = (u64::from(rate) / period_frames as u64 / 12)
            .max(1)
            .next_power_of_two();
        Self {
            period_frames,
            period_nanos,
            sample_periods,
            stall_window_nanos: STALL_WINDOW_NANOS.max(
                period_nanos
                    .saturating_mul(sample_periods)
                    .saturating_mul(WINDOW as u64 * 3),
            ),
            generation: None,
            warm_until: 0,
            last_ready: None,
            stalled_until: 0,
            samples: [0; WINDOW],
            count: 0,
            baseline: None,
            high_windows: 0,
        }
    }

    pub(crate) fn sample_due(&self, sequence: u64) -> bool {
        sequence.is_multiple_of(self.sample_periods)
    }

    pub(crate) fn ready(&mut self, generation: u64, now: u64) {
        if self.generation != Some(generation) {
            self.generation = Some(generation);
            self.warm_until = now.saturating_add(WARMUP_NANOS);
            self.last_ready = None;
            self.stalled_until = 0;
            self.baseline = None;
            self.count = 0;
            self.high_windows = 0;
        }
        if self
            .last_ready
            .is_some_and(|last| now.saturating_sub(last) > self.period_nanos.saturating_mul(2))
        {
            self.stalled_until = now.saturating_add(self.stall_window_nanos);
        }
        self.last_ready = Some(now);
    }

    pub(crate) fn completed(&mut self, ready: u64, now: u64) {
        if now.saturating_sub(ready) > self.period_nanos {
            self.stalled_until = now.saturating_add(self.stall_window_nanos);
        }
    }

    pub(crate) fn sample(&mut self, now: u64, playback: i64, capture: i64) -> bool {
        if now < self.warm_until || playback < 0 || capture < 0 {
            return false;
        }
        self.samples[self.count] = playback.saturating_add(capture);
        self.count += 1;
        if self.count != WINDOW {
            return false;
        }
        self.count = 0;
        self.samples.sort_unstable();
        let median = self.samples[WINDOW / 2];
        let Some(baseline) = self.baseline else {
            self.baseline = Some(median);
            return false;
        };
        if now < self.stalled_until && median.saturating_sub(baseline) >= self.period_frames {
            self.high_windows = self.high_windows.saturating_add(1);
        } else {
            self.high_windows = 0;
        }
        self.high_windows >= 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn window(g: &mut LatencyGuard, time: u64, total: i64) -> bool {
        let mut trigger = false;
        for _ in 0..WINDOW {
            trigger = g.sample(time, total, 0);
        }
        trigger
    }
    fn primed() -> LatencyGuard {
        let mut g = LatencyGuard::new(64, 48000);
        g.ready(0, 0);
        assert!(!window(&mut g, WARMUP_NANOS, 200));
        g
    }
    #[test]
    fn persistent_extra_period_after_stall_requires_two_windows() {
        let mut g = primed();
        g.completed(1_000_000_000, 1_003_500_000);
        assert!(!window(&mut g, 1_100_000_000, 315));
        assert!(window(&mut g, 1_200_000_000, 315));
    }
    #[test]
    fn small_phase_changes_transients_and_unrelated_queue_changes_do_not_rebase() {
        let mut g = primed();
        assert!(!window(&mut g, 1_100_000_000, 315));
        assert!(!window(&mut g, 1_200_000_000, 315)); // No HW stall.
        g.completed(2_000_000_000, 2_003_500_000);
        assert!(!window(&mut g, 2_100_000_000, 225));
        assert!(!window(&mut g, 2_200_000_000, 225));
        assert!(!window(&mut g, 2_300_000_000, 315));
        assert!(!window(&mut g, 2_400_000_000, 200));
        assert!(!window(&mut g, 13_000_000_000, 315)); // Evidence expired.
    }
    #[test]
    fn generation_change_relearns_baseline_and_normal_callback_budget_is_not_stall() {
        let mut g = primed();
        g.completed(1_000_000_000, 1_001_100_000);
        assert!(!window(&mut g, 1_100_000_000, 315));
        assert!(!window(&mut g, 1_200_000_000, 315));
        g.completed(2_000_000_000, 2_004_000_000);
        g.ready(1, 3_000_000_000);
        assert!(!window(&mut g, 3_100_000_000, 400)); // Startup ignored.
        assert!(!window(&mut g, 4_000_000_000, 210));
        assert_eq!(g.baseline, Some(210));
    }

    #[test]
    fn sampling_scales_with_period_and_rejects_single_outliers() {
        for (period, stride) in [(64, 64), (256, 16), (1024, 4)] {
            let g = LatencyGuard::new(period, 48000);
            assert!(g.sample_due(stride));
            assert!(!g.sample_due(stride / 2));
        }
        let mut g = primed();
        g.completed(1_000_000_000, 1_004_000_000);
        for _ in 0..3 {
            assert!(!g.sample(2_000_000_000, 1000, 0));
            for _ in 1..WINDOW {
                assert!(!g.sample(2_000_000_000, 200, 0));
            }
        }
    }
}
