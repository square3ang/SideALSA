//! Minimal Linux usbfs bindings for the Sycon UAC2 backend.
//!
//! Only the ioctls the backend needs are bound. Layouts and request numbers
//! match `linux/usbdevice_fs.h` on 64-bit Linux and are checked by tests.

use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
    ptr,
};

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
compile_error!("Sycon usbfs ioctl bindings require 64-bit Linux");

pub(crate) const URB_TYPE_ISO: u8 = 0;
pub(crate) const URB_ISO_ASAP: u32 = 0x02;
/// One URB never carries more packets than this; usbfs accepts up to 128.
pub(crate) const MAX_ISO_PACKETS: usize = 8;

const USBDEVFS_CONTROL: libc::Ioctl = 0xc018_5500_u32 as libc::Ioctl;
const USBDEVFS_SETINTERFACE: libc::Ioctl = 0x8008_5504_u32 as libc::Ioctl;
const USBDEVFS_SUBMITURB: libc::Ioctl = 0x8038_550a_u32 as libc::Ioctl;
const USBDEVFS_DISCARDURB: libc::Ioctl = 0x0000_550b_u32 as libc::Ioctl;
const USBDEVFS_REAPURBNDELAY: libc::Ioctl = 0x4008_550d_u32 as libc::Ioctl;
const USBDEVFS_RELEASEINTERFACE: libc::Ioctl = 0x8004_5510_u32 as libc::Ioctl;
const USBDEVFS_IOCTL: libc::Ioctl = 0xc010_5512_u32 as libc::Ioctl;
const USBDEVFS_DISCONNECT_CLAIM: libc::Ioctl = 0x8108_551b_u32 as libc::Ioctl;
const USBDEVFS_CONNECT: libc::c_int = 0x0000_5517;
const DISCONNECT_CLAIM_IF_DRIVER: u32 = 0x01;
const DISCONNECT_CLAIM_EXCEPT_DRIVER: u32 = 0x02;

const CONTROL_TIMEOUT_MS: u32 = 1000;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IsoPacketDesc {
    pub length: u32,
    pub actual_length: u32,
    /// Negative errno stored in an unsigned field by the kernel.
    pub status: u32,
}

#[repr(C)]
#[derive(Debug)]
pub(crate) struct UrbHeader {
    pub kind: u8,
    pub endpoint: u8,
    pub status: i32,
    pub flags: u32,
    pub buffer: *mut c_void,
    pub buffer_length: i32,
    pub actual_length: i32,
    pub start_frame: i32,
    pub number_of_packets: i32,
    pub error_count: i32,
    pub signr: u32,
    pub usercontext: *mut c_void,
}

const _: () = assert!(std::mem::size_of::<UrbHeader>() == 56);

/// An isochronous URB with its packet descriptors laid out where usbfs expects them.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct IsoUrb {
    pub header: UrbHeader,
    pub packets: [IsoPacketDesc; MAX_ISO_PACKETS],
}

impl IsoUrb {
    pub(crate) fn new(endpoint: u8, buffer: *mut u8, tag: usize) -> Self {
        Self {
            header: UrbHeader {
                kind: URB_TYPE_ISO,
                endpoint,
                status: 0,
                flags: URB_ISO_ASAP,
                buffer: buffer.cast(),
                buffer_length: 0,
                actual_length: 0,
                start_frame: 0,
                number_of_packets: 0,
                error_count: 0,
                signr: 0,
                usercontext: tag as *mut c_void,
            },
            packets: [IsoPacketDesc::default(); MAX_ISO_PACKETS],
        }
    }

    /// Resets completion fields before a resubmit; `lengths` sets the packet layout.
    pub(crate) fn prepare(&mut self, lengths: &[u32]) {
        assert!(lengths.len() <= MAX_ISO_PACKETS);
        let mut total = 0_u32;
        for (packet, &length) in self.packets.iter_mut().zip(lengths) {
            *packet = IsoPacketDesc {
                length,
                actual_length: 0,
                status: 0,
            };
            total += length;
        }
        self.header.status = 0;
        self.header.flags = URB_ISO_ASAP;
        self.header.buffer_length = total as i32;
        self.header.actual_length = 0;
        self.header.start_frame = 0;
        self.header.number_of_packets = lengths.len() as i32;
        self.header.error_count = 0;
    }

