//! USB Audio Class 2 descriptor parsing and clock control for the Sycon backend.

use std::{fs, io, path::PathBuf, thread, time::Duration};

use super::usbfs::UsbDevice;

const DT_CONFIG: u8 = 0x02;
const DT_INTERFACE: u8 = 0x04;
const DT_ENDPOINT: u8 = 0x05;
const DT_INTERFACE_ASSOCIATION: u8 = 0x0b;
const DT_CS_INTERFACE: u8 = 0x24;

const CLASS_AUDIO: u8 = 0x01;
const SUBCLASS_CONTROL: u8 = 0x01;
const SUBCLASS_STREAMING: u8 = 0x02;
const PROTOCOL_UAC2: u8 = 0x20;

const AC_INPUT_TERMINAL: u8 = 0x02;
const AC_OUTPUT_TERMINAL: u8 = 0x03;
const AC_CLOCK_SOURCE: u8 = 0x0a;
const AC_CLOCK_SELECTOR: u8 = 0x0b;
const AC_CLOCK_MULTIPLIER: u8 = 0x0c;
const AS_GENERAL: u8 = 0x01;
const AS_FORMAT_TYPE: u8 = 0x02;
const FORMAT_TYPE_I: u8 = 0x01;
const FORMAT_PCM: u32 = 0x01;

const REQUEST_GET: u8 = 0xa1;
const REQUEST_SET: u8 = 0x21;
const CUR: u8 = 0x01;
const RANGE: u8 = 0x02;
const CS_SAM_FREQ_CONTROL: u16 = 0x01;
const CS_CLOCK_VALID_CONTROL: u16 = 0x02;
const CX_CLOCK_SELECTOR_CONTROL: u16 = 0x01;

const EP_IN: u8 = 0x80;
const EP_XFER_ISOC: u8 = 0x01;
const EP_SYNC_MASK: u8 = 0x0c;
const EP_SYNC_ASYNC: u8 = 0x04;
const EP_USAGE_MASK: u8 = 0x30;
const EP_USAGE_DATA: u8 = 0x00;
const EP_USAGE_FEEDBACK: u8 = 0x10;
const EP_USAGE_IMPLICIT_FB: u8 = 0x20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub address: u8,
    pub attributes: u8,
    /// Bytes per service interval, including high-bandwidth transactions.
    pub max_packet_bytes: u32,
    /// log2 of the service interval in (micro)frames.
    pub interval_log2: u8,
}

impl Endpoint {
    fn is_in(self) -> bool {
        self.address & EP_IN != 0
    }

    fn is_iso(self) -> bool {
        self.attributes & 0x03 == EP_XFER_ISOC
    }

    fn usage(self) -> u8 {
        self.attributes & EP_USAGE_MASK
    }

    fn is_data(self) -> bool {
        matches!(self.usage(), EP_USAGE_DATA | EP_USAGE_IMPLICIT_FB)
    }

