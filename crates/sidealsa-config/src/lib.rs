use std::{collections::HashSet, fs, path::Path};

use serde::Deserialize;
use thiserror::Error;
use toml_edit::{DocumentMut, Item, Table, value};

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
    #[serde(default)]
    pub shared_playback_repeat_on_underrun: bool,
    #[serde(default = "default_realtime")]
    pub realtime: bool,
    #[serde(default = "default_realtime_priority")]
    pub realtime_priority: u32,
    pub playback: PcmConfig,
    pub capture: PcmConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimingSettings {
    pub rate: u32,
    pub period_size: u32,
    pub hardware_period_size: Option<u32>,
    pub buffer_size: u32,
    pub shared_buffer_size: Option<u32>,
    pub playback_queue_periods: Option<u32>,
    pub playback_timer_scheduling: bool,
    pub duplex_link: Option<bool>,
    pub linked_playback_guard_frames: Option<u32>,
    pub linked_phase_max_attempts: u32,
    pub pro_latency_periods: u32,
    pub pro_handoff_us: u32,
    pub pro_realtime_priority: Option<u32>,
    pub shared_latency_periods: u32,
    pub shared_playback_repeat_on_underrun: bool,
    pub realtime: bool,
    pub realtime_priority: u32,
}

#[derive(Clone, Debug)]
pub struct ProfileDocument {
    document: DocumentMut,
    profile: Profile,
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
    #[error("could not edit profile: {0}")]
    Edit(#[from] toml_edit::TomlError),
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

    pub fn fingerprint(&self) -> u64 {
        let mut hash = 0xcbf29ce484222325_u64;
        hash_u64(&mut hash, TimingSettings::from(&self.device).fingerprint());
        hash_string(&mut hash, &self.device.name);
        hash_pcm(&mut hash, &self.device.playback);
        hash_pcm(&mut hash, &self.device.capture);
        hash_ports(&mut hash, &self.ports.playback);
        hash_ports(&mut hash, &self.ports.capture);
        hash
    }
}

impl ProfileDocument {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ProfileError> {
        let text = fs::read_to_string(path)?;
        Self::from_toml(&text)
    }

    pub fn from_toml(text: &str) -> Result<Self, ProfileError> {
        let profile = Profile::from_toml(text)?;
        let document = text.parse::<DocumentMut>()?;
        Ok(Self { document, profile })
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn timing(&self) -> TimingSettings {
        TimingSettings::from(&self.profile.device)
    }

    pub fn apply_timing(&mut self, timing: &TimingSettings) -> Result<(), ProfileError> {
        let mut profile = self.profile.clone();
        timing.apply_to(&mut profile.device);
        profile.validate()?;

        let mut document = self.document.clone();
        let device = document
            .get_mut("device")
            .and_then(toml_edit::Item::as_table_mut)
            .ok_or_else(|| ProfileError::Invalid("profile lacks [device] table".into()))?;
        write_timing(device, timing);

        let rendered = document.to_string();
        let reparsed = Profile::from_toml(&rendered)?;
        self.document = document;
        self.profile = reparsed;
        Ok(())
    }

    pub fn to_toml(&self) -> String {
        self.document.to_string()
    }
}

impl From<&HardwareConfig> for TimingSettings {
    fn from(config: &HardwareConfig) -> Self {
        Self {
            rate: config.rate,
            period_size: config.period_size,
            hardware_period_size: config.hardware_period_size,
            buffer_size: config.buffer_size,
            shared_buffer_size: config.shared_buffer_size,
            playback_queue_periods: config.playback_queue_periods,
            playback_timer_scheduling: config.playback_timer_scheduling,
            duplex_link: config.duplex_link,
            linked_playback_guard_frames: config.linked_playback_guard_frames,
            linked_phase_max_attempts: config.linked_phase_max_attempts,
            pro_latency_periods: config.pro_latency_periods,
            pro_handoff_us: config.pro_handoff_us,
            pro_realtime_priority: config.pro_realtime_priority,
            shared_latency_periods: config.shared_latency_periods,
            shared_playback_repeat_on_underrun: config.shared_playback_repeat_on_underrun,
            realtime: config.realtime,
            realtime_priority: config.realtime_priority,
        }
    }
}

impl TimingSettings {
    pub fn apply_to(&self, config: &mut HardwareConfig) {
        config.rate = self.rate;
        config.period_size = self.period_size;
        config.hardware_period_size = self.hardware_period_size;
        config.buffer_size = self.buffer_size;
        config.shared_buffer_size = self.shared_buffer_size;
        config.playback_queue_periods = self.playback_queue_periods;
        config.playback_timer_scheduling = self.playback_timer_scheduling;
        config.duplex_link = self.duplex_link;
        config.linked_playback_guard_frames = self.linked_playback_guard_frames;
        config.linked_phase_max_attempts = self.linked_phase_max_attempts;
        config.pro_latency_periods = self.pro_latency_periods;
        config.pro_handoff_us = self.pro_handoff_us;
        config.pro_realtime_priority = self.pro_realtime_priority;
        config.shared_latency_periods = self.shared_latency_periods;
        config.shared_playback_repeat_on_underrun = self.shared_playback_repeat_on_underrun;
        config.realtime = self.realtime;
        config.realtime_priority = self.realtime_priority;
    }

