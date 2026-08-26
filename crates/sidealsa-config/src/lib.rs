use std::{collections::HashSet, fs, path::Path};

use serde::Deserialize;
use thiserror::Error;

pub const MAX_PRO_LATENCY_PERIODS: u32 = 7;
pub const MAX_SHARED_LATENCY_PERIODS: u32 = 7;
pub const MAX_SHARED_BUFFER_PERIODS: u32 = 8;
pub const MAX_REALTIME_PRIORITY: u32 = 99;
pub const MAX_LINKED_PHASE_ATTEMPTS: u32 = 64;
pub const DEFAULT_PRO_HANDOFF_US: u32 = 250;
pub const LINKED_PHASE_OVERHEAD_DIVISOR: u32 = 8;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub device: HardwareConfig,
    #[serde(default)]
    pub ports: PortsConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HardwareConfig {
    pub name: String,
    pub rate: u32,
    pub period_size: u32,
    #[serde(default)]
    pub hardware_period_size: Option<u32>,
    pub buffer_size: u32,
    #[serde(default)]
    pub shared_buffer_size: Option<u32>,
    #[serde(default)]
    pub playback_queue_periods: Option<u32>,
    #[serde(default)]
    pub playback_timer_scheduling: bool,
    #[serde(default)]
    pub duplex_link: Option<bool>,
    #[serde(default)]
    pub linked_playback_guard_frames: Option<u32>,
    #[serde(default)]
    pub linked_phase_max_attempts: u32,
    #[serde(default = "default_pro_latency_periods")]
    pub pro_latency_periods: u32,
    #[serde(default = "default_pro_handoff_us")]
    pub pro_handoff_us: u32,
    #[serde(default)]
    pub pro_realtime_priority: Option<u32>,
    #[serde(default = "default_shared_latency_periods")]
    pub shared_latency_periods: u32,
    #[serde(default = "default_realtime")]
    pub realtime: bool,
    #[serde(default = "default_realtime_priority")]
    pub realtime_priority: u32,
    pub playback: PcmConfig,
    pub capture: PcmConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PcmConfig {
    pub device: String,
    pub channels: u32,
    pub format: SampleFormat,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PortsConfig {
    #[serde(default)]
    pub playback: Vec<PortConfig>,
    #[serde(default)]
    pub capture: Vec<PortConfig>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PortConfig {
    pub id: String,
    pub name: String,
    pub channels: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub enum SampleFormat {
    #[serde(rename = "S32_LE")]
    S32Le,
}

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("could not read profile: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not parse profile: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid profile: {0}")]
    Invalid(String),
}

impl Profile {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ProfileError> {
        let text = fs::read_to_string(path)?;
        Self::from_toml(&text)
    }

    pub fn from_toml(text: &str) -> Result<Self, ProfileError> {
        let profile: Self = toml::from_str(text)?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<(), ProfileError> {
        self.device.validate()?;

        let mut ids = HashSet::new();
        validate_ports(
            "playback",
            &self.ports.playback,
            self.device.playback.channels,
            &mut ids,
        )?;
        validate_ports(
            "capture",
            &self.ports.capture,
            self.device.capture.channels,
            &mut ids,
        )?;
        Ok(())
    }
}

impl HardwareConfig {
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.name.trim().is_empty() {
            return Err(ProfileError::Invalid("device name is empty".into()));
        }
        if self.rate == 0 {
            return Err(ProfileError::Invalid("rate must be non-zero".into()));
        }
        if self.period_size == 0 {
            return Err(ProfileError::Invalid("period_size must be non-zero".into()));
        }
        let hardware_period_size = self.effective_hardware_period_size();
        if hardware_period_size == 0 {
            return Err(ProfileError::Invalid(
                "hardware_period_size must be non-zero".into(),
            ));
        }
        if hardware_period_size > self.period_size
            || !self.period_size.is_multiple_of(hardware_period_size)
        {
            return Err(ProfileError::Invalid(
                "hardware_period_size must divide period_size".into(),
            ));
        }
        if self.buffer_size == 0 {
            return Err(ProfileError::Invalid("buffer_size must be non-zero".into()));
        }
        if self.buffer_size < self.period_size {
            return Err(ProfileError::Invalid(
                "buffer_size must not be smaller than period_size".into(),
            ));
        }
        if let Some(shared_buffer_size) = self.shared_buffer_size {
            if shared_buffer_size < self.period_size.saturating_mul(2) {
                return Err(ProfileError::Invalid(
                    "shared_buffer_size must contain at least two periods".into(),
                ));
            }
            if !shared_buffer_size.is_multiple_of(self.period_size) {
                return Err(ProfileError::Invalid(
                    "shared_buffer_size must be a multiple of period_size".into(),
                ));
            }
            if shared_buffer_size > self.period_size.saturating_mul(MAX_SHARED_BUFFER_PERIODS) {
                return Err(ProfileError::Invalid(format!(
                    "shared_buffer_size must contain at most {MAX_SHARED_BUFFER_PERIODS} periods"
                )));
            }
        }
        if let Some(periods) = self.playback_queue_periods {
            if periods == 0 {
                return Err(ProfileError::Invalid(
                    "playback_queue_periods must be non-zero".into(),
                ));
            }
            let queue_frames = self.period_size.checked_mul(periods).ok_or_else(|| {
                ProfileError::Invalid("playback_queue_periods is too large".into())
            })?;
            if queue_frames > self.buffer_size {
                return Err(ProfileError::Invalid(
                    "playback_queue_periods must fit within buffer_size".into(),
                ));
            }
        }
        if let Some(guard_frames) = self.linked_playback_guard_frames {
            if !self.effective_duplex_link() {
                return Err(ProfileError::Invalid(
                    "linked_playback_guard_frames requires duplex_link = true".into(),
                ));
            }
            if self.pro_latency_periods > 1 {
                return Err(ProfileError::Invalid(
                    "linked_playback_guard_frames requires pro_latency_periods <= 1".into(),
                ));
            }
            if self.pro_latency_periods == 1
                && self.buffer_size < self.period_size.saturating_add(hardware_period_size)
            {
                return Err(ProfileError::Invalid(
                    "linked_playback_guard_frames with one-period PRO lead requires buffer_size >= period_size + hardware_period_size"
                        .into(),
                ));
            }
            if guard_frames < hardware_period_size {
                return Err(ProfileError::Invalid(
                    "linked_playback_guard_frames must be at least hardware_period_size".into(),
                ));
            }
            let write_frames = if self.pro_latency_periods == 0 {
                self.period_size
            } else {
                hardware_period_size
            };
            if guard_frames > self.buffer_size.saturating_sub(write_frames) {
                return Err(ProfileError::Invalid(
                    "linked_playback_guard_frames must leave one linked write writable".into(),
                ));
            }
            if self.pro_latency_periods == 0
                && hardware_period_size < self.period_size
                && guard_frames >= self.period_size
            {
                return Err(ProfileError::Invalid(
                    "zero-lead linked_playback_guard_frames must be below period_size for split physical periods".into(),
                ));
            }
        }
        if self.pro_latency_periods > MAX_PRO_LATENCY_PERIODS {
            return Err(ProfileError::Invalid(format!(
                "pro_latency_periods must be <= {MAX_PRO_LATENCY_PERIODS}"
            )));
        }
        if self.pro_handoff_us == 0 {
            return Err(ProfileError::Invalid(
                "pro_handoff_us must be non-zero".into(),
            ));
        }
        let handoff_frames = self
            .pro_handoff_nanos()
            .saturating_mul(u64::from(self.rate))
            .div_ceil(1_000_000_000);
        if self.effective_duplex_link() && self.pro_latency_periods <= 1 {
            let overhead_frames = (hardware_period_size / LINKED_PHASE_OVERHEAD_DIVISOR).max(1);
            let hardware_period_nanos = u64::from(hardware_period_size)
                .saturating_mul(1_000_000_000)
                / u64::from(self.rate);
            let overhead_nanos =
                u64::from(overhead_frames).saturating_mul(1_000_000_000) / u64::from(self.rate);
            if self.pro_handoff_nanos().saturating_add(overhead_nanos) > hardware_period_nanos {
                return Err(ProfileError::Invalid(
                    "pro_handoff_us leaves no physical-period write reserve".into(),
                ));
            }
        }
        if self.pro_latency_periods == 0 {
            if !self.effective_duplex_link() {
                return Err(ProfileError::Invalid(
                    "pro_latency_periods = 0 requires duplex_link = true".into(),
                ));
            }
            if !self.playback_timer_scheduling {
                return Err(ProfileError::Invalid(
                    "pro_latency_periods = 0 requires playback_timer_scheduling = true".into(),
                ));
            }
            if self.buffer_size < self.period_size.saturating_mul(2) {
                return Err(ProfileError::Invalid(
                    "pro_latency_periods = 0 requires at least two logical periods".into(),
                ));
            }
            let safety_frames = handoff_frames
                .div_ceil(u64::from(hardware_period_size))
                .saturating_mul(u64::from(hardware_period_size));
            let required_buffer = u64::from(self.period_size)
                .saturating_add(u64::from(hardware_period_size))
                .saturating_add(safety_frames);
            if u64::from(self.buffer_size) < required_buffer {
                return Err(ProfileError::Invalid(format!(
                    "pro_latency_periods = 0 requires buffer_size >= {required_buffer} for linked handoff"
                )));
            }
        }
        if self.linked_phase_max_attempts > MAX_LINKED_PHASE_ATTEMPTS {
            return Err(ProfileError::Invalid(format!(
                "linked_phase_max_attempts must be <= {MAX_LINKED_PHASE_ATTEMPTS}"
            )));
        }
        if self.linked_phase_max_attempts > 0
            && (!self.effective_duplex_link() || self.pro_latency_periods != 0)
        {
            return Err(ProfileError::Invalid(
                "linked_phase_max_attempts requires linked zero-lead PRO".into(),
            ));
        }
        if self.linked_phase_max_attempts > 0 {
            let target_nanos =
                u128::from(hardware_period_size) * 1_000_000_000 / (u128::from(self.rate) * 2);
            let overhead_frames = (hardware_period_size / LINKED_PHASE_OVERHEAD_DIVISOR).max(1);
            let required_nanos = u128::from(self.pro_handoff_nanos())
                + u128::from(overhead_frames) * 1_000_000_000 / u128::from(self.rate);
            if target_nanos < required_nanos {
                return Err(ProfileError::Invalid(
                    "linked phase target is too short for PRO handoff and overhead".into(),
                ));
            }
        }
        if self.shared_latency_periods > MAX_SHARED_LATENCY_PERIODS {
            return Err(ProfileError::Invalid(format!(
                "shared_latency_periods must be <= {MAX_SHARED_LATENCY_PERIODS}"
            )));
        }
        if self.realtime_priority == 0 || self.realtime_priority > MAX_REALTIME_PRIORITY {
            return Err(ProfileError::Invalid(format!(
                "realtime_priority must be between 1 and {MAX_REALTIME_PRIORITY}"
            )));
        }
        if let Some(priority) = self.pro_realtime_priority {
            if priority == 0 || priority > MAX_REALTIME_PRIORITY {
                return Err(ProfileError::Invalid(format!(
                    "pro_realtime_priority must be between 1 and {MAX_REALTIME_PRIORITY}"
                )));
            }
            if self.realtime && priority > self.realtime_priority.saturating_sub(2) {
                return Err(ProfileError::Invalid(
                    "pro_realtime_priority must be at least two below realtime_priority".into(),
                ));
            }
        }
        self.playback.validate("playback")?;
        self.capture.validate("capture")?;
        Ok(())
    }

    pub fn effective_pro_realtime_priority(&self) -> u32 {
        if !self.realtime || self.realtime_priority <= 2 {
            0
        } else {
            self.pro_realtime_priority
                .unwrap_or(self.realtime_priority - 2)
        }
    }

    pub fn pro_handoff_nanos(&self) -> u64 {
        u64::from(self.pro_handoff_us).saturating_mul(1_000)
    }

    pub fn effective_duplex_link(&self) -> bool {
        self.duplex_link
            .unwrap_or_else(|| self.playback.device == self.capture.device)
    }

    pub fn effective_hardware_period_size(&self) -> u32 {
        self.hardware_period_size.unwrap_or(self.period_size)
    }

    pub fn effective_linked_playback_guard_frames(&self) -> u32 {
        self.linked_playback_guard_frames
            .unwrap_or_else(|| self.effective_hardware_period_size())
    }

    pub fn effective_pro_output_latency_frames(&self) -> u32 {
        let base = self
            .period_size
            .saturating_mul(self.pro_latency_periods.max(1));
        if self.uses_staged_pro_packets() {
            base.saturating_add(self.effective_hardware_period_size())
        } else {
            base
        }
    }

    pub fn uses_staged_pro_packets(&self) -> bool {
        let hardware_period = self.effective_hardware_period_size();
        self.effective_duplex_link()
            && self.pro_latency_periods == 1
            && hardware_period < self.period_size
            && self.buffer_size >= self.period_size.saturating_add(hardware_period)
    }

    pub fn effective_shared_buffer_size(&self) -> u32 {
        self.shared_buffer_size.unwrap_or_else(|| {
            self.period_size.saturating_mul(
                self.buffer_size
                    .div_ceil(self.period_size)
                    .clamp(2, MAX_SHARED_BUFFER_PERIODS),
            )
        })
    }
}

fn default_pro_latency_periods() -> u32 {
    1
}

fn default_pro_handoff_us() -> u32 {
    DEFAULT_PRO_HANDOFF_US
}

fn default_shared_latency_periods() -> u32 {
    4
}

fn default_realtime() -> bool {
    true
}

fn default_realtime_priority() -> u32 {
    50
}

impl PcmConfig {
    fn validate(&self, direction: &str) -> Result<(), ProfileError> {
        if self.device.trim().is_empty() {
            return Err(ProfileError::Invalid(format!(
                "{direction} device is empty"
            )));
        }
        if self.channels == 0 {
            return Err(ProfileError::Invalid(format!(
                "{direction} channels must be non-zero"
            )));
        }
        Ok(())
    }
}

fn validate_ports(
    direction: &str,
    ports: &[PortConfig],
    channel_count: u32,
    ids: &mut HashSet<String>,
) -> Result<(), ProfileError> {
    let mut mapped_channels = HashSet::new();

    for port in ports {
        validate_port_id(&port.id)?;
        if !ids.insert(port.id.clone()) {
            return Err(ProfileError::Invalid(format!(
                "duplicate port id '{}'",
                port.id
            )));
        }
        if port.name.trim().is_empty() {
            return Err(ProfileError::Invalid(format!(
                "port '{}' name is empty",
                port.id
            )));
        }
        if port.name.chars().any(char::is_control) {
            return Err(ProfileError::Invalid(format!(
                "port '{}' name contains a control character",
                port.id
            )));
        }
        if port.channels.is_empty() {
            return Err(ProfileError::Invalid(format!(
                "port '{}' must map at least one channel",
                port.id
            )));
        }

        for &channel in &port.channels {
            if channel >= channel_count {
                return Err(ProfileError::Invalid(format!(
                    "{direction} port '{}' maps channel {channel} outside 0..{channel_count}",
                    port.id
                )));
            }
            if !mapped_channels.insert(channel) {
                return Err(ProfileError::Invalid(format!(
                    "{direction} channel {channel} is mapped more than once"
                )));
            }
        }
    }
    Ok(())
}

fn validate_port_id(id: &str) -> Result<(), ProfileError> {
    if id.is_empty() {
        return Err(ProfileError::Invalid("port id is empty".into()));
    }
    if id != id.trim()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(ProfileError::Invalid(format!(
            "port id '{id}' contains invalid characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r#"
        [device]
        name = "Test interface"
        rate = 48000
        period_size = 32
        buffer_size = 64
        playback_queue_periods = 1
        playback_timer_scheduling = true
        duplex_link = true
        pro_latency_periods = 1
        pro_realtime_priority = 15
        shared_latency_periods = 4
        realtime = true
        realtime_priority = 50

        [device.playback]
        device = "hw:Test,0"
        channels = 4
        format = "S32_LE"

        [device.capture]
        device = "hw:Test,0"
        channels = 2
        format = "S32_LE"

        [[ports.playback]]
        id = "line1"
        name = "Line 1"
        channels = [0, 1]

        [[ports.playback]]
        id = "line2"
        name = "Line 2"
        channels = [2, 3]

        [[ports.capture]]
        id = "mic1"
        name = "Mic 1"
        channels = [0]
    "#;
    const E1X2_PROFILE: &str = include_str!("../../../profiles/topping-e1x2.toml");

    #[test]
    fn parses_profile_with_ports() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");

        assert_eq!(profile.device.name, "Test interface");
        assert_eq!(profile.device.hardware_period_size, None);
        assert_eq!(profile.device.effective_hardware_period_size(), 32);
        assert_eq!(profile.device.shared_buffer_size, None);
        assert_eq!(profile.device.effective_shared_buffer_size(), 64);
        assert_eq!(profile.device.pro_latency_periods, 1);
        assert_eq!(profile.device.pro_handoff_us, DEFAULT_PRO_HANDOFF_US);
        assert_eq!(profile.device.pro_handoff_nanos(), 250_000);
        assert_eq!(profile.device.pro_realtime_priority, Some(15));
        assert_eq!(profile.device.effective_pro_realtime_priority(), 15);
        assert_eq!(profile.device.playback_queue_periods, Some(1));
        assert!(profile.device.playback_timer_scheduling);
        assert_eq!(profile.device.duplex_link, Some(true));
        assert!(profile.device.effective_duplex_link());
        assert_eq!(profile.device.linked_playback_guard_frames, None);
        assert_eq!(profile.device.effective_linked_playback_guard_frames(), 32);
        assert_eq!(profile.device.linked_phase_max_attempts, 0);
        assert_eq!(profile.device.shared_latency_periods, 4);
        assert!(profile.device.realtime);
        assert_eq!(profile.device.realtime_priority, 50);
        assert_eq!(profile.ports.playback.len(), 2);
        assert_eq!(profile.ports.capture[0].channels, vec![0]);
    }

    #[test]
    fn parses_reference_profile_handoff() {
        let profile = Profile::from_toml(E1X2_PROFILE).expect("reference profile should parse");

        assert_eq!(profile.device.pro_handoff_us, 250);
        assert_eq!(profile.device.pro_handoff_nanos(), 250_000);
        assert_eq!(profile.device.pro_latency_periods, 0);
        assert_eq!(profile.device.linked_playback_guard_frames, Some(32));
        assert_eq!(profile.device.effective_linked_playback_guard_frames(), 32);
        assert!(!profile.device.uses_staged_pro_packets());
        assert_eq!(profile.device.effective_pro_output_latency_frames(), 64);
        assert_eq!(profile.device.linked_phase_max_attempts, 0);
        assert_eq!(profile.device.effective_shared_buffer_size(), 512);
    }

    #[test]
    fn whole_period_link_does_not_report_packet_staging() {
        let text = E1X2_PROFILE
            .replace("hardware_period_size = 32", "hardware_period_size = 64")
            .replace(
                "linked_playback_guard_frames = 32",
                "linked_playback_guard_frames = 64",
            );
        let profile = Profile::from_toml(&text).expect("whole-period profile should parse");

        assert!(!profile.device.uses_staged_pro_packets());
        assert_eq!(profile.device.effective_pro_output_latency_frames(), 64);
    }

    #[test]
    fn parses_configurable_pro_latency() {
        let text = PROFILE.replace("pro_latency_periods = 1", "pro_latency_periods = 3");
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.pro_latency_periods, 3);
    }

    #[test]
    fn parses_configurable_pro_handoff() {
        let text = PROFILE.replace(
            "pro_latency_periods = 1",
            "pro_latency_periods = 1\n        pro_handoff_us = 500",
        );
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.pro_handoff_us, 500);
        assert_eq!(profile.device.pro_handoff_nanos(), 500_000);
    }

    #[test]
    fn rejects_zero_pro_handoff() {
        let text = PROFILE.replace(
            "pro_latency_periods = 1",
            "pro_latency_periods = 1\n        pro_handoff_us = 0",
        );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(
            error
                .to_string()
                .contains("pro_handoff_us must be non-zero")
        );
    }

    #[test]
    fn parses_linked_phase_attempts() {
        let text = PROFILE
            .replace("pro_latency_periods = 1", "pro_latency_periods = 0")
            .replace("buffer_size = 64", "buffer_size = 96")
            .replace(
                "duplex_link = true",
                "duplex_link = true\n        linked_phase_max_attempts = 32",
            );
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.linked_phase_max_attempts, 32);
    }

    #[test]
    fn rejects_linked_phase_attempts_without_zero_lead() {
        let text = PROFILE.replace(
            "duplex_link = true",
            "duplex_link = true\n        linked_phase_max_attempts = 1",
        );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(error.to_string().contains("requires linked zero-lead PRO"));
    }

    #[test]
    fn rejects_too_many_linked_phase_attempts() {
        let text = PROFILE
            .replace("pro_latency_periods = 1", "pro_latency_periods = 0")
            .replace("buffer_size = 64", "buffer_size = 96")
            .replace(
                "duplex_link = true",
                "duplex_link = true\n        linked_phase_max_attempts = 65",
            );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(error.to_string().contains("must be <= 64"));
    }

    #[test]
    fn rejects_linked_phase_target_shorter_than_handoff() {
        let text = PROFILE
            .replace(
                "pro_latency_periods = 1",
                "pro_latency_periods = 0\n        pro_handoff_us = 300",
            )
            .replace("buffer_size = 64", "buffer_size = 96")
            .replace(
                "duplex_link = true",
                "duplex_link = true\n        linked_phase_max_attempts = 1",
            );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(
            error
                .to_string()
                .contains("too short for PRO handoff and overhead")
        );
    }

    #[test]
    fn parses_smaller_hardware_period() {
        let text = PROFILE.replace(
            "period_size = 32",
            "period_size = 32\n        hardware_period_size = 16",
        );
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.hardware_period_size, Some(16));
        assert_eq!(profile.device.effective_hardware_period_size(), 16);
    }

    #[test]
    fn rejects_linked_playback_guard_below_hardware_period() {
        let text = PROFILE.replace(
            "duplex_link = true",
            "duplex_link = true\n        linked_playback_guard_frames = 16",
        );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(error.to_string().contains("at least hardware_period_size"));
    }

    #[test]
    fn rejects_linked_playback_guard_without_write_space() {
        let text = PROFILE.replace(
            "duplex_link = true",
            "duplex_link = true\n        linked_playback_guard_frames = 33",
        );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(error.to_string().contains("leave one linked write"));
    }

    #[test]
    fn accepts_hardware_period_guard_with_zero_lead() {
        let profile = Profile::from_toml(E1X2_PROFILE).expect("profile should parse");
        assert_eq!(profile.device.linked_playback_guard_frames, Some(32));
    }

    #[test]
    fn rejects_linked_playback_guard_for_longer_pro_lead() {
        let text = PROFILE
            .replace("pro_latency_periods = 1", "pro_latency_periods = 2")
            .replace(
                "duplex_link = true",
                "duplex_link = true\n        linked_playback_guard_frames = 32",
            );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(error.to_string().contains("pro_latency_periods <= 1"));
    }

    #[test]
    fn rejects_one_lead_guard_without_linked_packet_buffer() {
        let text = PROFILE
            .replace("buffer_size = 64", "buffer_size = 48")
            .replace(
                "duplex_link = true",
                "duplex_link = true\n        linked_playback_guard_frames = 32",
            );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(
            error
                .to_string()
                .contains("buffer_size >= period_size + hardware_period_size")
        );
    }

    #[test]
    fn zero_lead_guard_leaves_a_full_client_write() {
        let text = E1X2_PROFILE.replace(
            "linked_playback_guard_frames = 32",
            "linked_playback_guard_frames = 160",
        );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(error.to_string().contains("leave one linked write"));
    }

    #[test]
    fn parses_independent_shared_buffer() {
        let text = PROFILE.replace(
            "buffer_size = 64",
            "buffer_size = 64\n        shared_buffer_size = 256",
        );
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.effective_shared_buffer_size(), 256);
    }

    #[test]
    fn rejects_shared_buffer_shorter_than_two_periods() {
        let text = PROFILE.replace(
            "buffer_size = 64",
            "buffer_size = 64\n        shared_buffer_size = 32",
        );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(error.to_string().contains("at least two periods"));
    }

    #[test]
    fn rejects_unaligned_shared_buffer() {
        let text = PROFILE.replace(
            "buffer_size = 64",
            "buffer_size = 64\n        shared_buffer_size = 200",
        );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(error.to_string().contains("multiple of period_size"));
    }

    #[test]
    fn rejects_shared_buffer_larger_than_slot_ring() {
        let text = PROFILE.replace(
            "buffer_size = 64",
            "buffer_size = 64\n        shared_buffer_size = 288",
        );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(error.to_string().contains("at most 8 periods"));
    }

    #[test]
    fn rejects_hardware_period_that_does_not_divide_client_period() {
        let text = PROFILE.replace(
            "period_size = 32",
            "period_size = 32\n        hardware_period_size = 24",
        );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(
            error
                .to_string()
                .contains("hardware_period_size must divide period_size")
        );
    }

    #[test]
    fn accepts_zero_pro_latency() {
        let text = PROFILE
            .replace("pro_latency_periods = 1", "pro_latency_periods = 0")
            .replace("buffer_size = 64", "buffer_size = 96");
        let profile = Profile::from_toml(&text).expect("zero lead should parse");

        assert_eq!(profile.device.pro_latency_periods, 0);
    }

    #[test]
    fn rejects_zero_pro_latency_without_handoff_buffer() {
        let text = PROFILE.replace("pro_latency_periods = 1", "pro_latency_periods = 0");

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(error.to_string().contains("requires buffer_size >= 96"));
    }

    #[test]
    fn linked_handoff_rounds_up_to_one_hardware_period() {
        let text = PROFILE
            .replace("buffer_size = 64", "buffer_size = 87")
            .replace(
                "pro_latency_periods = 1",
                "pro_latency_periods = 0\n        pro_handoff_us = 500",
            );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(error.to_string().contains("requires buffer_size >= 96"));
    }

    #[test]
    fn rejects_pro_handoff_without_physical_write_reserve() {
        let text = PROFILE
            .replace("buffer_size = 64", "buffer_size = 96")
            .replace(
                "pro_latency_periods = 1",
                "pro_latency_periods = 0\n        pro_handoff_us = 600",
            );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(
            error
                .to_string()
                .contains("leaves no physical-period write reserve")
        );
    }

    #[test]
    fn rejects_zero_pro_latency_without_timer_scheduling() {
        let text = PROFILE
            .replace("pro_latency_periods = 1", "pro_latency_periods = 0")
            .replace(
                "playback_timer_scheduling = true",
                "playback_timer_scheduling = false",
            );

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(
            error
                .to_string()
                .contains("requires playback_timer_scheduling = true")
        );
    }

    #[test]
    fn rejects_zero_pro_latency_without_linked_duplex() {
        let text = PROFILE
            .replace("pro_latency_periods = 1", "pro_latency_periods = 0")
            .replace("duplex_link = true", "duplex_link = false");

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(error.to_string().contains("requires duplex_link = true"));
    }

    #[test]
    fn rejects_zero_pro_latency_with_one_hardware_period() {
        let text = PROFILE
            .replace("buffer_size = 64", "buffer_size = 32")
            .replace("pro_latency_periods = 1", "pro_latency_periods = 0");

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(
            error
                .to_string()
                .contains("requires at least two logical periods")
        );
    }

    #[test]
    fn parses_configurable_pro_realtime_priority() {
        let text = PROFILE
            .replace("pro_realtime_priority = 15", "pro_realtime_priority = 86")
            .replace("realtime_priority = 50", "realtime_priority = 88");
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.pro_realtime_priority, Some(86));
        assert_eq!(profile.device.effective_pro_realtime_priority(), 86);
    }

    #[test]
    fn rejects_pro_priority_too_close_to_hardware() {
        let text = PROFILE.replace("pro_realtime_priority = 15", "pro_realtime_priority = 49");

        let error = Profile::from_toml(&text).expect_err("profile should fail");
        assert!(
            error
                .to_string()
                .contains("must be at least two below realtime_priority")
        );
    }

    #[test]
    fn disables_pro_realtime_when_hardware_realtime_is_disabled() {
        let text = PROFILE.replace("realtime = true", "realtime = false");
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.effective_pro_realtime_priority(), 0);
    }

    #[test]
    fn defaults_pro_latency_when_omitted() {
        let text = PROFILE
            .replace("pro_latency_periods = 1\n", "")
            .replace("pro_realtime_priority = 15\n", "")
            .replace("playback_queue_periods = 1\n", "")
            .replace("playback_timer_scheduling = true\n", "")
            .replace("duplex_link = true\n", "")
            .replace("shared_latency_periods = 4\n", "")
            .replace("realtime = true\n", "")
            .replace("realtime_priority = 50\n", "");
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.pro_latency_periods, 1);
        assert_eq!(profile.device.pro_realtime_priority, None);
        assert_eq!(profile.device.effective_pro_realtime_priority(), 48);
        assert_eq!(profile.device.playback_queue_periods, None);
        assert!(!profile.device.playback_timer_scheduling);
        assert_eq!(profile.device.duplex_link, None);
        assert!(profile.device.effective_duplex_link());
        assert_eq!(profile.device.shared_latency_periods, 4);
        assert!(profile.device.realtime);
        assert_eq!(profile.device.realtime_priority, 50);
    }

    #[test]
    fn defaults_duplex_link_off_for_separate_devices() {
        let text = PROFILE.replace("duplex_link = true\n", "").replace(
            "[device.capture]\n        device = \"hw:Test,0\"",
            "[device.capture]\n        device = \"hw:Other,0\"",
        );
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.duplex_link, None);
        assert!(!profile.device.effective_duplex_link());
    }

    #[test]
    fn rejects_playback_queue_larger_than_hardware_buffer() {
        let text = PROFILE.replace("playback_queue_periods = 1", "playback_queue_periods = 3");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(
            error
                .to_string()
                .contains("playback_queue_periods must fit within buffer_size")
        );
    }

    #[test]
    fn rejects_pro_latency_above_ring_capacity() {
        let text = PROFILE.replace("pro_latency_periods = 1", "pro_latency_periods = 8");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(
            error
                .to_string()
                .contains("pro_latency_periods must be <= 7")
        );
    }

    #[test]
    fn rejects_invalid_pro_realtime_priority() {
        let text = PROFILE.replace("pro_realtime_priority = 15", "pro_realtime_priority = 100");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(
            error
                .to_string()
                .contains("pro_realtime_priority must be between 1 and 99")
        );
    }

    #[test]
    fn parses_configurable_shared_latency() {
        let text = PROFILE.replace("shared_latency_periods = 4", "shared_latency_periods = 6");
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.shared_latency_periods, 6);
    }

    #[test]
    fn rejects_shared_latency_above_ring_capacity() {
        let text = PROFILE.replace("shared_latency_periods = 4", "shared_latency_periods = 8");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(
            error
                .to_string()
                .contains("shared_latency_periods must be <= 7")
        );
    }

    #[test]
    fn parses_configurable_realtime_priority() {
        let text = PROFILE.replace("realtime_priority = 50", "realtime_priority = 80");
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.realtime_priority, 80);
    }

    #[test]
    fn rejects_invalid_realtime_priority() {
        let text = PROFILE.replace("realtime_priority = 50", "realtime_priority = 100");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(
            error
                .to_string()
                .contains("realtime_priority must be between 1 and 99")
        );
    }

    #[test]
    fn rejects_out_of_range_channel() {
        let text = PROFILE.replace("channels = [2, 3]", "channels = [2, 4]");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(error.to_string().contains("outside"));
    }

    #[test]
    fn rejects_duplicate_port_id() {
        let text = PROFILE.replace("id = \"line2\"", "id = \"line1\"");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(error.to_string().contains("duplicate port id"));
    }

    #[test]
    fn rejects_duplicate_channel_mapping() {
        let text = PROFILE.replace("channels = [2, 3]", "channels = [1, 2]");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(error.to_string().contains("mapped more than once"));
    }

    #[test]
    fn rejects_duplicate_channel_within_port() {
        let text = PROFILE.replace("channels = [0, 1]", "channels = [0, 0]");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(error.to_string().contains("mapped more than once"));
    }

    #[test]
    fn rejects_invalid_port_id() {
        let text = PROFILE.replace("id = \"line1\"", "id = \"line 1\"");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(error.to_string().contains("invalid characters"));
    }

    #[test]
    fn rejects_empty_port_name() {
        let text = PROFILE.replace("name = \"Line 1\"", "name = \"\"");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(error.to_string().contains("name is empty"));
    }

    #[test]
    fn rejects_empty_port_mapping() {
        let text = PROFILE.replace("channels = [0, 1]", "channels = []");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(error.to_string().contains("at least one channel"));
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = PROFILE.replace("[device]\n", "[device]\nunknown = true\n");

        let error = Profile::from_toml(&text).expect_err("profile should fail");

        assert!(error.to_string().contains("unknown field"));
    }
}
