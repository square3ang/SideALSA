use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProMiss {
    pub generation: u64,
    pub session_id: u64,
    pub sequence: u64,
    pub wait_entry_budget_nanos: Option<u64>,
    pub cutoff_nanos: Option<u64>,
    pub selection_started_nanos: u64,
    pub published_nanos: Option<u64>,
    pub capture_to_publish_nanos: Option<u64>,
    pub core: bool,
    pub late: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProDiagnosticsSnapshot {
    pub misses_recorded: u64,
    pub accepted_roundtrip_samples: u64,
    pub accepted_roundtrip_max_nanos: u64,
    pub last_miss: Option<ProMiss>,
}

#[derive(Default)]
pub(crate) struct ProDiagnostics {
    pub enabled: AtomicBool,
    version: AtomicU64,
    fields: [AtomicU64; 10],
    accepted_roundtrip_samples: AtomicU64,
    accepted_roundtrip_max_nanos: AtomicU64,
}

impl ProDiagnostics {
    pub fn record_accepted(&self, elapsed: u64) {
        self.accepted_roundtrip_max_nanos
            .fetch_max(elapsed, Ordering::Relaxed);
        self.accepted_roundtrip_samples
            .fetch_add(1, Ordering::Relaxed);
    }

    // Single writer: the daemon's sole playback bridge. SeqCst atomics give
    // bounded readers a coherent record without locks or non-atomic data races.
    pub fn record_miss(&self, miss: ProMiss) {
        let values = [
            miss.generation,
            miss.session_id,
            miss.sequence,
            miss.wait_entry_budget_nanos.unwrap_or(0),
            miss.cutoff_nanos.unwrap_or(0),
            miss.selection_started_nanos,
            miss.published_nanos.unwrap_or(0),
            miss.capture_to_publish_nanos.unwrap_or(0),
            u64::from(miss.core),
            u64::from(miss.late)
                | (u64::from(miss.wait_entry_budget_nanos.is_some()) << 1)
                | (u64::from(miss.cutoff_nanos.is_some()) << 2)
                | (u64::from(miss.published_nanos.is_some()) << 3)
                | (u64::from(miss.capture_to_publish_nanos.is_some()) << 4),
        ];
        self.version.fetch_add(1, Ordering::SeqCst);
        for (field, value) in self.fields.iter().zip(values) {
            field.store(value, Ordering::SeqCst);
        }
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    pub fn snapshot(&self) -> Option<ProDiagnosticsSnapshot> {
        for _ in 0..3 {
            let version = self.version.load(Ordering::SeqCst);
            if !version.is_multiple_of(2) {
                continue;
            }
            let fields = self
                .fields
                .each_ref()
                .map(|field| field.load(Ordering::SeqCst));
            if self.version.load(Ordering::SeqCst) != version {
                continue;
            }
            let optional =
                |value: u64, bit: u32| (fields[9] & (1_u64 << bit) != 0).then_some(value);
            return Some(ProDiagnosticsSnapshot {
                misses_recorded: version / 2,
                accepted_roundtrip_samples: self.accepted_roundtrip_samples.load(Ordering::Relaxed),
                accepted_roundtrip_max_nanos: self
                    .accepted_roundtrip_max_nanos
                    .load(Ordering::Relaxed),
                last_miss: (version != 0).then_some(ProMiss {
                    generation: fields[0],
                    session_id: fields[1],
                    sequence: fields[2],
                    wait_entry_budget_nanos: optional(fields[3], 1),
                    cutoff_nanos: optional(fields[4], 2),
                    selection_started_nanos: fields[5],
                    published_nanos: optional(fields[6], 3),
                    capture_to_publish_nanos: optional(fields[7], 4),
                    core: fields[8] != 0,
                    late: fields[9] & 1 != 0,
                }),
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_preserves_zero_budget_and_unknown_publication() {
        let diagnostics = ProDiagnostics::default();
        assert!(diagnostics.snapshot().unwrap().last_miss.is_none());
        let miss = ProMiss {
            sequence: 42,
            wait_entry_budget_nanos: Some(0),
            core: true,
            ..ProMiss::default()
        };
        diagnostics.record_miss(miss);
        let snapshot = diagnostics.snapshot().unwrap();
        assert_eq!(snapshot.misses_recorded, 1);
        assert_eq!(snapshot.last_miss, Some(miss));
        diagnostics.record_accepted(123);
        diagnostics.record_accepted(100);
        let snapshot = diagnostics.snapshot().unwrap();
        assert_eq!(snapshot.accepted_roundtrip_samples, 2);
        assert_eq!(snapshot.accepted_roundtrip_max_nanos, 123);
    }

    #[test]
    fn snapshot_preserves_the_full_optional_timestamp_range() {
        let diagnostics = ProDiagnostics::default();
        for value in [Some(u64::MAX), Some(0), None] {
            let miss = ProMiss {
                wait_entry_budget_nanos: value,
                cutoff_nanos: value,
                published_nanos: value,
                capture_to_publish_nanos: value,
                ..ProMiss::default()
            };
            diagnostics.record_miss(miss);
            assert_eq!(diagnostics.snapshot().unwrap().last_miss, Some(miss));
        }
    }

    #[test]
    fn concurrent_snapshots_do_not_mix_misses() {
        let diagnostics = std::sync::Arc::new(ProDiagnostics::default());
        let writer = std::sync::Arc::clone(&diagnostics);
        let handle = std::thread::spawn(move || {
            for sequence in 1..=10_000 {
                writer.record_miss(ProMiss {
                    sequence,
                    session_id: sequence,
                    published_nanos: Some(sequence),
                    ..ProMiss::default()
                });
            }
        });
        while !handle.is_finished() {
            if let Some(snapshot) = diagnostics.snapshot()
                && let Some(miss) = snapshot.last_miss
            {
                assert_eq!(miss.sequence, miss.session_id);
                assert_eq!(miss.published_nanos, Some(miss.sequence));
                assert_eq!(snapshot.misses_recorded, miss.sequence);
            }
        }
        handle.join().unwrap();
        assert_eq!(diagnostics.snapshot().unwrap().misses_recorded, 10_000);
    }
}