    pub(crate) fn is_async(self) -> bool {
        self.attributes & EP_SYNC_MASK == EP_SYNC_ASYNC
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StreamFormat {
    pub interface: u8,
    pub altsetting: u8,
    pub terminal_link: u8,
    pub channels: u8,
    pub subslot_bytes: u8,
    pub bit_resolution: u8,
    pub data: Endpoint,
    pub feedback: Option<Endpoint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClockEntity {
    Source { controls: u8 },
    Selector,
    Multiplier { source: u8 },
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AudioFunction {
    pub control_interface: Option<u8>,
    /// Every interface of the association holding the control interface
    /// (MIDI and unused streams included): snd-usb-audio binds them all.
    pub interfaces: Vec<u8>,
    clocks: Vec<(u8, ClockEntity, Vec<u8>)>,
    /// (terminal id, clock entity id)
    terminals: Vec<(u8, u8)>,
    pub streams: Vec<StreamFormat>,
}

impl AudioFunction {
    /// Parses one raw configuration descriptor (starting at its config header).
    pub(crate) fn parse(config: &[u8]) -> Result<Self, String> {
        let mut function = Self::default();
        let mut current: Option<(u8, u8, u8, u8, u8)> = None;
        let mut pending: Option<StreamFormat> = None;
        let mut associations = Vec::new();
        let mut offset = 0;
        while offset + 2 <= config.len() {
            let length = usize::from(config[offset]);
            if length < 2 || offset + length > config.len() {
                return Err(format!("malformed descriptor at byte {offset}"));
            }
            let descriptor = &config[offset..offset + length];
            offset += length;
            match descriptor[1] {
                DT_INTERFACE if length >= 9 => {
                    if let Some(stream) = pending.take() {
                        function.push_stream(stream);
                    }
                    current = Some((
                        descriptor[2],
                        descriptor[3],
                        descriptor[5],
                        descriptor[6],
                        descriptor[7],
                    ));
                    if let Some((number, 0, CLASS_AUDIO, SUBCLASS_CONTROL, PROTOCOL_UAC2)) = current
                    {
                        function.control_interface.get_or_insert(number);
                    }
                    if let Some((number, alt, CLASS_AUDIO, SUBCLASS_STREAMING, PROTOCOL_UAC2)) =
                        current
                        && alt != 0
                    {
                        pending = Some(StreamFormat {
                            interface: number,
                            altsetting: alt,
                            terminal_link: 0,
                            channels: 0,
                            subslot_bytes: 0,
                            bit_resolution: 0,
                            data: Endpoint {
                                address: 0,
                                attributes: 0,
                                max_packet_bytes: 0,
                                interval_log2: 0,
                            },
                            feedback: None,
                        });
                    }
                }
                DT_CS_INTERFACE if length >= 3 => match current {
                    Some((number, 0, CLASS_AUDIO, SUBCLASS_CONTROL, PROTOCOL_UAC2))
                        if Some(number) == function.control_interface =>
                    {
                        function.parse_control_entity(descriptor);
                    }
                    Some((_, _, CLASS_AUDIO, SUBCLASS_STREAMING, PROTOCOL_UAC2)) => {
                        if let Some(stream) = pending.as_mut() {
                            parse_streaming_entity(stream, descriptor);
                        }
                    }
                    _ => {}
                },
                DT_ENDPOINT if length >= 7 => {
                    if let Some(stream) = pending.as_mut() {
                        let raw_packet = u16::from_le_bytes([descriptor[4], descriptor[5]]);
                        let endpoint = Endpoint {
                            address: descriptor[2],
                            attributes: descriptor[3],
                            max_packet_bytes: u32::from(raw_packet & 0x07ff)
                                * (1 + u32::from((raw_packet >> 11) & 0x03)),
                            interval_log2: descriptor[6].clamp(1, 16) - 1,
                        };
                        if endpoint.is_iso() && endpoint.usage() == EP_USAGE_FEEDBACK {
                            stream.feedback = Some(endpoint);
                        } else if endpoint.is_iso()
                            && endpoint.is_data()
                            && stream.data.address == 0
                        {
                            stream.data = endpoint;
                        }
                    }
                }
                DT_INTERFACE_ASSOCIATION if length >= 8 => {
                    let first = u16::from(descriptor[2]);
                    associations.push(first..first + u16::from(descriptor[3]));
                }
                DT_CONFIG if offset > length => break,
                _ => {}
            }
        }
        if let Some(stream) = pending.take() {
            function.push_stream(stream);
        }
        let Some(control) = function.control_interface else {
            return Err("no USB Audio Class 2 control interface".into());
        };
        if let Some(range) = associations
            .into_iter()
            .find(|range| range.contains(&u16::from(control)))
        {
            function.interfaces = range
                .filter_map(|number| u8::try_from(number).ok())
                .collect();
            function
                .streams
                .retain(|stream| function.interfaces.contains(&stream.interface));
        } else {
            function.interfaces = vec![control];
        }
        Ok(function)
    }

    fn push_stream(&mut self, stream: StreamFormat) {
        if stream.data.address != 0 && stream.channels != 0 {
            self.streams.push(stream);
        }
    }

    fn parse_control_entity(&mut self, descriptor: &[u8]) {
        let length = descriptor.len();
        match descriptor[2] {
            AC_CLOCK_SOURCE if length >= 8 => self.clocks.push((
                descriptor[3],
                ClockEntity::Source {
                    controls: descriptor[5],
                },
                Vec::new(),
            )),
            AC_CLOCK_SELECTOR if length >= 5 => {
                let pins = usize::from(descriptor[4]);
                let sources = descriptor.get(5..5 + pins).unwrap_or_default().to_vec();
                self.clocks
                    .push((descriptor[3], ClockEntity::Selector, sources));
            }
            AC_CLOCK_MULTIPLIER if length >= 5 => self.clocks.push((
                descriptor[3],
                ClockEntity::Multiplier {
                    source: descriptor[4],
                },
                Vec::new(),
            )),
            AC_INPUT_TERMINAL if length >= 8 => self.terminals.push((descriptor[3], descriptor[7])),
            AC_OUTPUT_TERMINAL if length >= 9 => {
                self.terminals.push((descriptor[3], descriptor[8]));
            }
            _ => {}
        }
    }

    /// Picks the first altsetting carrying `channels` PCM channels of `subslot_bytes`.
    pub(crate) fn find_stream(
        &self,
        capture: bool,
        channels: u32,
        subslot_bytes: u8,
    ) -> Option<&StreamFormat> {
        self.streams.iter().find(|stream| {
            stream.data.is_in() == capture
                && stream.data.is_data()
                && u32::from(stream.channels) == channels
                && stream.subslot_bytes == subslot_bytes
        })
    }

    fn clock_entity(&self, id: u8) -> Option<&(u8, ClockEntity, Vec<u8>)> {
        self.clocks.iter().find(|(entity, _, _)| *entity == id)
    }

    fn terminal_clock(&self, terminal: u8) -> Option<u8> {
        self.terminals
            .iter()
            .find(|(id, _)| *id == terminal)
            .map(|(_, clock)| *clock)
    }
}

fn parse_streaming_entity(stream: &mut StreamFormat, descriptor: &[u8]) {
    match descriptor[2] {
        AS_GENERAL if descriptor.len() >= 11 => {
            let formats =
                u32::from_le_bytes([descriptor[6], descriptor[7], descriptor[8], descriptor[9]]);
            if descriptor[5] == FORMAT_TYPE_I && formats & FORMAT_PCM != 0 {
                stream.terminal_link = descriptor[3];
                stream.channels = descriptor[10];
            }
        }
        AS_FORMAT_TYPE if descriptor.len() >= 6 && descriptor[3] == FORMAT_TYPE_I => {
            stream.subslot_bytes = descriptor[4];
            stream.bit_resolution = descriptor[5];
        }
        _ => {}
    }
}

/// Splits a usbfs/sysfs descriptor dump into its configuration descriptors and
/// returns the one whose bConfigurationValue matches `active`.
pub(crate) fn active_configuration(dump: &[u8], active: u8) -> Result<&[u8], String> {
    if dump.len() < 18 || dump[1] != 0x01 {
        return Err("descriptor dump does not start with a device descriptor".into());
    }
    let mut offset = usize::from(dump[0]);
    while offset + 9 <= dump.len() {
        if dump[offset + 1] != DT_CONFIG {
            return Err(format!(
                "expected configuration descriptor at byte {offset}"
            ));
        }
        let total = usize::from(u16::from_le_bytes([dump[offset + 2], dump[offset + 3]]));
        if total < 9 || total > dump.len() - offset {
            return Err("configuration descriptor is truncated".into());
        }
        let end = offset + total;
        if dump[offset + 5] == active {
            return Ok(&dump[offset..end]);
        }
        offset = end;
    }
    Err(format!("active configuration {active} not found"))
}

/// A USB device located through sysfs.
#[derive(Debug)]
pub(crate) struct LocatedDevice {
    pub node: PathBuf,
    pub port: String,
    pub active_configuration: u8,
    pub high_speed: bool,
}

pub(crate) fn locate_device(
    vendor_id: u16,
    product_id: u16,
    port: Option<&str>,
) -> io::Result<LocatedDevice> {
    let mut matches = Vec::new();
    for entry in fs::read_dir("/sys/bus/usb/devices")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // Interfaces ("1-11:1.0") and root hubs ("usb1") are not devices we stream from.
        if name.contains(':') || name.starts_with("usb") {
            continue;
        }
        let path = entry.path();
        let read_hex = |file: &str| {
            fs::read_to_string(path.join(file))
                .ok()
                .and_then(|value| u16::from_str_radix(value.trim(), 16).ok())
        };
        if read_hex("idVendor") != Some(vendor_id) || read_hex("idProduct") != Some(product_id) {
            continue;
        }
        if port.is_some_and(|port| port != name) {
            continue;
        }
        let read_decimal = |file: &str| {
            fs::read_to_string(path.join(file))
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
        };
        let (Some(bus), Some(device)) = (read_decimal("busnum"), read_decimal("devnum")) else {
            continue;
        };
        let active_configuration = read_decimal("bConfigurationValue")
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(0);
        let high_speed = read_decimal("speed").is_some_and(|speed| speed >= 480);
        matches.push(LocatedDevice {
            node: PathBuf::from(format!("/dev/bus/usb/{bus:03}/{device:03}")),
            port: name,
            active_configuration,
            high_speed,
        });
    }
    match matches.len() {
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("USB device {vendor_id:04x}:{product_id:04x} not found"),
        )),
        1 => Ok(matches.remove(0)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "several {vendor_id:04x}:{product_id:04x} devices found ({}); set sycon.port",
                matches
                    .iter()
                    .map(|device| device.port.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

/// Resolves the clock source behind a streaming terminal, following selectors
/// and multipliers.
pub(crate) fn resolve_clock_source(
    device: &UsbDevice,
    function: &AudioFunction,
    terminal: u8,
) -> Result<(u8, u8), String> {
    let interface = function
        .control_interface
        .ok_or("no audio control interface")?;
    let id = function
        .terminal_clock(terminal)
        .ok_or_else(|| format!("terminal {terminal} has no clock"))?;
    find_clock_source(device, function, interface, id, 0)
}

/// Like snd-usb-audio's autoclock, a selector whose current input is invalid
/// (an unplugged external clock) is switched to the first valid input.
fn find_clock_source(
    device: &UsbDevice,
    function: &AudioFunction,
    interface: u8,
    id: u8,
    depth: u8,
) -> Result<(u8, u8), String> {
    if depth >= 8 {
        return Err("clock entity chain is too deep".into());
    }
    let (_, entity, sources) = function
        .clock_entity(id)
        .ok_or_else(|| format!("clock entity {id} is not described"))?;
    match *entity {
        ClockEntity::Source { controls } => Ok((id, controls)),
        ClockEntity::Multiplier { source } => {
            find_clock_source(device, function, interface, source, depth + 1)
        }
        ClockEntity::Selector => {
            if sources.is_empty() {
                return Err(format!("clock selector {id} has no inputs"));
            }
            let index = entity_index(id, interface);
            let mut pin = [0_u8; 1];
            let reported = device
                .control(
                    REQUEST_GET,
                    CUR,
                    CX_CLOCK_SELECTOR_CONTROL << 8,
                    index,
                    &mut pin,
                )
                .ok()
                .filter(|&read| read == 1)
                .map(|_| usize::from(pin[0]))
                .filter(|&value| (1..=sources.len()).contains(&value));
            let selected = reported.unwrap_or(1);
            let current = find_clock_source(
                device,
                function,
                interface,
                sources[selected - 1],
                depth + 1,
            )?;
            if sources.len() == 1 || clock_valid(device, interface, current) != Some(false) {
                if reported.is_none() && sources.len() > 1 {
                    let _ = device.control(
                        REQUEST_SET,
                        CUR,
                        CX_CLOCK_SELECTOR_CONTROL << 8,
                        index,
                        &mut [selected as u8],
                    );
                }
                return Ok(current);
            }
            for (pin, &source) in (1..).zip(sources) {
                if usize::from(pin) == selected {
                    continue;
                }
                let Ok(candidate) =
                    find_clock_source(device, function, interface, source, depth + 1)
                else {
                    continue;
                };
                if clock_valid(device, interface, candidate) == Some(true)
                    && device
                        .control(
                            REQUEST_SET,
                            CUR,
                            CX_CLOCK_SELECTOR_CONTROL << 8,
                            index,
                            &mut [pin],
                        )
                        .is_ok()
                {
                    return Ok(candidate);
                }
            }
            // No valid alternative: set_sample_rate reports the invalid clock.
            Ok(current)
        }
    }
}

/// None when the validity read fails. Like snd-usb-audio, a source without a
/// readable validity control counts as valid.
fn clock_valid(device: &UsbDevice, interface: u8, (clock, controls): (u8, u8)) -> Option<bool> {
    if (controls >> 2) & 0x03 == 0 {
        return Some(true);
    }
    let mut valid = [0_u8; 1];
    match device.control(
        REQUEST_GET,
        CUR,
        CS_CLOCK_VALID_CONTROL << 8,
        entity_index(clock, interface),
        &mut valid,
    ) {
        Ok(1) => Some(valid[0] != 0),
        _ => None,
    }
}

/// Fails when the clock's GET_RANGE excludes `rate`; an unreadable range passes.
pub(crate) fn check_rate_supported(
    device: &UsbDevice,
    interface: u8,
    clock: u8,
    rate: u32,
) -> Result<(), String> {
    if let Some(ranges) = get_rate_ranges(device, entity_index(clock, interface))
        && !ranges.is_empty()
        && !ranges.iter().any(|&(min, max, resolution)| {
            rate >= min
                && rate <= max
                && (resolution == 0 || min == max || (rate - min).is_multiple_of(resolution))
        })
    {
        return Err(format!(
            "clock {clock} does not support {rate} Hz (ranges {ranges:?})"
        ));
    }
    Ok(())
}

/// Programs `rate` on a clock source and waits until the device reports it valid.
pub(crate) fn set_sample_rate(
    device: &UsbDevice,
    interface: u8,
    clock: u8,
    controls: u8,
    rate: u32,
) -> Result<(), String> {
    let index = entity_index(clock, interface);
    let current = get_rate(device, index).ok();
    if current != Some(rate) {
        let mut payload = rate.to_le_bytes();
        device
            .control(
                REQUEST_SET,
                CUR,
                CS_SAM_FREQ_CONTROL << 8,
                index,
                &mut payload,
            )
            .map_err(|error| format!("set clock {clock} to {rate} Hz: {error}"))?;
    }
    // Validity control readable: bits 2..3 of bmControls.
    if (controls >> 2) & 0x03 != 0 {
        let mut locked = false;
        for _ in 0..50 {
            let mut valid = [0_u8; 1];
            match device.control(
                REQUEST_GET,
                CUR,
                CS_CLOCK_VALID_CONTROL << 8,
                index,
                &mut valid,
            ) {
                Ok(1) if valid[0] != 0 => {
                    locked = true;
                    break;
                }
                Ok(_) => thread::sleep(Duration::from_millis(10)),
                // Some firmware stalls the validity request; fall through to readback.
                Err(_) => {
                    locked = true;
                    break;
                }
            }
        }
        if !locked {
            return Err(format!("clock {clock} did not become valid at {rate} Hz"));
        }
    }
    if let Ok(actual) = get_rate(device, index)
        && actual != rate
    {
        return Err(format!(
            "clock {clock} reports {actual} Hz after setting {rate} Hz"
        ));
    }
    Ok(())
}

fn get_rate(device: &UsbDevice, index: u16) -> io::Result<u32> {
    let mut payload = [0_u8; 4];
    let read = device.control(
        REQUEST_GET,
        CUR,
        CS_SAM_FREQ_CONTROL << 8,
        index,
        &mut payload,
    )?;
    if read != 4 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short sample rate read",
        ));
    }
    Ok(u32::from_le_bytes(payload))
}

fn get_rate_ranges(device: &UsbDevice, index: u16) -> Option<Vec<(u32, u32, u32)>> {
    let mut count = [0_u8; 2];
    device
        .control(
            REQUEST_GET,
            RANGE,
            CS_SAM_FREQ_CONTROL << 8,
            index,
            &mut count,
        )
        .ok()
        .filter(|&read| read == 2)?;
    let ranges = usize::from(u16::from_le_bytes(count));
    let mut payload = vec![0_u8; 2 + ranges * 12];
    let read = device
        .control(
            REQUEST_GET,
            RANGE,
            CS_SAM_FREQ_CONTROL << 8,
            index,
            &mut payload,
        )
        .ok()?;
    Some(parse_rate_ranges(&payload[..read]))
}

fn parse_rate_ranges(payload: &[u8]) -> Vec<(u32, u32, u32)> {
    let word = |bytes: &[u8]| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    payload
        .get(2..)
        .unwrap_or_default()
        .as_chunks::<12>()
        .0
        .iter()
        .map(|range| (word(&range[0..]), word(&range[4..]), word(&range[8..])))
        .collect()
}

fn entity_index(entity: u8, interface: u8) -> u16 {
    (u16::from(entity) << 8) | u16::from(interface)
}

#[cfg(test)]
pub(crate) fn e1x2_descriptor_dump() -> Vec<u8> {
    let hex: String = include_str!("testdata/topping-e1x2-descriptors.hex")
        .split_whitespace()
        .collect();
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_topping_e1x2_function() {
        let dump = e1x2_descriptor_dump();
        let config = active_configuration(&dump, 1).unwrap();
        assert_eq!(config.len(), 0x1a5);
        let function = AudioFunction::parse(config).unwrap();
        assert_eq!(function.control_interface, Some(0));
        // DFU (3) and HID (4) sit outside the audio association.
        assert_eq!(function.interfaces, [0, 1, 2]);

        let playback = function.find_stream(false, 8, 4).unwrap();
        assert_eq!((playback.interface, playback.altsetting), (1, 1));
        assert_eq!(playback.terminal_link, 2);
        assert_eq!(playback.bit_resolution, 24);
        assert_eq!(playback.data.address, 0x01);
        assert_eq!(playback.data.max_packet_bytes, 800);
        assert_eq!(playback.data.interval_log2, 0);
        assert!(playback.data.is_async());
        let feedback = playback.feedback.unwrap();
        assert_eq!(feedback.address, 0x81);
        assert_eq!(feedback.max_packet_bytes, 4);
        assert_eq!(feedback.interval_log2, 3);

        let capture = function.find_stream(true, 10, 4).unwrap();
        assert_eq!((capture.interface, capture.altsetting), (2, 1));
        assert_eq!(capture.terminal_link, 22);
        assert_eq!(capture.data.address, 0x82);
        assert_eq!(capture.data.max_packet_bytes, 1000);
        assert!(capture.feedback.is_none());

        assert!(function.find_stream(false, 2, 4).is_none());
        assert_eq!(function.terminal_clock(2), Some(40));
        assert_eq!(function.terminal_clock(22), Some(40));
        assert_eq!(
            function
                .clock_entity(40)
                .map(|(_, entity, sources)| (*entity, sources.clone())),
            Some((ClockEntity::Selector, vec![41]))
        );
        assert_eq!(
            function.clock_entity(41).map(|(_, entity, _)| *entity),
            Some(ClockEntity::Source { controls: 0x07 })
        );
    }

    #[test]
    fn rejects_truncated_descriptors() {
        let dump = e1x2_descriptor_dump();
        let config = active_configuration(&dump, 1).unwrap();
        let mut broken = config[..40].to_vec();
        broken.push(0x30);
        broken.push(0x24);
        assert!(AudioFunction::parse(&broken).is_err());
        assert!(active_configuration(&dump, 9).is_err());
        // Truncated cleanly on a descriptor boundary inside the active config.
        assert!(active_configuration(&dump[..18 + 9], 1).is_err());
    }

    #[test]
    fn accepts_implicit_feedback_capture_and_ignores_reserved_usage() {
        let mut config = vec![
            // Config header (length filled below)
            9,
            DT_CONFIG,
            0,
            0,
            2,
            1,
            0,
            0x80,
            50,
            // AudioControl if0
            9,
            DT_INTERFACE,
            0,
            0,
            0,
            CLASS_AUDIO,
            SUBCLASS_CONTROL,
            PROTOCOL_UAC2,
            0,
            // AudioStreaming if1 alt1 (capture, 2ch S32)
            9,
            DT_INTERFACE,
            1,
            1,
            2,
            CLASS_AUDIO,
            SUBCLASS_STREAMING,
            PROTOCOL_UAC2,
            0,
            16,
            DT_CS_INTERFACE,
            AS_GENERAL,
            10,
            0,
            FORMAT_TYPE_I,
            1,
            0,
            0,
            0,
            2,
            0,
            0,
            0,
            0,
            0,
            6,
            DT_CS_INTERFACE,
            AS_FORMAT_TYPE,
            FORMAT_TYPE_I,
            4,
            24,
            // Reserved usage 0x30 ISO endpoint must not shadow the real data endpoint.
            7,
            DT_ENDPOINT,
            0x89,
            0x35,
            0x00,
            0x01,
            1,
            // Implicit feedback data IN endpoint (usage = 0x20, bmAttributes = 0x25).
            7,
            DT_ENDPOINT,
            0x82,
            0x25,
            0x00,
            0x01,
            1,
        ];
        let total = (config.len() as u16).to_le_bytes();
        config[2] = total[0];
        config[3] = total[1];

        let function = AudioFunction::parse(&config).unwrap();
        let capture = function.find_stream(true, 2, 4).unwrap();
        assert_eq!(capture.data.address, 0x82);
        assert_eq!(capture.data.usage(), EP_USAGE_IMPLICIT_FB);
    }

    #[test]
    fn isolates_primary_audio_function_in_multi_iad_config() {
        let mut config = vec![
            9,
            DT_CONFIG,
            0,
            0,
            4,
            1,
            0,
            0x80,
            50,
            // IAD 0: if0 (AC) + if1 (AS playback 2ch)
            8,
            DT_INTERFACE_ASSOCIATION,
            0,
            2,
            CLASS_AUDIO,
            0,
            PROTOCOL_UAC2,
            0,
            9,
            DT_INTERFACE,
            0,
            0,
            0,
            CLASS_AUDIO,
            SUBCLASS_CONTROL,
            PROTOCOL_UAC2,
            0,
            8,
            DT_CS_INTERFACE,
            AC_CLOCK_SOURCE,
            10,
            1,
            0x03,
            0,
            0,
            17,
            DT_CS_INTERFACE,
            AC_INPUT_TERMINAL,
            2,
            1,
            1,
            0,
            10,
            2,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            9,
            DT_INTERFACE,
            1,
            1,
            1,
            CLASS_AUDIO,
            SUBCLASS_STREAMING,
            PROTOCOL_UAC2,
            0,
            16,
            DT_CS_INTERFACE,
            AS_GENERAL,
            2,
            0,
            FORMAT_TYPE_I,
            1,
            0,
            0,
            0,
            2,
            0,
            0,
            0,
            0,
            0,
            6,
            DT_CS_INTERFACE,
            AS_FORMAT_TYPE,
            FORMAT_TYPE_I,
            4,
            24,
            7,
            DT_ENDPOINT,
            0x01,
            0x05,
            0x00,
            0x01,
            1,
            // IAD 1: if2 (second AC with colliding terminal 2 -> clock 99) + if3 (AS capture 4ch)
            8,
            DT_INTERFACE_ASSOCIATION,
            2,
            2,
            CLASS_AUDIO,
            0,
            PROTOCOL_UAC2,
            0,
            9,
            DT_INTERFACE,
            2,
            0,
            0,
            CLASS_AUDIO,
            SUBCLASS_CONTROL,
            PROTOCOL_UAC2,
            0,
            8,
            DT_CS_INTERFACE,
            AC_CLOCK_SOURCE,
            99,
            1,
            0x03,
            0,
            0,
            17,
            DT_CS_INTERFACE,
            AC_INPUT_TERMINAL,
            2,
            1,
            1,
            0,
            99,
            4,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            9,
            DT_INTERFACE,
            3,
            1,
            1,
            CLASS_AUDIO,
            SUBCLASS_STREAMING,
            PROTOCOL_UAC2,
            0,
            16,
            DT_CS_INTERFACE,
            AS_GENERAL,
            2,
            0,
            FORMAT_TYPE_I,
            1,
            0,
            0,
            0,
            4,
            0,
            0,
            0,
            0,
            0,
            6,
            DT_CS_INTERFACE,
            AS_FORMAT_TYPE,
            FORMAT_TYPE_I,
            4,
            24,
            7,
            DT_ENDPOINT,
            0x83,
            0x05,
            0x00,
            0x01,
            1,
        ];
        let total = (config.len() as u16).to_le_bytes();
        config[2] = total[0];
        config[3] = total[1];

        let function = AudioFunction::parse(&config).unwrap();
        assert_eq!(function.control_interface, Some(0));
        assert_eq!(function.interfaces, [0, 1]);
        assert!(function.find_stream(false, 2, 4).is_some());
        assert!(function.find_stream(true, 4, 4).is_none());
        assert_eq!(function.terminal_clock(2), Some(10));
        assert!(function.clock_entity(99).is_none());
    }

    #[test]
    fn parses_rate_ranges() {
        let mut payload = vec![2, 0];
        for (min, max, res) in [(44_100_u32, 44_100_u32, 0_u32), (48_000, 192_000, 48_000)] {
            payload.extend_from_slice(&min.to_le_bytes());
            payload.extend_from_slice(&max.to_le_bytes());
            payload.extend_from_slice(&res.to_le_bytes());
        }
        assert_eq!(
            parse_rate_ranges(&payload),
            vec![(44_100, 44_100, 0), (48_000, 192_000, 48_000)]
        );
    }
}
