use sidealsa_config::{PortConfig, Profile, ProfileError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RoutingError {
    #[error("invalid profile: {0}")]
    Profile(#[from] ProfileError),
    #[error("port channel {channel} does not fit target index size")]
    ChannelIndex { channel: u32 },
    #[error("physical channel count must be non-zero")]
    ZeroPhysicalChannels,
    #[error("port channel {channel} is outside physical channel count {physical_channels}")]
    PhysicalChannelOutOfRange {
        channel: usize,
        physical_channels: usize,
    },
    #[error("physical buffer too small: {actual} samples, {required} required")]
    PhysicalBufferTooSmall { actual: usize, required: usize },
    #[error("logical buffer too small: {actual} samples, {required} required")]
    LogicalBufferTooSmall { actual: usize, required: usize },
    #[error("sample count overflows buffer size")]
    SampleCountOverflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledPort {
    id: Box<str>,
    name: Box<str>,
    channels: Box<[usize]>,
}

impl CompiledPort {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn channels(&self) -> &[usize] {
        &self.channels
    }

    pub fn copy_from_physical<T: Copy>(
        &self,
        physical: &[T],
        physical_channels: usize,
        frames: usize,
        logical: &mut [T],
    ) -> Result<(), RoutingError> {
        self.check_buffers(physical.len(), physical_channels, frames, logical.len())?;

        for frame in 0..frames {
            let physical_offset = frame * physical_channels;
            let logical_offset = frame * self.channels.len();
            for (logical_channel, &physical_channel) in self.channels.iter().enumerate() {
                logical[logical_offset + logical_channel] =
                    physical[physical_offset + physical_channel];
            }
        }
        Ok(())
    }

    pub fn copy_to_physical<T: Copy>(
        &self,
        logical: &[T],
        physical_channels: usize,
        frames: usize,
        physical: &mut [T],
    ) -> Result<(), RoutingError> {
        self.check_buffers(physical.len(), physical_channels, frames, logical.len())?;

        for frame in 0..frames {
            let physical_offset = frame * physical_channels;
            let logical_offset = frame * self.channels.len();
            for (logical_channel, &physical_channel) in self.channels.iter().enumerate() {
                physical[physical_offset + physical_channel] =
                    logical[logical_offset + logical_channel];
            }
        }
        Ok(())
    }

    pub fn add_to_physical_saturating(
        &self,
        logical: &[i32],
        physical_channels: usize,
        frames: usize,
        physical: &mut [i32],
    ) -> Result<(), RoutingError> {
        self.check_buffers(physical.len(), physical_channels, frames, logical.len())?;

        for frame in 0..frames {
            let physical_offset = frame * physical_channels;
            let logical_offset = frame * self.channels.len();
            for (logical_channel, &physical_channel) in self.channels.iter().enumerate() {
                let physical_index = physical_offset + physical_channel;
                physical[physical_index] = physical[physical_index]
                    .saturating_add(logical[logical_offset + logical_channel]);
            }
        }
        Ok(())
    }

    fn check_buffers(
        &self,
        physical_len: usize,
        physical_channels: usize,
        frames: usize,
        logical_len: usize,
    ) -> Result<(), RoutingError> {
        if physical_channels == 0 {
            return Err(RoutingError::ZeroPhysicalChannels);
        }
        if let Some(&channel) = self
            .channels
            .iter()
            .find(|&&channel| channel >= physical_channels)
        {
            return Err(RoutingError::PhysicalChannelOutOfRange {
                channel,
                physical_channels,
            });
        }
        let physical_samples = frames
            .checked_mul(physical_channels)
            .ok_or(RoutingError::SampleCountOverflow)?;
        let logical_samples = frames
            .checked_mul(self.channels.len())
            .ok_or(RoutingError::SampleCountOverflow)?;
        if physical_len < physical_samples {
            return Err(RoutingError::PhysicalBufferTooSmall {
                actual: physical_len,
                required: physical_samples,
            });
        }
        if logical_len < logical_samples {
            return Err(RoutingError::LogicalBufferTooSmall {
                actual: logical_len,
                required: logical_samples,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutingTable {
    playback: Box<[CompiledPort]>,
    capture: Box<[CompiledPort]>,
}

impl RoutingTable {
    pub fn compile(profile: &Profile) -> Result<Self, RoutingError> {
        profile.validate()?;
        Ok(Self {
            playback: compile_ports(&profile.ports.playback)?,
            capture: compile_ports(&profile.ports.capture)?,
        })
    }

    pub fn playback_ports(&self) -> &[CompiledPort] {
        &self.playback
    }

    pub fn capture_ports(&self) -> &[CompiledPort] {
        &self.capture
    }

    pub fn find_playback(&self, id: &str) -> Option<&CompiledPort> {
        self.playback.iter().find(|port| port.id() == id)
    }

    pub fn find_capture(&self, id: &str) -> Option<&CompiledPort> {
        self.capture.iter().find(|port| port.id() == id)
    }
}

fn compile_ports(ports: &[PortConfig]) -> Result<Box<[CompiledPort]>, RoutingError> {
    ports
        .iter()
        .map(|port| {
            let channels = port
                .channels
                .iter()
                .copied()
                .map(|channel| {
                    usize::try_from(channel).map_err(|_| RoutingError::ChannelIndex { channel })
                })
                .collect::<Result<Vec<_>, _>>()?
                .into_boxed_slice();
            Ok(CompiledPort {
                id: port.id.clone().into_boxed_str(),
                name: port.name.clone().into_boxed_str(),
                channels,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Vec::into_boxed_slice)
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
        channels = [1, 3]
    "#;

    #[test]
    fn compiles_immutable_port_channels() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let routing = RoutingTable::compile(&profile).expect("routing should compile");
        let port = routing.find_playback("line1").expect("port should exist");

        assert_eq!(port.name(), "Line 1");
        assert_eq!(port.channels(), &[1, 3]);
    }

    #[test]
    fn maps_physical_frames_to_logical_frames() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let routing = RoutingTable::compile(&profile).expect("routing should compile");
        let port = routing.find_playback("line1").expect("port should exist");
        let physical = [10, 11, 12, 13, 20, 21, 22, 23];
        let mut logical = [0; 4];

        port.copy_from_physical(&physical, 4, 2, &mut logical)
            .expect("mapping should succeed");

        assert_eq!(logical, [11, 13, 21, 23]);
    }

    #[test]
    fn maps_logical_frames_to_physical_frames() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let routing = RoutingTable::compile(&profile).expect("routing should compile");
        let port = routing.find_playback("line1").expect("port should exist");
        let logical = [100, 101, 200, 201];
        let mut physical = [0; 8];

        port.copy_to_physical(&logical, 4, 2, &mut physical)
            .expect("mapping should succeed");

        assert_eq!(physical, [0, 100, 0, 101, 0, 200, 0, 201]);
    }

    #[test]
    fn rejects_short_mapping_buffers() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let routing = RoutingTable::compile(&profile).expect("routing should compile");
        let port = routing.find_playback("line1").expect("port should exist");
        let mut logical = [0; 2];

        let error = port
            .copy_from_physical(&[0; 8], 4, 2, &mut logical)
            .expect_err("mapping should fail");

        assert!(matches!(error, RoutingError::LogicalBufferTooSmall { .. }));
    }

    #[test]
    fn mixes_logical_frames_with_saturation() {
        let profile = Profile::from_toml(PROFILE).expect("profile should parse");
        let routing = RoutingTable::compile(&profile).expect("routing should compile");
        let port = routing.find_playback("line1").expect("port should exist");
        let logical = [i32::MAX, 2, -i32::MAX, -3];
        let mut physical = [0, i32::MAX - 1, 0, 0, 0, 0, 0, 0];

        port.add_to_physical_saturating(&logical, 4, 2, &mut physical)
            .expect("mix should succeed");

        assert_eq!(physical, [0, i32::MAX, 0, 2, 0, -i32::MAX, 0, -3]);
    }
}