    pub fn fingerprint(&self) -> u64 {
        let mut hash = 0xcbf29ce484222325_u64;
        hash_u32(&mut hash, self.rate);
        hash_u32(&mut hash, self.period_size);
        hash_optional_u32(&mut hash, self.hardware_period_size);
        hash_u32(&mut hash, self.buffer_size);
        hash_optional_u32(&mut hash, self.shared_buffer_size);
        hash_optional_u32(&mut hash, self.playback_queue_periods);
        hash_bool(&mut hash, self.playback_timer_scheduling);
        hash_optional_bool(&mut hash, self.duplex_link);
        hash_optional_u32(&mut hash, self.linked_playback_guard_frames);
        hash_u32(&mut hash, self.linked_phase_max_attempts);
        hash_u32(&mut hash, self.pro_latency_periods);
        hash_u32(&mut hash, self.pro_handoff_us);
        hash_optional_u32(&mut hash, self.pro_realtime_priority);
        hash_u32(&mut hash, self.shared_latency_periods);
        hash_bool(&mut hash, self.shared_playback_repeat_on_underrun);
        hash_bool(&mut hash, self.realtime);
        hash_u32(&mut hash, self.realtime_priority);
        hash
    }
}

fn write_timing(device: &mut Table, timing: &TimingSettings) {
    set_u32(device, "rate", timing.rate);
    set_u32(device, "period_size", timing.period_size);
    set_optional_u32(device, "hardware_period_size", timing.hardware_period_size);
    set_u32(device, "buffer_size", timing.buffer_size);
    set_optional_u32(device, "shared_buffer_size", timing.shared_buffer_size);
    set_optional_u32(
        device,
        "playback_queue_periods",
        timing.playback_queue_periods,
    );
    set_bool(
        device,
        "playback_timer_scheduling",
        timing.playback_timer_scheduling,
    );
    set_optional_bool(device, "duplex_link", timing.duplex_link);
    set_optional_u32(
        device,
        "linked_playback_guard_frames",
        timing.linked_playback_guard_frames,
    );
    set_u32(
        device,
        "linked_phase_max_attempts",
        timing.linked_phase_max_attempts,
    );
    set_u32(device, "pro_latency_periods", timing.pro_latency_periods);
    set_u32(device, "pro_handoff_us", timing.pro_handoff_us);
    set_optional_u32(
        device,
        "pro_realtime_priority",
        timing.pro_realtime_priority,
    );
    set_u32(
        device,
        "shared_latency_periods",
        timing.shared_latency_periods,
    );
    set_bool(
        device,
        "shared_playback_repeat_on_underrun",
        timing.shared_playback_repeat_on_underrun,
    );
    set_bool(device, "realtime", timing.realtime);
    set_u32(device, "realtime_priority", timing.realtime_priority);
}

fn set_u32(table: &mut Table, key: &str, setting: u32) {
    set_value(table, key, value(i64::from(setting)));
}

fn set_bool(table: &mut Table, key: &str, setting: bool) {
    set_value(table, key, value(setting));
}

fn set_optional_u32(table: &mut Table, key: &str, setting: Option<u32>) {
    if let Some(setting) = setting {
        set_u32(table, key, setting);
    } else {
        table.remove(key);
    }
}

fn set_optional_bool(table: &mut Table, key: &str, setting: Option<bool>) {
    if let Some(setting) = setting {
        set_bool(table, key, setting);
    } else {
        table.remove(key);
    }
}

fn set_value(table: &mut Table, key: &str, mut setting: Item) {
    if let (Some(existing), Some(replacement)) = (
        table.get(key).and_then(Item::as_value),
        setting.as_value_mut(),
    ) {
        *replacement.decor_mut() = existing.decor().clone();
    }
    table[key] = setting;
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

fn hash_u32(hash: &mut u64, setting: u32) {
    hash_bytes(hash, &setting.to_le_bytes());
}

fn hash_u64(hash: &mut u64, setting: u64) {
    hash_bytes(hash, &setting.to_le_bytes());
}

fn hash_bool(hash: &mut u64, setting: bool) {
    hash_bytes(hash, &[u8::from(setting)]);
}

fn hash_optional_u32(hash: &mut u64, setting: Option<u32>) {
    hash_bool(hash, setting.is_some());
    if let Some(setting) = setting {
        hash_u32(hash, setting);
    }
}

fn hash_optional_bool(hash: &mut u64, setting: Option<bool>) {
    hash_bool(hash, setting.is_some());
    if let Some(setting) = setting {
        hash_bool(hash, setting);
    }
}

fn hash_string(hash: &mut u64, setting: &str) {
    hash_u64(hash, setting.len() as u64);
    hash_bytes(hash, setting.as_bytes());
}

fn hash_pcm(hash: &mut u64, pcm: &PcmConfig) {
    hash_string(hash, &pcm.device);
    hash_u32(hash, pcm.channels);
    hash_bytes(
        hash,
        &[match pcm.format {
            SampleFormat::S32Le => 0,
        }],
    );
}

fn hash_ports(hash: &mut u64, ports: &[PortConfig]) {
    hash_u64(hash, ports.len() as u64);
    for port in ports {
        hash_string(hash, &port.id);
        hash_string(hash, &port.name);
        hash_u64(hash, port.channels.len() as u64);
        for channel in &port.channels {
            hash_u32(hash, *channel);
        }
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
            let hardware_period_nanos = u64::from(hardware_period_size)
                .saturating_mul(1_000_000_000)
                / u64::from(self.rate);
            let overhead_nanos = hardware_period_nanos / u64::from(LINKED_PHASE_OVERHEAD_DIVISOR);
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
        shared_playback_repeat_on_underrun = true
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
        assert!(profile.device.shared_playback_repeat_on_underrun);
        assert!(profile.device.realtime);
        assert_eq!(profile.device.realtime_priority, 50);
        assert_eq!(profile.ports.playback.len(), 2);
        assert_eq!(profile.ports.capture[0].channels, vec![0]);
    }

    #[test]
    fn parses_reference_profile_handoff() {
        let profile = Profile::from_toml(E1X2_PROFILE).expect("reference profile should parse");

        assert_eq!(profile.device.buffer_size, 256);
        assert_eq!(profile.device.pro_handoff_us, 500);
        assert_eq!(profile.device.pro_handoff_nanos(), 500_000);
        assert_eq!(profile.device.pro_latency_periods, 0);
        assert_eq!(profile.device.linked_playback_guard_frames, Some(32));
        assert_eq!(profile.device.effective_linked_playback_guard_frames(), 32);
        assert!(!profile.device.uses_staged_pro_packets());
        assert_eq!(profile.device.effective_pro_output_latency_frames(), 64);
        assert_eq!(profile.device.linked_phase_max_attempts, 8);
        assert_eq!(profile.device.effective_shared_buffer_size(), 512);
        assert_eq!(profile.device.shared_latency_periods, 7);
        assert!(profile.device.shared_playback_repeat_on_underrun);
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
    fn accepts_linked_phase_attempts_with_bounded_handoff() {
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

        let profile = Profile::from_toml(&text).expect("profile should parse");
        assert_eq!(profile.device.linked_phase_max_attempts, 1);
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
    fn accepts_reference_guard_with_zero_lead() {
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
            "linked_playback_guard_frames = 224",
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
            .replace("shared_playback_repeat_on_underrun = true\n", "")
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
        assert!(!profile.device.shared_playback_repeat_on_underrun);
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

    #[test]
    fn profile_document_updates_timing_without_rewriting_other_content() {
        let text = PROFILE
            .replace("rate = 48000", "rate = 48000 # Keep this timing note.")
            .replace(
                "[device.playback]",
                "# Playback routing must survive GUI edits.\n        [device.playback]",
            );
        let mut document = ProfileDocument::from_toml(&text).expect("document should parse");
        let mut timing = document.timing();
        timing.rate = 44_100;
        timing.hardware_period_size = Some(16);
        timing.shared_buffer_size = Some(128);
        timing.linked_playback_guard_frames = Some(16);
        timing.pro_realtime_priority = Some(30);
        timing.shared_playback_repeat_on_underrun = false;

        document
            .apply_timing(&timing)
            .expect("valid timing should apply");

        let rendered = document.to_toml();
        assert!(rendered.contains("# Playback routing must survive GUI edits."));
        assert!(rendered.contains("rate = 44100 # Keep this timing note."));
        assert!(rendered.contains("device = \"hw:Test,0\""));
        assert_eq!(document.profile().device.rate, 44_100);
        assert_eq!(document.profile().device.hardware_period_size, Some(16));
        assert_eq!(document.profile().device.shared_buffer_size, Some(128));
        assert!(!document.profile().device.shared_playback_repeat_on_underrun);
        assert_eq!(
            document.profile().device.linked_playback_guard_frames,
            Some(16)
        );
    }

    #[test]
    fn profile_document_removes_automatic_optional_timing() {
        let mut document = ProfileDocument::from_toml(PROFILE).expect("document should parse");
        let mut timing = document.timing();
        timing.playback_queue_periods = None;
        timing.duplex_link = None;
        timing.pro_realtime_priority = None;

        document
            .apply_timing(&timing)
            .expect("automatic settings should apply");

        let rendered = document.to_toml();
        assert!(!rendered.contains("playback_queue_periods"));
        assert!(!rendered.contains("duplex_link"));
        assert!(!rendered.contains("pro_realtime_priority"));
        assert_eq!(document.timing().duplex_link, None);
    }

    #[test]
    fn profile_document_rejects_invalid_timing_without_mutation() {
        let mut document = ProfileDocument::from_toml(PROFILE).expect("document should parse");
        let original = document.to_toml();
        let mut timing = document.timing();
        timing.hardware_period_size = Some(24);

        let error = document
            .apply_timing(&timing)
            .expect_err("invalid timing should fail");

        assert!(error.to_string().contains("must divide period_size"));
        assert_eq!(document.to_toml(), original);
    }

    #[test]
    fn timing_fingerprint_covers_values_and_automatic_state() {
        let timing = ProfileDocument::from_toml(PROFILE)
            .expect("profile should parse")
            .timing();
        let mut changed = timing.clone();
        changed.pro_handoff_us += 1;
        assert_ne!(timing.fingerprint(), changed.fingerprint());

        changed = timing.clone();
        changed.duplex_link = None;
        assert_ne!(timing.fingerprint(), changed.fingerprint());

        changed = timing.clone();
        changed.shared_playback_repeat_on_underrun = false;
        assert_ne!(timing.fingerprint(), changed.fingerprint());
    }

    #[test]
    fn profile_fingerprint_covers_device_and_routing() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let mut changed = profile.clone();
        changed.ports.playback[0].channels.swap(0, 1);
        assert_ne!(profile.fingerprint(), changed.fingerprint());

        changed = profile.clone();
        changed.device.playback.device = "hw:Other,0".into();
        assert_ne!(profile.fingerprint(), changed.fingerprint());
    }
}