    pub(crate) fn tag(&self) -> usize {
        self.header.usercontext as usize
    }
}

#[repr(C)]
struct CtrlTransfer {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout: u32,
    data: *mut c_void,
}

#[repr(C)]
struct SetInterface {
    interface: u32,
    altsetting: u32,
}

#[repr(C)]
struct UsbIoctl {
    ifno: libc::c_int,
    ioctl_code: libc::c_int,
    data: *mut c_void,
}

#[repr(C)]
struct DisconnectClaim {
    interface: u32,
    flags: u32,
    driver: [u8; 256],
}

/// An open usbfs device node. Dropping it releases every claimed interface and
/// asks the kernel to bind its driver again, so snd-usb-audio comes back.
pub(crate) struct UsbDevice {
    file: File,
    claimed: Vec<u8>,
}

impl UsbDevice {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)?;
        Ok(Self {
            file,
            claimed: Vec::new(),
        })
    }

    pub(crate) fn raw_fd(&self) -> libc::c_int {
        self.file.as_raw_fd()
    }

    /// Device descriptor followed by every raw configuration descriptor.
    pub(crate) fn read_descriptors(&mut self) -> io::Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Detaches the kernel driver from `interface` and claims it for this
    /// process in one step. Fails with EBUSY when another usbfs user (a
    /// running sidealsad) owns it, instead of stealing it.
    pub(crate) fn detach_and_claim(&mut self, interface: u8) -> io::Result<()> {
        self.disconnect_claim(interface, DISCONNECT_CLAIM_EXCEPT_DRIVER, b"usbfs")
    }

    /// Claims `interface` only if it is unbound or bound to `driver`;
    /// any other holder keeps it (EBUSY).
    pub(crate) fn claim_from(&mut self, interface: u8, driver: &[u8]) -> io::Result<()> {
        self.disconnect_claim(interface, DISCONNECT_CLAIM_IF_DRIVER, driver)
    }

    fn disconnect_claim(&mut self, interface: u8, flags: u32, driver: &[u8]) -> io::Result<()> {
        let mut request = DisconnectClaim {
            interface: u32::from(interface),
            flags,
            driver: [0; 256],
        };
        request.driver[..driver.len()].copy_from_slice(driver);
        // SAFETY: `request` matches struct usbdevfs_disconnect_claim.
        cvt(unsafe { libc::ioctl(self.raw_fd(), USBDEVFS_DISCONNECT_CLAIM, &mut request) })?;
        self.claimed.push(interface);
        Ok(())
    }

    pub(crate) fn set_interface(&self, interface: u8, altsetting: u8) -> io::Result<()> {
        let mut request = SetInterface {
            interface: u32::from(interface),
            altsetting: u32::from(altsetting),
        };
        // SAFETY: `request` matches struct usbdevfs_setinterface.
        cvt(unsafe { libc::ioctl(self.raw_fd(), USBDEVFS_SETINTERFACE, &mut request) }).map(drop)
    }

    pub(crate) fn control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
    ) -> io::Result<usize> {
        let length = u16::try_from(data.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "control data too long"))?;
        let mut transfer = CtrlTransfer {
            request_type,
            request,
            value,
            index,
            length,
            timeout: CONTROL_TIMEOUT_MS,
            data: data.as_mut_ptr().cast(),
        };
        // SAFETY: `transfer` points at `data`, which is valid for `length` bytes.
        let transferred =
            cvt(unsafe { libc::ioctl(self.raw_fd(), USBDEVFS_CONTROL, &mut transfer) })?;
        Ok(transferred as usize)
    }

    /// # Safety
    /// `urb` and its buffer must stay valid and unmoved until the URB is reaped
    /// or this device is closed.
    pub(crate) unsafe fn submit(&self, urb: *mut IsoUrb) -> io::Result<()> {
        cvt(unsafe { libc::ioctl(self.raw_fd(), USBDEVFS_SUBMITURB, urb) }).map(drop)
    }

    /// Returns a completed URB without blocking, or `None` when none is ready.
    pub(crate) fn reap_nonblocking(&self) -> io::Result<Option<*mut IsoUrb>> {
        let mut urb: *mut c_void = ptr::null_mut();
        // SAFETY: the kernel writes one pointer into `urb`.
        match cvt(unsafe { libc::ioctl(self.raw_fd(), USBDEVFS_REAPURBNDELAY, &mut urb) }) {
            Ok(_) => Ok(Some(urb.cast())),
            Err(error) if error.raw_os_error() == Some(libc::EAGAIN) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Cancels a submitted URB; it still has to be reaped.
    pub(crate) fn discard(&self, urb: *mut IsoUrb) -> io::Result<()> {
        // SAFETY: usbfs only uses the pointer as a lookup key.
        cvt(unsafe { libc::ioctl(self.raw_fd(), USBDEVFS_DISCARDURB, urb) }).map(drop)
    }

    /// Waits until a completed URB can be reaped. Returns false on timeout.
    pub(crate) fn wait_reapable(&self, timeout_ms: i32) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        loop {
            // SAFETY: one valid pollfd.
            let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if descriptor.revents & libc::POLLOUT != 0 {
                return Ok(true);
            }
            if descriptor.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                return Err(io::Error::from_raw_os_error(libc::ENODEV));
            }
            return Ok(false);
        }
    }

    /// Asks the kernel to probe a driver for an unclaimed interface.
    /// EBUSY means a driver already took it (snd-usb-audio claims the whole
    /// function from its control interface).
    pub(crate) fn connect_driver(&self, interface: u8) -> io::Result<()> {
        let mut request = UsbIoctl {
            ifno: libc::c_int::from(interface),
            ioctl_code: USBDEVFS_CONNECT,
            data: ptr::null_mut(),
        };
        // SAFETY: `request` matches struct usbdevfs_ioctl.
        match cvt(unsafe { libc::ioctl(self.raw_fd(), USBDEVFS_IOCTL, &mut request) }) {
            Err(error) if error.raw_os_error() == Some(libc::EBUSY) => Ok(()),
            result => result.map(drop),
        }
    }

    fn release_all(&mut self) {
        let claimed = std::mem::take(&mut self.claimed);
        for &interface in &claimed {
            let mut number = u32::from(interface);
            // SAFETY: the kernel reads one unsigned int.
            unsafe {
                let _ = libc::ioctl(
                    self.file.as_raw_fd(),
                    USBDEVFS_RELEASEINTERFACE,
                    &mut number,
                );
            }
        }
        // Reconnect only after every release: probing the control interface
        // must find the streaming interfaces free.
        for &interface in &claimed {
            let _ = self.connect_driver(interface);
        }
    }
}

