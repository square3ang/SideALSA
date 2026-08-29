use std::{
    io::{self, Read, Write},
    mem::{align_of, size_of},
    sync::atomic::{AtomicU32, AtomicU64},
};

use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 16;
pub const PROTOCOL_MAGIC: [u8; 4] = *b"SALS";
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;
pub const FEATURE_PRO: u32 = 1 << 0;
pub const FEATURE_SHARED: u32 = 1 << 1;

pub const SHARED_MAGIC: u32 = u32::from_le_bytes(*b"SASH");
pub const SHARED_VERSION: u16 = 9;
pub const SHARED_SLOT_COUNT: u32 = 8;
pub const SHARED_SLOT_FREE: u32 = 0;
pub const SHARED_SLOT_READY: u32 = 1;
pub const SHARED_SLOT_WRITING: u32 = 2;
pub const SHARED_SLOT_READING: u32 = 3;
pub const SHARED_CLIENT_IDLE: u32 = 0;
pub const SHARED_CLIENT_STARTING: u32 = 1;
pub const SHARED_CLIENT_RUNNING: u32 = 2;
pub const SHARED_ACTIVATION_PENDING: u64 = 0;
pub const SHARED_ACTIVATION_CLAIMED: u64 = 1;
pub const SHARED_ACTIVATION_READY: u64 = 2;
pub const SHARED_ALIGNMENT: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Hello { version: u16 },
    GetInfo,
    OpenPro,
    OpenShared { port_id: String },
    Start { session_id: u64 },
    Stop { session_id: u64 },
    Close { session_id: u64 },
    GetStats,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Hello {
        version: u16,
        features: u32,
    },
    Info(DeviceInfo),
    OpenPro {
        session_id: u64,
        shared: SharedRegionInfo,
    },
    OpenShared {
        session_id: u64,
        direction: PortDirection,
        shared: SharedRegionInfo,
    },
    Ack,
    Busy,
    Unsupported,
    Stats(Box<Stats>),
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub profile_fingerprint: u64,
    pub rate: u32,
    pub period_size: u32,
    pub hardware_period_size: u32,
    pub buffer_size: u32,
    pub shared_buffer_size: u32,
    pub pro_latency_periods: u32,
    pub pro_output_latency_frames: u32,
    pub pro_realtime_priority: u32,
    pub shared_latency_periods: u32,
    pub playback_channels: u32,
    pub capture_channels: u32,
    pub playback_ports: Vec<PortInfo>,
    pub capture_ports: Vec<PortInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortInfo {
    pub id: String,
    pub name: String,
    pub channels: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PortDirection {
    Playback = 1,
    Capture = 2,
}

impl TryFrom<u8> for PortDirection {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Playback),
            2 => Ok(Self::Capture),
            _ => Err(ProtocolError::UnknownDirection(value)),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
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
    pub duplex_pointer_phase_nanos: i64,
    pub duplex_pointer_phase_min_nanos: i64,
    pub duplex_pointer_phase_max_nanos: i64,
    pub duplex_pointer_phase_samples: u64,
    pub linked_phase_attempts: u64,
    pub linked_phase_rebases: u64,
    pub linked_phase_score_nanos: u64,
    pub linked_phase_target_met: bool,
    pub playback_low_watermarks: u64,
    pub pro_deadline_misses: u64,
    pub pro_client_deadline_misses: u64,
    pub pro_core_deadline_misses: u64,
    pub pro_capture_overruns: u64,
    pub pro_expired_capture_blocks: u64,
    pub pro_playback_submit_failures: u64,
    pub pro_realtime_failures: u64,
    pub pro_callback_overruns: u64,
    pub pro_callback_max_nanos: u64,
    pub pro_playback_blocks: u64,
    pub pro_playback_nonzero_blocks: u64,
    pub shared_underruns: u64,
    pub shared_overruns: u64,
    pub timeline_resets: u64,
    pub periods_processed: u64,
    pub shared_playback_ports: Vec<SharedPlaybackPortStats>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SharedPlaybackPortStats {
    pub port_id: String,
    pub underruns: u64,
    pub last_underrun_sequence: u64,
    pub last_underrun_nanos: u64,
    pub last_sequence_lag_periods: u64,
    pub max_sequence_lag_periods: u64,
    pub expired_playback_periods: u64,
    pub playback_submit_failures: u64,
    pub playback_xruns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedRegionInfo {
    pub size: u64,
    pub period_frames: u32,
    pub playback_channels: u32,
    pub capture_channels: u32,
    pub slot_count: u32,
    pub slot_stride: u64,
    pub capture_offset: u64,
    pub playback_offset: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
enum RequestCode {
    Hello = 1,
    GetInfo = 2,
    OpenPro = 3,
    OpenShared = 4,
    Start = 5,
    Stop = 6,
    Close = 7,
    GetStats = 8,
}

impl TryFrom<u16> for RequestCode {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::GetInfo),
            3 => Ok(Self::OpenPro),
            4 => Ok(Self::OpenShared),
            5 => Ok(Self::Start),
            6 => Ok(Self::Stop),
            7 => Ok(Self::Close),
            8 => Ok(Self::GetStats),
            _ => Err(ProtocolError::UnknownRequest(value)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
enum ResponseCode {
    Hello = 1,
    Info = 2,
    OpenPro = 3,
    OpenShared = 9,
    Ack = 4,
    Busy = 5,
    Unsupported = 6,
    Stats = 7,
    Error = 8,
}

impl TryFrom<u16> for ResponseCode {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, ProtocolError> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::Info),
            3 => Ok(Self::OpenPro),
            9 => Ok(Self::OpenShared),
            4 => Ok(Self::Ack),
            5 => Ok(Self::Busy),
            6 => Ok(Self::Unsupported),
            7 => Ok(Self::Stats),
            8 => Ok(Self::Error),
            _ => Err(ProtocolError::UnknownResponse(value)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum ErrorCode {
    InvalidRequest = 1,
    NotOwner = 2,
    BadState = 3,
    Internal = 4,
}

impl TryFrom<u16> for ErrorCode {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidRequest),
            2 => Ok(Self::NotOwner),
            3 => Ok(Self::BadState),
            4 => Ok(Self::Internal),
            _ => Err(ProtocolError::UnknownError(value)),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid protocol magic")]
    InvalidMagic,
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("frame payload too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("unknown request code {0}")]
    UnknownRequest(u16),
    #[error("unknown response code {0}")]
    UnknownResponse(u16),
    #[error("unknown error code {0}")]
    UnknownError(u16),
    #[error("unknown port direction {0}")]
    UnknownDirection(u8),
    #[error("malformed payload")]
    MalformedPayload,
    #[error("string too long: {0} bytes")]
    StringTooLong(usize),
    #[error("too many channels or ports")]
    TooManyItems,
    #[error("shared layout arithmetic overflow")]
    LayoutOverflow,
}

pub fn write_request<W: Write>(writer: &mut W, request: &Request) -> Result<(), ProtocolError> {
    writer.write_all(&encode_request(request)?)?;
    Ok(())
}

pub fn read_request<R: Read>(reader: &mut R) -> Result<Request, ProtocolError> {
    decode_request(&read_frame(reader)?)
}

pub fn write_response<W: Write>(writer: &mut W, response: &Response) -> Result<(), ProtocolError> {
    writer.write_all(&encode_response(response)?)?;
    Ok(())
}

pub fn read_response<R: Read>(reader: &mut R) -> Result<Response, ProtocolError> {
    decode_response(&read_frame(reader)?)
}

pub fn encode_request(request: &Request) -> Result<Vec<u8>, ProtocolError> {
    let (code, payload) = encode_request_payload(request)?;
    Ok(encode_frame(code as u16, &payload)?.into_iter().collect())
}

pub fn encode_response(response: &Response) -> Result<Vec<u8>, ProtocolError> {
    let (code, payload) = encode_response_payload(response)?;
    encode_frame(code as u16, &payload)
}

pub fn decode_request(frame: &[u8]) -> Result<Request, ProtocolError> {
    let (code, payload) = decode_frame(frame)?;
    decode_request_payload(RequestCode::try_from(code)?, payload)
}

pub fn decode_response(frame: &[u8]) -> Result<Response, ProtocolError> {
    let (code, payload) = decode_frame(frame)?;
    decode_response_payload(ResponseCode::try_from(code)?, payload)
}

fn encode_frame(code: u16, payload: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(ProtocolError::FrameTooLarge(payload.len()));
    }
    let mut frame = Vec::with_capacity(12 + payload.len());
    frame.extend_from_slice(&PROTOCOL_MAGIC);
    frame.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    frame.extend_from_slice(&code.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

fn read_frame<R: Read>(reader: &mut R) -> Result<Vec<u8>, ProtocolError> {
    let mut header = [0_u8; 12];
    reader.read_exact(&mut header)?;
    if header[..4] != PROTOCOL_MAGIC {
        return Err(ProtocolError::InvalidMagic);
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    let payload_len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    if payload_len > MAX_FRAME_PAYLOAD {
        return Err(ProtocolError::FrameTooLarge(payload_len));
    }
    let mut frame = Vec::with_capacity(12 + payload_len);
    frame.extend_from_slice(&header);
    frame.resize(12 + payload_len, 0);
    reader.read_exact(&mut frame[12..])?;
    Ok(frame)
}

fn decode_frame(frame: &[u8]) -> Result<(u16, &[u8]), ProtocolError> {
    if frame.len() < 12 {
        return Err(ProtocolError::MalformedPayload);
    }
    if frame[..4] != PROTOCOL_MAGIC {
        return Err(ProtocolError::InvalidMagic);
    }
    let version = u16::from_le_bytes([frame[4], frame[5]]);
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    let code = u16::from_le_bytes([frame[6], frame[7]]);
    let payload_len = u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]) as usize;
    if payload_len > MAX_FRAME_PAYLOAD {
        return Err(ProtocolError::FrameTooLarge(payload_len));
    }
    if frame.len() != 12 + payload_len {
        return Err(ProtocolError::MalformedPayload);
    }
    Ok((code, &frame[12..]))
}

fn encode_request_payload(request: &Request) -> Result<(RequestCode, Vec<u8>), ProtocolError> {
    let mut payload = Vec::new();
    let code = match request {
        Request::Hello { version } => {
            put_u16(&mut payload, *version);
            RequestCode::Hello
        }
        Request::GetInfo => RequestCode::GetInfo,
        Request::OpenPro => RequestCode::OpenPro,
        Request::OpenShared { port_id } => {
            put_string(&mut payload, port_id)?;
            RequestCode::OpenShared
        }
        Request::Start { session_id } => {
            put_u64(&mut payload, *session_id);
            RequestCode::Start
        }
        Request::Stop { session_id } => {
            put_u64(&mut payload, *session_id);
            RequestCode::Stop
        }
        Request::Close { session_id } => {
            put_u64(&mut payload, *session_id);
            RequestCode::Close
        }
        Request::GetStats => RequestCode::GetStats,
    };
    Ok((code, payload))
}

fn decode_request_payload(code: RequestCode, payload: &[u8]) -> Result<Request, ProtocolError> {
    let mut decoder = Decoder::new(payload);
    let request = match code {
        RequestCode::Hello => Request::Hello {
            version: decoder.u16()?,
        },
        RequestCode::GetInfo => Request::GetInfo,
        RequestCode::OpenPro => Request::OpenPro,
        RequestCode::OpenShared => Request::OpenShared {
            port_id: decoder.string()?,
        },
        RequestCode::Start => Request::Start {
            session_id: decoder.u64()?,
        },
        RequestCode::Stop => Request::Stop {
            session_id: decoder.u64()?,
        },
        RequestCode::Close => Request::Close {
            session_id: decoder.u64()?,
        },
        RequestCode::GetStats => Request::GetStats,
    };
    decoder.finish()?;
    Ok(request)
}

fn encode_response_payload(response: &Response) -> Result<(ResponseCode, Vec<u8>), ProtocolError> {
    let mut payload = Vec::new();
    let code = match response {
        Response::Hello { version, features } => {
            put_u16(&mut payload, *version);
            put_u32(&mut payload, *features);
            ResponseCode::Hello
        }
        Response::Info(info) => {
            encode_info(&mut payload, info)?;
            ResponseCode::Info
        }
        Response::OpenPro { session_id, shared } => {
            put_u64(&mut payload, *session_id);
            encode_shared_info(&mut payload, shared);
            ResponseCode::OpenPro
        }
        Response::OpenShared {
            session_id,
            direction,
            shared,
        } => {
            put_u64(&mut payload, *session_id);
            put_u8(&mut payload, *direction as u8);
            encode_shared_info(&mut payload, shared);
            ResponseCode::OpenShared
        }
        Response::Ack => ResponseCode::Ack,
        Response::Busy => ResponseCode::Busy,
        Response::Unsupported => ResponseCode::Unsupported,
        Response::Stats(stats) => {
            encode_stats(&mut payload, stats)?;
            ResponseCode::Stats
        }
        Response::Error { code, message } => {
            put_u16(&mut payload, *code as u16);
            put_string(&mut payload, message)?;
            ResponseCode::Error
        }
    };
    Ok((code, payload))
}

fn decode_response_payload(code: ResponseCode, payload: &[u8]) -> Result<Response, ProtocolError> {
    let mut decoder = Decoder::new(payload);
    let response = match code {
        ResponseCode::Hello => Response::Hello {
            version: decoder.u16()?,
            features: decoder.u32()?,
        },
        ResponseCode::Info => Response::Info(decode_info(&mut decoder)?),
        ResponseCode::OpenPro => Response::OpenPro {
            session_id: decoder.u64()?,
            shared: decode_shared_info(&mut decoder)?,
        },
        ResponseCode::OpenShared => Response::OpenShared {
            session_id: decoder.u64()?,
            direction: PortDirection::try_from(decoder.u8()?)?,
            shared: decode_shared_info(&mut decoder)?,
        },
        ResponseCode::Ack => Response::Ack,
        ResponseCode::Busy => Response::Busy,
        ResponseCode::Unsupported => Response::Unsupported,
        ResponseCode::Stats => Response::Stats(Box::new(decode_stats(&mut decoder)?)),
        ResponseCode::Error => Response::Error {
            code: ErrorCode::try_from(decoder.u16()?)?,
            message: decoder.string()?,
        },
    };
    decoder.finish()?;
    Ok(response)
}

fn encode_info(payload: &mut Vec<u8>, info: &DeviceInfo) -> Result<(), ProtocolError> {
    put_string(payload, &info.name)?;
    put_u64(payload, info.profile_fingerprint);
    put_u32(payload, info.rate);
    put_u32(payload, info.period_size);
    put_u32(payload, info.hardware_period_size);
    put_u32(payload, info.buffer_size);
    put_u32(payload, info.shared_buffer_size);
    put_u32(payload, info.pro_latency_periods);
    put_u32(payload, info.pro_output_latency_frames);
    put_u32(payload, info.pro_realtime_priority);
    put_u32(payload, info.shared_latency_periods);
    put_u32(payload, info.playback_channels);
    put_u32(payload, info.capture_channels);
    encode_ports(payload, &info.playback_ports)?;
    encode_ports(payload, &info.capture_ports)
}

fn decode_info(decoder: &mut Decoder<'_>) -> Result<DeviceInfo, ProtocolError> {
    Ok(DeviceInfo {
        name: decoder.string()?,
        profile_fingerprint: decoder.u64()?,
        rate: decoder.u32()?,
        period_size: decoder.u32()?,
        hardware_period_size: decoder.u32()?,
        buffer_size: decoder.u32()?,
        shared_buffer_size: decoder.u32()?,
        pro_latency_periods: decoder.u32()?,
        pro_output_latency_frames: decoder.u32()?,
        pro_realtime_priority: decoder.u32()?,
        shared_latency_periods: decoder.u32()?,
        playback_channels: decoder.u32()?,
        capture_channels: decoder.u32()?,
        playback_ports: decode_ports(decoder)?,
        capture_ports: decode_ports(decoder)?,
    })
}

fn encode_ports(payload: &mut Vec<u8>, ports: &[PortInfo]) -> Result<(), ProtocolError> {
    let count = u16::try_from(ports.len()).map_err(|_| ProtocolError::TooManyItems)?;
    put_u16(payload, count);
    for port in ports {
        put_string(payload, &port.id)?;
        put_string(payload, &port.name)?;
        let channel_count =
            u16::try_from(port.channels.len()).map_err(|_| ProtocolError::TooManyItems)?;
        put_u16(payload, channel_count);
        for &channel in &port.channels {
            put_u32(payload, channel);
        }
    }
    Ok(())
}

fn decode_ports(decoder: &mut Decoder<'_>) -> Result<Vec<PortInfo>, ProtocolError> {
    let count = decoder.u16()? as usize;
    let mut ports = Vec::with_capacity(count);
    for _ in 0..count {
        let id = decoder.string()?;
        let name = decoder.string()?;
        let channel_count = decoder.u16()? as usize;
        let mut channels = Vec::with_capacity(channel_count);
        for _ in 0..channel_count {
            channels.push(decoder.u32()?);
        }
        ports.push(PortInfo { id, name, channels });
    }
    Ok(ports)
}

fn encode_shared_info(payload: &mut Vec<u8>, info: &SharedRegionInfo) {
    put_u64(payload, info.size);
    put_u32(payload, info.period_frames);
    put_u32(payload, info.playback_channels);
    put_u32(payload, info.capture_channels);
    put_u32(payload, info.slot_count);
    put_u64(payload, info.slot_stride);
    put_u64(payload, info.capture_offset);
    put_u64(payload, info.playback_offset);
}

fn decode_shared_info(decoder: &mut Decoder<'_>) -> Result<SharedRegionInfo, ProtocolError> {
    Ok(SharedRegionInfo {
        size: decoder.u64()?,
        period_frames: decoder.u32()?,
        playback_channels: decoder.u32()?,
        capture_channels: decoder.u32()?,
        slot_count: decoder.u32()?,
        slot_stride: decoder.u64()?,
        capture_offset: decoder.u64()?,
        playback_offset: decoder.u64()?,
    })
}

fn encode_stats(payload: &mut Vec<u8>, stats: &Stats) -> Result<(), ProtocolError> {
    for value in [
        stats.generation,
        stats.sample_position,
        stats.playback_position,
        stats.capture_position,
        stats.hw_playback_xruns,
        stats.hw_capture_xruns,
        stats.playback_delay_frames,
        stats.capture_delay_frames,
        stats.playback_delay_min_frames,
        stats.playback_delay_max_frames,
        stats.playback_ring_delay_frames,
        stats.playback_ring_delay_min_frames,
        stats.playback_ring_delay_max_frames,
        stats.playback_driver_delay_frames,
        stats.playback_driver_delay_min_frames,
        stats.playback_driver_delay_max_frames,
        stats.capture_delay_min_frames,
        stats.capture_delay_max_frames,
        stats.playback_target_overshoot_max_frames,
        stats.capture_clock_wait_max_nanos,
        stats.pro_wait_budget_min_nanos,
        stats.pro_wait_budget_max_nanos,
        stats.pro_ready_wait_max_nanos,
        stats.playback_write_max_nanos,
        stats.capture_to_playback_write_nanos,
        stats.capture_to_playback_write_min_nanos,
        stats.capture_to_playback_write_max_nanos,
        stats.duplex_pointer_phase_nanos as u64,
        stats.duplex_pointer_phase_min_nanos as u64,
        stats.duplex_pointer_phase_max_nanos as u64,
        stats.duplex_pointer_phase_samples,
        stats.linked_phase_attempts,
        stats.linked_phase_rebases,
        stats.linked_phase_score_nanos,
        u64::from(stats.linked_phase_target_met),
        stats.playback_low_watermarks,
        stats.pro_deadline_misses,
        stats.pro_client_deadline_misses,
        stats.pro_core_deadline_misses,
        stats.pro_capture_overruns,
        stats.pro_expired_capture_blocks,
        stats.pro_playback_submit_failures,
        stats.pro_realtime_failures,
        stats.pro_callback_overruns,
        stats.pro_callback_max_nanos,
        stats.pro_playback_blocks,
        stats.pro_playback_nonzero_blocks,
        stats.shared_underruns,
        stats.shared_overruns,
        stats.timeline_resets,
        stats.periods_processed,
    ] {
        put_u64(payload, value);
    }
    let count = u16::try_from(stats.shared_playback_ports.len())
        .map_err(|_| ProtocolError::TooManyItems)?;
    put_u16(payload, count);
    for port in &stats.shared_playback_ports {
        put_string(payload, &port.port_id)?;
        for value in [
            port.underruns,
            port.last_underrun_sequence,
            port.last_underrun_nanos,
            port.last_sequence_lag_periods,
            port.max_sequence_lag_periods,
            port.expired_playback_periods,
            port.playback_submit_failures,
            port.playback_xruns,
        ] {
            put_u64(payload, value);
        }
    }
    Ok(())
}

fn decode_stats(decoder: &mut Decoder<'_>) -> Result<Stats, ProtocolError> {
    let mut stats = Stats {
        generation: decoder.u64()?,
        sample_position: decoder.u64()?,
        playback_position: decoder.u64()?,
        capture_position: decoder.u64()?,
        hw_playback_xruns: decoder.u64()?,
        hw_capture_xruns: decoder.u64()?,
        playback_delay_frames: decoder.u64()?,
        capture_delay_frames: decoder.u64()?,
        playback_delay_min_frames: decoder.u64()?,
        playback_delay_max_frames: decoder.u64()?,
        playback_ring_delay_frames: decoder.u64()?,
        playback_ring_delay_min_frames: decoder.u64()?,
        playback_ring_delay_max_frames: decoder.u64()?,
        playback_driver_delay_frames: decoder.u64()?,
        playback_driver_delay_min_frames: decoder.u64()?,
        playback_driver_delay_max_frames: decoder.u64()?,
        capture_delay_min_frames: decoder.u64()?,
        capture_delay_max_frames: decoder.u64()?,
        playback_target_overshoot_max_frames: decoder.u64()?,
        capture_clock_wait_max_nanos: decoder.u64()?,
        pro_wait_budget_min_nanos: decoder.u64()?,
        pro_wait_budget_max_nanos: decoder.u64()?,
        pro_ready_wait_max_nanos: decoder.u64()?,
        playback_write_max_nanos: decoder.u64()?,
        capture_to_playback_write_nanos: decoder.u64()?,
        capture_to_playback_write_min_nanos: decoder.u64()?,
        capture_to_playback_write_max_nanos: decoder.u64()?,
        duplex_pointer_phase_nanos: decoder.u64()? as i64,
        duplex_pointer_phase_min_nanos: decoder.u64()? as i64,
        duplex_pointer_phase_max_nanos: decoder.u64()? as i64,
        duplex_pointer_phase_samples: decoder.u64()?,
        linked_phase_attempts: decoder.u64()?,
        linked_phase_rebases: decoder.u64()?,
        linked_phase_score_nanos: decoder.u64()?,
        linked_phase_target_met: decoder.u64()? != 0,
        playback_low_watermarks: decoder.u64()?,
        pro_deadline_misses: decoder.u64()?,
        pro_client_deadline_misses: decoder.u64()?,
        pro_core_deadline_misses: decoder.u64()?,
        pro_capture_overruns: decoder.u64()?,
        pro_expired_capture_blocks: decoder.u64()?,
        pro_playback_submit_failures: decoder.u64()?,
        pro_realtime_failures: decoder.u64()?,
        pro_callback_overruns: decoder.u64()?,
        pro_callback_max_nanos: decoder.u64()?,
        pro_playback_blocks: decoder.u64()?,
        pro_playback_nonzero_blocks: decoder.u64()?,
        shared_underruns: decoder.u64()?,
        shared_overruns: decoder.u64()?,
        timeline_resets: decoder.u64()?,
        periods_processed: decoder.u64()?,
        shared_playback_ports: Vec::new(),
    };
    let port_count = decoder.u16()? as usize;
    stats.shared_playback_ports.reserve(port_count);
    for _ in 0..port_count {
        stats.shared_playback_ports.push(SharedPlaybackPortStats {
            port_id: decoder.string()?,
            underruns: decoder.u64()?,
            last_underrun_sequence: decoder.u64()?,
            last_underrun_nanos: decoder.u64()?,
            last_sequence_lag_periods: decoder.u64()?,
            max_sequence_lag_periods: decoder.u64()?,
            expired_playback_periods: decoder.u64()?,
            playback_submit_failures: decoder.u64()?,
            playback_xruns: decoder.u64()?,
        });
    }
    Ok(stats)
}

fn put_u16(payload: &mut Vec<u8>, value: u16) {
    payload.extend_from_slice(&value.to_le_bytes());
}

fn put_u8(payload: &mut Vec<u8>, value: u8) {
    payload.push(value);
}

fn put_u32(payload: &mut Vec<u8>, value: u32) {
    payload.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(payload: &mut Vec<u8>, value: u64) {
    payload.extend_from_slice(&value.to_le_bytes());
}

fn put_string(payload: &mut Vec<u8>, value: &str) -> Result<(), ProtocolError> {
    let bytes = value.as_bytes();
    let length =
        u16::try_from(bytes.len()).map_err(|_| ProtocolError::StringTooLong(bytes.len()))?;
    put_u16(payload, length);
    payload.extend_from_slice(bytes);
    Ok(())
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], ProtocolError> {
        let end = self
            .position
            .checked_add(N)
            .ok_or(ProtocolError::MalformedPayload)?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(ProtocolError::MalformedPayload)?;
        self.position = end;
        bytes
            .try_into()
            .map_err(|_| ProtocolError::MalformedPayload)
    }

    fn u16(&mut self) -> Result<u16, ProtocolError> {
        Ok(u16::from_le_bytes(self.take()?))
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take::<1>()?[0])
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn u64(&mut self) -> Result<u64, ProtocolError> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    fn string(&mut self) -> Result<String, ProtocolError> {
        let length = self.u16()? as usize;
        let end = self
            .position
            .checked_add(length)
            .ok_or(ProtocolError::MalformedPayload)?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(ProtocolError::MalformedPayload)?;
        self.position = end;
        String::from_utf8(bytes.to_vec()).map_err(|_| ProtocolError::MalformedPayload)
    }

    fn finish(&self) -> Result<(), ProtocolError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(ProtocolError::MalformedPayload)
        }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct SharedRegionHeader {
    pub magic: u32,
    pub version: u16,
    pub header_size: u16,
    pub total_size: u64,
    pub period_frames: u32,
    pub playback_channels: u32,
    pub capture_channels: u32,
    pub slot_count: u32,
    pub slot_stride: u64,
    pub capture_offset: u64,
    pub playback_offset: u64,
    pub cycle_sequence: AtomicU64,
    pub playback_sequence: AtomicU64,
    pub lifecycle_generation: AtomicU64,
    pub hardware_generation: AtomicU64,
    pub client_state: AtomicU32,
    pub activation_state: AtomicU64,
    pub start_sequence: AtomicU64,
    pub capture_discontinuities: AtomicU64,
    pub client_expired_capture_blocks: AtomicU64,
    pub client_expired_playback_periods: AtomicU64,
    pub client_playback_submit_failures: AtomicU64,
    pub client_playback_xruns: AtomicU64,
    pub client_playback_sequence: AtomicU64,
    pub client_realtime_failures: AtomicU64,
    pub client_callback_overruns: AtomicU64,
    pub client_callback_max_nanos: AtomicU64,
}

impl SharedRegionHeader {
    pub fn new(layout: &SharedRegionLayout) -> Self {
        let info = layout.info();
        Self {
            magic: SHARED_MAGIC,
            version: SHARED_VERSION,
            header_size: size_of::<Self>() as u16,
            total_size: info.size,
            period_frames: info.period_frames,
            playback_channels: info.playback_channels,
            capture_channels: info.capture_channels,
            slot_count: info.slot_count,
            slot_stride: info.slot_stride,
            capture_offset: info.capture_offset,
            playback_offset: info.playback_offset,
            cycle_sequence: AtomicU64::new(0),
            playback_sequence: AtomicU64::new(0),
            lifecycle_generation: AtomicU64::new(0),
            hardware_generation: AtomicU64::new(0),
            client_state: AtomicU32::new(SHARED_CLIENT_IDLE),
            activation_state: AtomicU64::new(SHARED_ACTIVATION_PENDING),
            start_sequence: AtomicU64::new(0),
            capture_discontinuities: AtomicU64::new(0),
            client_expired_capture_blocks: AtomicU64::new(0),
            client_expired_playback_periods: AtomicU64::new(0),
            client_playback_submit_failures: AtomicU64::new(0),
            client_playback_xruns: AtomicU64::new(0),
            client_playback_sequence: AtomicU64::new(0),
            client_realtime_failures: AtomicU64::new(0),
            client_callback_overruns: AtomicU64::new(0),
            client_callback_max_nanos: AtomicU64::new(0),
        }
    }
}

#[repr(C, align(64))]
pub struct SharedSlotHeader {
    pub state: AtomicU32,
    pub _reserved: u32,
    pub sequence: AtomicU64,
    pub published_nanos: AtomicU64,
}

impl SharedSlotHeader {
    pub fn new() -> Self {
        Self {
            state: AtomicU32::new(SHARED_SLOT_FREE),
            _reserved: 0,
            sequence: AtomicU64::new(0),
            published_nanos: AtomicU64::new(0),
        }
    }
}

impl Default for SharedSlotHeader {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedRegionLayout {
    size: usize,
    period_frames: u32,
    playback_channels: u32,
    capture_channels: u32,
    slot_count: u32,
    slot_stride: usize,
    capture_offset: usize,
    playback_offset: usize,
}

impl SharedRegionLayout {
    pub fn new(
        period_frames: u32,
        playback_channels: u32,
        capture_channels: u32,
        slot_count: u32,
    ) -> Result<Self, ProtocolError> {
        if period_frames == 0
            || (playback_channels == 0 && capture_channels == 0)
            || slot_count == 0
        {
            return Err(ProtocolError::MalformedPayload);
        }
        let max_samples = usize::try_from(period_frames.max(1))
            .ok()
            .and_then(|frames| {
                usize::try_from(playback_channels.max(capture_channels))
                    .ok()
                    .and_then(|channels| frames.checked_mul(channels))
            })
            .ok_or(ProtocolError::LayoutOverflow)?;
        let audio_bytes = max_samples
            .checked_mul(size_of::<i32>())
            .ok_or(ProtocolError::LayoutOverflow)?;
        let slot_stride = align_up(
            size_of::<SharedSlotHeader>()
                .checked_add(audio_bytes)
                .ok_or(ProtocolError::LayoutOverflow)?,
            SHARED_ALIGNMENT,
        )?;
        let capture_offset = align_up(size_of::<SharedRegionHeader>(), SHARED_ALIGNMENT)?;
        let ring_bytes = slot_stride
            .checked_mul(usize::try_from(slot_count).map_err(|_| ProtocolError::LayoutOverflow)?)
            .ok_or(ProtocolError::LayoutOverflow)?;
        let playback_offset = capture_offset
            .checked_add(ring_bytes)
            .ok_or(ProtocolError::LayoutOverflow)?;
        let size = playback_offset
            .checked_add(ring_bytes)
            .ok_or(ProtocolError::LayoutOverflow)?;
        Ok(Self {
            size,
            period_frames,
            playback_channels,
            capture_channels,
            slot_count,
            slot_stride,
            capture_offset,
            playback_offset,
        })
    }

    pub fn info(&self) -> SharedRegionInfo {
        SharedRegionInfo {
            size: self.size as u64,
            period_frames: self.period_frames,
            playback_channels: self.playback_channels,
            capture_channels: self.capture_channels,
            slot_count: self.slot_count,
            slot_stride: self.slot_stride as u64,
            capture_offset: self.capture_offset as u64,
            playback_offset: self.playback_offset as u64,
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn slot_stride(&self) -> usize {
        self.slot_stride
    }

    pub fn capture_offset(&self) -> usize {
        self.capture_offset
    }

    pub fn playback_offset(&self) -> usize {
        self.playback_offset
    }

    pub fn slot_count(&self) -> usize {
        self.slot_count as usize
    }

    pub fn slot_offset(&self, ring_offset: usize, index: usize) -> Option<usize> {
        if index >= self.slot_count as usize {
            return None;
        }
        ring_offset.checked_add(index.checked_mul(self.slot_stride)?)
    }
}

fn align_up(value: usize, alignment: usize) -> Result<usize, ProtocolError> {
    let remainder = value % alignment;
    if remainder == 0 {
        Ok(value)
    } else {
        value
            .checked_add(alignment - remainder)
            .ok_or(ProtocolError::LayoutOverflow)
    }
}

pub fn shared_slot_header_alignment() -> usize {
    align_of::<SharedSlotHeader>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_round_trip() {
        let request = Request::OpenShared {
            port_id: "line1".into(),
        };
        let bytes = encode_request(&request).expect("request should encode");
        let decoded = decode_request(&bytes).expect("request should decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn response_round_trip() {
        let response = Response::Stats(Box::new(Stats {
            generation: 4,
            sample_position: 128,
            playback_position: 64,
            capture_position: 96,
            hw_playback_xruns: 1,
            hw_capture_xruns: 2,
            playback_delay_frames: 128,
            capture_delay_frames: 32,
            playback_delay_min_frames: 64,
            playback_delay_max_frames: 192,
            playback_ring_delay_frames: 63,
            playback_ring_delay_min_frames: 31,
            playback_ring_delay_max_frames: 95,
            playback_driver_delay_frames: 160,
            playback_driver_delay_min_frames: 128,
            playback_driver_delay_max_frames: 192,
            capture_delay_min_frames: 0,
            capture_delay_max_frames: 64,
            playback_target_overshoot_max_frames: 4,
            capture_clock_wait_max_nanos: 20,
            pro_wait_budget_min_nanos: 30,
            pro_wait_budget_max_nanos: 40,
            pro_ready_wait_max_nanos: 50,
            playback_write_max_nanos: 60,
            capture_to_playback_write_nanos: 70,
            capture_to_playback_write_min_nanos: 65,
            capture_to_playback_write_max_nanos: 75,
            duplex_pointer_phase_nanos: -500_000,
            duplex_pointer_phase_min_nanos: -600_000,
            duplex_pointer_phase_max_nanos: -400_000,
            duplex_pointer_phase_samples: 128,
            linked_phase_attempts: 3,
            linked_phase_rebases: 2,
            linked_phase_score_nanos: 333_333,
            linked_phase_target_met: true,
            playback_low_watermarks: 1,
            pro_deadline_misses: 3,
            pro_client_deadline_misses: 4,
            pro_core_deadline_misses: 5,
            pro_capture_overruns: 6,
            pro_expired_capture_blocks: 7,
            pro_playback_submit_failures: 8,
            pro_realtime_failures: 9,
            pro_callback_overruns: 10,
            pro_callback_max_nanos: 11,
            pro_playback_blocks: 12,
            pro_playback_nonzero_blocks: 13,
            shared_underruns: 14,
            shared_overruns: 15,
            timeline_resets: 16,
            periods_processed: 17,
            shared_playback_ports: vec![SharedPlaybackPortStats {
                port_id: "line1".into(),
                underruns: 18,
                last_underrun_sequence: 19,
                last_underrun_nanos: 20,
                last_sequence_lag_periods: 21,
                max_sequence_lag_periods: 22,
                expired_playback_periods: 23,
                playback_submit_failures: 24,
                playback_xruns: 25,
            }],
        }));
        let bytes = encode_response(&response).expect("response should encode");
        let decoded = decode_response(&bytes).expect("response should decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn stream_helpers_read_and_write_frames() {
        let request = Request::GetInfo;
        let mut bytes = Vec::new();
        write_request(&mut bytes, &request).expect("request should write");
        let decoded = read_request(&mut Cursor::new(bytes)).expect("request should read");
        assert_eq!(decoded, request);
    }

    #[test]
    fn shared_response_round_trip() {
        let layout = SharedRegionLayout::new(32, 2, 0, SHARED_SLOT_COUNT)
            .expect("playback-only layout should compile");
        let response = Response::OpenShared {
            session_id: 7,
            direction: PortDirection::Playback,
            shared: layout.info(),
        };
        let bytes = encode_response(&response).expect("response should encode");
        let decoded = decode_response(&bytes).expect("response should decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn shared_layout_is_aligned_and_non_overlapping() {
        let layout =
            SharedRegionLayout::new(32, 8, 10, SHARED_SLOT_COUNT).expect("layout should compile");
        let info = layout.info();
        assert_eq!(shared_slot_header_alignment(), SHARED_ALIGNMENT);
        assert_eq!(layout.capture_offset() % SHARED_ALIGNMENT, 0);
        assert_eq!(layout.playback_offset() % SHARED_ALIGNMENT, 0);
        assert!(layout.playback_offset() >= layout.capture_offset() + layout.slot_stride());
        assert_eq!(info.size as usize, layout.size());
    }

    #[test]
    fn capture_only_layout_is_valid() {
        let layout = SharedRegionLayout::new(32, 0, 1, SHARED_SLOT_COUNT)
            .expect("capture-only layout should compile");
        assert_eq!(layout.info().playback_channels, 0);
        assert_eq!(layout.info().capture_channels, 1);
    }

    #[test]
    fn malformed_payload_is_rejected() {
        let bytes = encode_request(&Request::GetInfo).expect("request should encode");
        let truncated = &bytes[..bytes.len() - 1];
        assert!(matches!(
            decode_request(truncated),
            Err(ProtocolError::MalformedPayload)
        ));
    }
}
