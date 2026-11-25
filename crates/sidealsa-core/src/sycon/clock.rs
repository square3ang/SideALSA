//! Per-packet frame counts for UAC isochronous streams, driven by explicit
//! feedback the same way snd-usb-audio does (16.16 frames per (micro)frame).

#[derive(Clone, Copy, Debug)]
pub(crate) struct PacketClock {
    nominal_q16: u32,
    current_q16: u32,
    interval_log2: u8,
    max_frames: u32,
    shift: Option<i32>,
    phase: u32,
}

impl PacketClock {
    pub(crate) fn new(rate: u32, high_speed: bool, interval_log2: u8, max_frames: u32) -> Self {
        let rate = u64::from(rate);
        let nominal_q16 = if high_speed {
            ((rate << 10) + 62) / 125
        } else {
            ((rate << 13) + 62) / 125
        } as u32;
        Self {
            nominal_q16,
            current_q16: nominal_q16,
            interval_log2,
            max_frames,
            shift: None,
            phase: 0,
        }
    }

    /// Frames the next packet carries.
    #[inline]
    pub(crate) fn next_packet_frames(&mut self) -> u32 {
        let phase =
            u64::from(self.phase & 0xffff) + (u64::from(self.current_q16) << self.interval_log2);
        self.phase = phase as u32;
        (phase >> 16).min(u64::from(self.max_frames)) as u32
    }

    /// Frames per packet in 16.16 at the current feedback rate.
    #[cfg(test)]
    fn packet_rate_q16(&self) -> u64 {
        u64::from(self.current_q16) << self.interval_log2
    }

    /// Applies one feedback packet. Returns false when the value was rejected.
    pub(crate) fn apply_feedback(&mut self, payload: &[u8]) -> bool {
        let value = match payload.len() {
            3 => u32::from_le_bytes([payload[0], payload[1], payload[2], 0]),
            4.. => u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]),
            _ => return false,
        };
        if value == 0 {
            return false;
        }
        let nominal = self.nominal_q16;
        let shift = match self.shift {
            Some(shift) => shift,
            None => {
                // Detect the device's fixed-point layout once, assuming the
                // feedback is within -25%..+50% of nominal (as snd-usb-audio).
                let mut probe = u64::from(value);
                let mut shift = 0_i32;
                while probe < u64::from(nominal - nominal / 4) && shift < 16 {
                    probe <<= 1;
                    shift += 1;
                }
                while probe > u64::from(nominal + nominal / 2) && shift > -16 {
                    probe >>= 1;
                    shift -= 1;
                }
                self.shift = Some(shift);
                shift
            }
        };
        let shifted = if shift >= 0 {
            u64::from(value) << shift as u32
        } else {
            u64::from(value) >> (-shift) as u32
        };
        if shifted >= u64::from(nominal - nominal / 8)
            && shifted <= u64::from(nominal + nominal / 2)
        {
            self.current_q16 = shifted as u32;
            true
        } else {
            self.shift = None;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nominal_high_speed_48k_is_six_frames() {
        let mut clock = PacketClock::new(48_000, true, 0, 25);
        assert!((0..800).all(|_| clock.next_packet_frames() == 6));
    }

    #[test]
    fn fractional_rates_accumulate_exactly() {
        let mut clock = PacketClock::new(44_100, true, 0, 25);
        let total: u32 = (0..8000).map(|_| clock.next_packet_frames()).sum();
        assert!((44_099..=44_101).contains(&total), "{total}");
    }

    #[test]
    fn applies_and_detects_feedback_layouts() {
        let mut clock = PacketClock::new(48_000, true, 0, 25);
        // 6.000244 frames per microframe in 16.16, as the E1x2 reports.
        assert!(clock.apply_feedback(&0x0006_0010_u32.to_le_bytes()));
        assert_eq!(clock.packet_rate_q16(), 0x0006_0010);

        // A device reporting frames per millisecond is shifted back to per microframe.
        let mut per_ms = PacketClock::new(48_000, true, 0, 25);
        assert!(per_ms.apply_feedback(&(48_u32 << 16).to_le_bytes()));
        assert_eq!(per_ms.packet_rate_q16(), 6 << 16);

        // Full speed 10.14 in three bytes.
        let mut full_speed = PacketClock::new(48_000, false, 0, 49);
        assert!(full_speed.apply_feedback(&[0x00, 0x00, 0x0c]));
        assert_eq!(full_speed.packet_rate_q16(), 48 << 16);

        assert!(!clock.apply_feedback(&[0, 0, 0, 0]));
        assert!(!clock.apply_feedback(&[1]));
    }

    #[test]
    fn clamps_to_max_packet_frames() {
        let mut clock = PacketClock::new(48_000, true, 0, 5);
        assert_eq!(clock.next_packet_frames(), 5);
    }

    #[test]
    fn rejects_high_bit_overflow_after_positive_shift() {
        let mut full_speed = PacketClock::new(48_000, false, 0, 64);
        // Lock shift = +2 using 10.14 feedback (48 << 14 = 0x000c_0000).
        assert!(full_speed.apply_feedback(&[0x00, 0x00, 0x0c]));
        assert_eq!(full_speed.packet_rate_q16(), 48 << 16);

        // In 32-bit shift, 0x400e_0000 << 2 would wrap to 0x0038_0000 (56 frames/ms).
        assert!(!full_speed.apply_feedback(&0x400e_0000_u32.to_le_bytes()));
        assert_eq!(full_speed.packet_rate_q16(), 48 << 16);
        // Subsequent valid 10.14 packet re-locks cleanly.
        assert!(full_speed.apply_feedback(&[0x00, 0x00, 0x0c]));
        assert_eq!(full_speed.packet_rate_q16(), 48 << 16);
    }

    #[test]
    fn handles_multi_microframe_intervals_without_overflow() {
        let mut hs_1ms = PacketClock::new(44_100, true, 3, 64);
        let total: u32 = (0..1000).map(|_| hs_1ms.next_packet_frames()).sum();
        assert!((44_099..=44_101).contains(&total), "{total}");

        let mut max_interval = PacketClock::new(48_000, true, 15, 25);
        assert_eq!(max_interval.next_packet_frames(), 25);
    }
}
