use std::{collections::HashSet, fs, path::Path};

use serde::Deserialize;
use thiserror::Error;

pub const MAX_PRO_LATENCY_PERIODS: u32 = 7;
pub const MAX_SHARED_LATENCY_PERIODS: u32 = 7;
pub const MAX_REALTIME_PRIORITY: u32 = 99;

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
    pub buffer_size: u32,
    #[serde(default = "default_pro_latency_periods")]
    pub pro_latency_periods: u32,
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
        if self.buffer_size == 0 {
            return Err(ProfileError::Invalid("buffer_size must be non-zero".into()));
        }
        if self.buffer_size < self.period_size {
            return Err(ProfileError::Invalid(
                "buffer_size must not be smaller than period_size".into(),
            ));
        }
        if self.pro_latency_periods > MAX_PRO_LATENCY_PERIODS {
            return Err(ProfileError::Invalid(format!(
                "pro_latency_periods must be <= {MAX_PRO_LATENCY_PERIODS}"
            )));
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
        self.playback.validate("playback")?;
        self.capture.validate("capture")?;
        Ok(())
    }
}

fn default_pro_latency_periods() -> u32 {
    1
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
        pro_latency_periods = 1
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

    #[test]
    fn parses_profile_with_ports() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");

        assert_eq!(profile.device.name, "Test interface");
        assert_eq!(profile.device.pro_latency_periods, 1);
        assert_eq!(profile.device.shared_latency_periods, 4);
        assert!(profile.device.realtime);
        assert_eq!(profile.device.realtime_priority, 50);
        assert_eq!(profile.ports.playback.len(), 2);
        assert_eq!(profile.ports.capture[0].channels, vec![0]);
    }

    #[test]
    fn parses_configurable_pro_latency() {
        let text = PROFILE.replace("pro_latency_periods = 1", "pro_latency_periods = 3");
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.pro_latency_periods, 3);
    }

    #[test]
    fn defaults_pro_latency_when_omitted() {
        let text = PROFILE
            .replace("pro_latency_periods = 1\n", "")
            .replace("shared_latency_periods = 4\n", "")
            .replace("realtime = true\n", "")
            .replace("realtime_priority = 50\n", "");
        let profile = Profile::from_toml(&text).expect("profile should parse");

        assert_eq!(profile.device.pro_latency_periods, 1);
        assert_eq!(profile.device.shared_latency_periods, 4);
        assert!(profile.device.realtime);
        assert_eq!(profile.device.realtime_priority, 50);
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
