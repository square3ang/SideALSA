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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChannelLayout {
    Empty,
    Mono(usize),
    Contiguous { start: usize, len: usize },
    Pair(usize, usize),
    General,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledPort {
    id: Box<str>,
    name: Box<str>,
    channels: Box<[usize]>,
    max_channel: Option<usize>,
    layout: ChannelLayout,
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
        let (physical_samples, logical_samples) =
            self.check_buffers(physical.len(), physical_channels, frames, logical.len())?;
        if frames == 0 {
            return Ok(());
        }
        let physical = &physical[..physical_samples];
        let logical = &mut logical[..logical_samples];

        match self.layout {
            ChannelLayout::Empty => {}
            ChannelLayout::Mono(ch) => {
                assert!(ch < physical_channels);
                for (dst, src) in logical
                    .iter_mut()
                    .zip(physical.chunks_exact(physical_channels))
                {
                    *dst = src[ch];
                }
            }
            ChannelLayout::Contiguous { start: 0, len } if len == physical_channels => {
                logical.copy_from_slice(physical);
            }
            ChannelLayout::Contiguous { start, len: 2 } => {
                let c0 = start;
                let c1 = start + 1;
                assert!(c1 < physical_channels);
                for (dst, src) in logical
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(physical.chunks_exact(physical_channels))
                {
                    dst[0] = src[c0];
                    dst[1] = src[c1];
                }
            }
            ChannelLayout::Contiguous { start, len } => {
                let end = start + len;
                assert!(end <= physical_channels);
                for (dst, src) in logical
                    .chunks_exact_mut(len)
                    .zip(physical.chunks_exact(physical_channels))
                {
                    dst.copy_from_slice(&src[start..end]);
                }
            }
            ChannelLayout::Pair(c0, c1) => {
                assert!(c0 < physical_channels && c1 < physical_channels);
                for (dst, src) in logical
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(physical.chunks_exact(physical_channels))
                {
                    dst[0] = src[c0];
                    dst[1] = src[c1];
                }
            }
            ChannelLayout::General => {
                let logical_channels = self.channels.len();
                for (dst, src) in logical
                    .chunks_exact_mut(logical_channels)
                    .zip(physical.chunks_exact(physical_channels))
                {
                    for (logical_channel, &physical_channel) in self.channels.iter().enumerate() {
                        dst[logical_channel] = src[physical_channel];
                    }
                }
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
        let (physical_samples, logical_samples) =
            self.check_buffers(physical.len(), physical_channels, frames, logical.len())?;
        if frames == 0 {
            return Ok(());
        }
        let logical = &logical[..logical_samples];
        let physical = &mut physical[..physical_samples];

        match self.layout {
            ChannelLayout::Empty => {}
            ChannelLayout::Mono(ch) => {
                assert!(ch < physical_channels);
                for (&src, dst) in logical
                    .iter()
                    .zip(physical.chunks_exact_mut(physical_channels))
                {
                    dst[ch] = src;
                }
            }
            ChannelLayout::Contiguous { start: 0, len } if len == physical_channels => {
                physical.copy_from_slice(logical);
            }
            ChannelLayout::Contiguous { start, len: 2 } => {
                let c0 = start;
                let c1 = start + 1;
                assert!(c1 < physical_channels);
                for (src, dst) in logical
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(physical.chunks_exact_mut(physical_channels))
                {
                    dst[c0] = src[0];
                    dst[c1] = src[1];
                }
            }
            ChannelLayout::Contiguous { start, len } => {
                let end = start + len;
                assert!(end <= physical_channels);
                for (src, dst) in logical
                    .chunks_exact(len)
                    .zip(physical.chunks_exact_mut(physical_channels))
                {
                    dst[start..end].copy_from_slice(src);
                }
            }
            ChannelLayout::Pair(c0, c1) => {
                assert!(c0 < physical_channels && c1 < physical_channels);
                for (src, dst) in logical
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(physical.chunks_exact_mut(physical_channels))
                {
                    dst[c0] = src[0];
                    dst[c1] = src[1];
                }
            }
            ChannelLayout::General => {
                let logical_channels = self.channels.len();
                for (src, dst) in logical
                    .chunks_exact(logical_channels)
                    .zip(physical.chunks_exact_mut(physical_channels))
                {
                    for (logical_channel, &physical_channel) in self.channels.iter().enumerate() {
                        dst[physical_channel] = src[logical_channel];
                    }
                }
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
        let (physical_samples, logical_samples) =
            self.check_buffers(physical.len(), physical_channels, frames, logical.len())?;
        if frames == 0 {
            return Ok(());
        }
        let logical = &logical[..logical_samples];
        let physical = &mut physical[..physical_samples];

        match self.layout {
            ChannelLayout::Empty => {}
            ChannelLayout::Mono(ch) => {
                assert!(ch < physical_channels);
                for (&src, dst) in logical
                    .iter()
                    .zip(physical.chunks_exact_mut(physical_channels))
                {
                    dst[ch] = dst[ch].saturating_add(src);
                }
            }
            ChannelLayout::Contiguous { start: 0, len } if len == physical_channels => {
                for (dst, &src) in physical.iter_mut().zip(logical.iter()) {
                    *dst = dst.saturating_add(src);
                }
            }
            ChannelLayout::Contiguous { start, len: 2 } => {
                let c0 = start;
                let c1 = start + 1;
                assert!(c1 < physical_channels);
                for (src, dst) in logical
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(physical.chunks_exact_mut(physical_channels))
                {
                    dst[c0] = dst[c0].saturating_add(src[0]);
                    dst[c1] = dst[c1].saturating_add(src[1]);
                }
            }
            ChannelLayout::Contiguous { start, len } => {
                let end = start + len;
                assert!(end <= physical_channels);
                for (src, dst) in logical
                    .chunks_exact(len)
                    .zip(physical.chunks_exact_mut(physical_channels))
                {
                    for (d, &s) in dst[start..end].iter_mut().zip(src.iter()) {
                        *d = d.saturating_add(s);
                    }
                }
            }
            ChannelLayout::Pair(c0, c1) => {
                assert!(c0 < physical_channels && c1 < physical_channels);
                for (src, dst) in logical
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(physical.chunks_exact_mut(physical_channels))
                {
                    dst[c0] = dst[c0].saturating_add(src[0]);
                    dst[c1] = dst[c1].saturating_add(src[1]);
                }
            }
            ChannelLayout::General => {
                let logical_channels = self.channels.len();
                for (src, dst) in logical
                    .chunks_exact(logical_channels)
                    .zip(physical.chunks_exact_mut(physical_channels))
                {
                    for (logical_channel, &physical_channel) in self.channels.iter().enumerate() {
                        dst[physical_channel] =
                            dst[physical_channel].saturating_add(src[logical_channel]);
                    }
                }
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
    ) -> Result<(usize, usize), RoutingError> {
        if physical_channels == 0 {
            return Err(RoutingError::ZeroPhysicalChannels);
        }
        if self
            .max_channel
            .is_some_and(|channel| channel >= physical_channels)
        {
            let channel = self
                .channels
                .iter()
                .copied()
                .find(|&channel| channel >= physical_channels)
                .unwrap_or(physical_channels);
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
        Ok((physical_samples, logical_samples))
    }
}

fn classify_channels(channels: &[usize]) -> (ChannelLayout, Option<usize>) {
    let Some(max_channel) = channels.iter().copied().max() else {
        return (ChannelLayout::Empty, None);
    };
    let layout = if channels.len() == 1 {
        ChannelLayout::Mono(channels[0])
    } else {
        let start = channels[0];
        let contiguous = channels
            .iter()
            .enumerate()
            .all(|(offset, &channel)| start.checked_add(offset) == Some(channel));
        if contiguous {
            ChannelLayout::Contiguous {
                start,
                len: channels.len(),
            }
        } else if channels.len() == 2 {
            ChannelLayout::Pair(channels[0], channels[1])
        } else {
            ChannelLayout::General
        }
    };
    (layout, Some(max_channel))
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
            let (layout, max_channel) = classify_channels(&channels);
            Ok(CompiledPort {
                id: port.id.clone().into_boxed_str(),
                name: port.name.clone().into_boxed_str(),
                channels,
                max_channel,
                layout,
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

    #[test]
    fn fast_paths_match_reference_across_all_channel_layouts() {
        let layouts: &[(&str, &str)] = &[
            ("mono", "[5]"),
            ("stereo", "[2, 3]"),
            ("triple_contig", "[4, 5, 6]"),
            ("pair_sparse", "[7, 1]"),
            ("general", "[6, 0, 3]"),
            ("full", "[0, 1, 2, 3, 4, 5, 6, 7]"),
        ];
        let physical_channels = 8;
        let frames = 5;
        let source_physical: Vec<i32> = (0..(frames * physical_channels) as i32)
            .map(|i| i * 100 - 500)
            .collect();

        for &(id, channels) in layouts {
            let profile_toml = format!(
                r#"
                [device]
                name = "Layout matrix"
                rate = 48000
                period_size = 64
                buffer_size = 128

                [device.playback]
                device = "hw:Test,0"
                channels = 8
                format = "S32_LE"

                [device.capture]
                device = "hw:Test,0"
                channels = 8
                format = "S32_LE"

                [[ports.playback]]
                id = "{id}"
                name = "{id}"
                channels = {channels}
                "#
            );
            let profile = Profile::from_toml(&profile_toml).expect("profile should parse");
            let routing = RoutingTable::compile(&profile).expect("routing should compile");
            let port = routing.find_playback(id).expect("port should exist");
            let logical_channels = port.channels().len();
            let mut logical = vec![0_i32; frames * logical_channels];
            port.copy_from_physical(&source_physical, physical_channels, frames, &mut logical)
                .expect("copy_from_physical should succeed");

            let mut expected_logical = vec![0_i32; frames * logical_channels];
            for frame in 0..frames {
                for (lch, &pch) in port.channels().iter().enumerate() {
                    expected_logical[frame * logical_channels + lch] =
                        source_physical[frame * physical_channels + pch];
                }
            }
            assert_eq!(logical, expected_logical, "copy_from {}", port.id());

            let mut dst_physical = vec![-7_i32; frames * physical_channels];
            port.copy_to_physical(&logical, physical_channels, frames, &mut dst_physical)
                .expect("copy_to_physical should succeed");
            let mut expected_physical = vec![-7_i32; frames * physical_channels];
            for frame in 0..frames {
                for (lch, &pch) in port.channels().iter().enumerate() {
                    expected_physical[frame * physical_channels + pch] =
                        logical[frame * logical_channels + lch];
                }
            }
            assert_eq!(dst_physical, expected_physical, "copy_to {}", port.id());

            let mut mixed = vec![i32::MAX - 10; frames * physical_channels];
            let mut expected_mixed = mixed.clone();
            port.add_to_physical_saturating(&logical, physical_channels, frames, &mut mixed)
                .expect("add_to_physical_saturating should succeed");
            for frame in 0..frames {
                for (lch, &pch) in port.channels().iter().enumerate() {
                    let idx = frame * physical_channels + pch;
                    expected_mixed[idx] =
                        expected_mixed[idx].saturating_add(logical[frame * logical_channels + lch]);
                }
            }
            assert_eq!(mixed, expected_mixed, "add_sat {}", port.id());

            // Zero frames still validates channel range and leaves buffers untouched.
            port.copy_from_physical::<i32>(&[], physical_channels, 0, &mut [])
                .expect("zero frames should succeed");
            if port.channels().iter().any(|&ch| ch >= 4) {
                let first_oob = port.channels().iter().copied().find(|&ch| ch >= 4).unwrap();
                let err = port
                    .copy_from_physical::<i32>(&[], 4, 0, &mut [])
                    .expect_err("out-of-range channel must fail even for zero frames");
                assert!(matches!(
                    err,
                    RoutingError::PhysicalChannelOutOfRange {
                        channel,
                        physical_channels: 4
                    } if channel == first_oob
                ));
            }
        }
    }

    #[test]
    #[ignore = "microbenchmark"]
    fn benchmark_compiled_port_routing_hot_paths() {
        let profile_toml = r#"
            [device]
            name = "Bench"
            rate = 48000
            period_size = 64
            buffer_size = 128

            [device.playback]
            device = "hw:Test,0"
            channels = 8
            format = "S32_LE"

            [device.capture]
            device = "hw:Test,0"
            channels = 10
            format = "S32_LE"

            [[ports.playback]]
            id = "line1"
            name = "Line 1"
            channels = [0, 1]

            [[ports.capture]]
            id = "mic1"
            name = "Mic 1"
            channels = [0]
        "#;
        let profile = Profile::from_toml(profile_toml).unwrap();
        let routing = RoutingTable::compile(&profile).unwrap();
        let stereo = routing.find_playback("line1").unwrap();
        let mono = routing.find_capture("mic1").unwrap();

        let frames = 64;
        let iterations = 200_000_usize;
        let mut physical = vec![1234_i32; frames * 8];
        let mut capture = vec![5678_i32; frames * 10];
        let mut logical_stereo = vec![99_i32; frames * 2];
        let mut logical_mono = vec![0_i32; frames];

        let t0 = std::time::Instant::now();
        for _ in 0..iterations {
            stereo
                .add_to_physical_saturating(
                    std::hint::black_box(&logical_stereo),
                    8,
                    frames,
                    std::hint::black_box(&mut physical),
                )
                .unwrap();
        }
        let mix_ns = t0.elapsed().as_nanos() as f64 / iterations as f64;

        let t1 = std::time::Instant::now();
        for _ in 0..iterations {
            stereo
                .copy_from_physical(
                    std::hint::black_box(&physical),
                    8,
                    frames,
                    std::hint::black_box(&mut logical_stereo),
                )
                .unwrap();
        }
        let stereo_copy_ns = t1.elapsed().as_nanos() as f64 / iterations as f64;

        let t2 = std::time::Instant::now();
        for _ in 0..iterations {
            mono.copy_from_physical(
                std::hint::black_box(&capture),
                10,
                frames,
                std::hint::black_box(&mut logical_mono),
            )
            .unwrap();
        }
        let mono_copy_ns = t2.elapsed().as_nanos() as f64 / iterations as f64;

        std::hint::black_box((&mut capture, &physical, &logical_stereo, &logical_mono));
        eprintln!(
            "routing Q64: stereo_add={mix_ns:.1}ns stereo_copy={stereo_copy_ns:.1}ns mono_copy={mono_copy_ns:.1}ns"
        );
    }
}