impl Drop for UsbDevice {
    fn drop(&mut self) {
        self.release_all();
    }
}

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::mem::{offset_of, size_of};

    use super::*;

    #[test]
    fn layouts_match_linux_headers() {
        assert_eq!(size_of::<UrbHeader>(), 56);
        assert_eq!(offset_of!(UrbHeader, status), 4);
        assert_eq!(offset_of!(UrbHeader, buffer), 16);
        assert_eq!(offset_of!(UrbHeader, start_frame), 32);
        assert_eq!(offset_of!(UrbHeader, number_of_packets), 36);
        assert_eq!(offset_of!(UrbHeader, usercontext), 48);
        assert_eq!(size_of::<IsoPacketDesc>(), 12);
        assert_eq!(offset_of!(IsoUrb, packets), 56);
        assert_eq!(size_of::<CtrlTransfer>(), 24);
        assert_eq!(offset_of!(CtrlTransfer, data), 16);
        assert_eq!(size_of::<SetInterface>(), 8);
        assert_eq!(size_of::<UsbIoctl>(), 16);
        assert_eq!(size_of::<DisconnectClaim>(), 264);
        assert_eq!(offset_of!(DisconnectClaim, driver), 8);
    }

    #[test]
    fn prepare_sums_packet_lengths() {
        let mut buffer = vec![0_u8; 64];
        let mut urb = IsoUrb::new(0x01, buffer.as_mut_ptr(), 7);
        urb.prepare(&[24, 32, 8]);
        assert_eq!(urb.header.buffer_length, 64);
        assert_eq!(urb.header.number_of_packets, 3);
        assert_eq!(urb.tag(), 7);
    }
}
