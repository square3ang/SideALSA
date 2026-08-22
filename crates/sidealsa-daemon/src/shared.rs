use std::{io, mem::size_of, os::fd::RawFd};

pub use sidealsa_client::{SharedError, SharedRegion};

pub struct SharedEvents {
    capture: RawFd,
    playback: RawFd,
    playback_ready: RawFd,
}

impl SharedEvents {
    pub fn new() -> Result<Self, SharedError> {
        let flags = libc::EFD_CLOEXEC | libc::EFD_NONBLOCK;
        let capture = unsafe { libc::eventfd(0, flags) };
        if capture < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let playback = unsafe { libc::eventfd(0, flags) };
        if playback < 0 {
            unsafe { libc::close(capture) };
            return Err(io::Error::last_os_error().into());
        }
        let playback_ready = unsafe { libc::eventfd(0, flags) };
        if playback_ready < 0 {
            unsafe {
                libc::close(capture);
                libc::close(playback);
            }
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self {
            capture,
            playback,
            playback_ready,
        })
    }

    pub fn capture_fd(&self) -> RawFd {
        self.capture
    }

    pub fn playback_fd(&self) -> RawFd {
        self.playback
    }

    pub fn playback_ready_fd(&self) -> RawFd {
        self.playback_ready
    }

    pub fn notify_capture(&self) {
        notify(self.capture);
    }

    pub fn notify_playback(&self) {
        notify(self.playback);
    }

    pub fn drain(&self) {
        drain(self.capture);
        drain(self.playback);
        drain(self.playback_ready);
    }
}

impl Drop for SharedEvents {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.capture);
            libc::close(self.playback);
            libc::close(self.playback_ready);
        }
    }
}

fn notify(fd: RawFd) {
    let value = 1_u64;
    let result = unsafe { libc::write(fd, (&value as *const u64).cast(), size_of::<u64>()) };
    if result < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN) {
        let _ = result;
    }
}

fn drain(fd: RawFd) {
    loop {
        let mut value = 0_u64;
        let result = unsafe { libc::read(fd, (&mut value as *mut u64).cast(), size_of::<u64>()) };
        if result == size_of::<u64>() as isize {
            continue;
        }
        if result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        break;
    }
}
