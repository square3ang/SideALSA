use std::{
    io,
    mem::size_of,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    ptr,
};

pub use sidealsa_client::{SharedError, SharedRegion};

const EVENTFD_IO_ATTEMPTS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackReadyWait {
    Ready,
    Timeout,
    Interrupted,
    Failed,
}

pub struct SharedEvents {
    capture: RawFd,
    playback: RawFd,
    playback_ready: RawFd,
    playback_deadline: OwnedFd,
}

impl SharedEvents {
    pub fn new() -> Result<Self, SharedError> {
        let timer = unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
            )
        };
        if timer < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let playback_deadline = unsafe { OwnedFd::from_raw_fd(timer) };
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
            playback_deadline,
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

    pub fn drain_playback_ready(&self) {
        drain(self.playback_ready);
    }

    pub fn notify_playback_ready(&self) {
        notify(self.playback_ready);
    }

    // Only the hardware playback thread waits on this endpoint's private timer.
    pub fn wait_playback_ready_before(&self, cutoff_nanos: u64) -> PlaybackReadyWait {
        if self.playback_ready < 0 {
            return PlaybackReadyWait::Failed;
        }
        let mut descriptors = [
            libc::pollfd {
                fd: self.playback_ready,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.playback_deadline.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // A zero timer value disarms it; use an expired absolute deadline instead.
        let cutoff_nanos = cutoff_nanos.max(1);
        let timer = libc::itimerspec {
            it_interval: zero,
            it_value: libc::timespec {
                tv_sec: (cutoff_nanos / 1_000_000_000)
                    .try_into()
                    .unwrap_or(libc::time_t::MAX),
                tv_nsec: (cutoff_nanos % 1_000_000_000) as _,
            },
        };
        // Keep the deadline absolute even if preempted before entering ppoll.
        if unsafe {
            libc::timerfd_settime(
                self.playback_deadline.as_raw_fd(),
                libc::TFD_TIMER_ABSTIME,
                &timer,
                ptr::null_mut(),
            )
        } < 0
        {
            return PlaybackReadyWait::Failed;
        }
        let result = unsafe {
            libc::ppoll(
                descriptors.as_mut_ptr(),
                descriptors.len() as _,
                ptr::null(),
                ptr::null(),
            )
        };
        let outcome = if result < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                PlaybackReadyWait::Interrupted
            } else {
                PlaybackReadyWait::Failed
            }
        } else if descriptors.iter().any(|descriptor| {
            descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
        }) {
            PlaybackReadyWait::Failed
        } else if descriptors[0].revents & libc::POLLIN != 0 {
            PlaybackReadyWait::Ready
        } else if descriptors[1].revents & libc::POLLIN != 0 {
            PlaybackReadyWait::Timeout
        } else {
            PlaybackReadyWait::Failed
        };
        // Avoid a needless timer interrupt after the client has already responded.
        let disarmed = libc::itimerspec {
            it_interval: zero,
            it_value: zero,
        };
        if unsafe {
            libc::timerfd_settime(
                self.playback_deadline.as_raw_fd(),
                0,
                &disarmed,
                ptr::null_mut(),
            )
        } < 0
        {
            return PlaybackReadyWait::Failed;
        }
        outcome
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
    for _ in 0..EVENTFD_IO_ATTEMPTS {
        let result = unsafe { libc::write(fd, (&value as *const u64).cast(), size_of::<u64>()) };
        if result >= 0 {
            return;
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) | None => return,
            Some(_) => return,
        }
    }
}

fn drain(fd: RawFd) {
    let mut value = 0_u64;
    for _ in 0..EVENTFD_IO_ATTEMPTS {
        let result = unsafe { libc::read(fd, (&mut value as *mut u64).cast(), size_of::<u64>()) };
        if result >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PlaybackReadyWait, SharedEvents};
    use std::time::Duration;

    fn monotonic_nanos() -> u64 {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) },
            0
        );
        time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
    }

    #[test]
    fn playback_ready_wait_distinguishes_timeout_and_notification() {
        let events = SharedEvents::new().expect("events should create");

        assert_eq!(
            events.wait_playback_ready_before(0),
            PlaybackReadyWait::Timeout
        );
        events.notify_playback_ready();
        assert_eq!(
            events.wait_playback_ready_before(monotonic_nanos() + 10_000_000),
            PlaybackReadyWait::Ready
        );
    }

    #[test]
    fn playback_ready_wait_reports_descriptor_failure() {
        let mut events = SharedEvents::new().expect("events should create");
        let fd = std::mem::replace(&mut events.playback_ready, -1);
        unsafe {
            libc::close(fd);
        }
        let result = events.wait_playback_ready_before(0);

        assert_eq!(result, PlaybackReadyWait::Failed);
    }

    #[test]
    fn elapsed_absolute_deadline_does_not_restart_the_wait() {
        let events = SharedEvents::new().expect("events should create");
        let cutoff = monotonic_nanos() + 1_000_000;
        std::thread::sleep(Duration::from_millis(2));
        assert!(monotonic_nanos() >= cutoff);
        assert_eq!(
            events.wait_playback_ready_before(cutoff),
            PlaybackReadyWait::Timeout
        );
    }

    #[test]
    fn playback_deadline_can_be_rearmed_after_timeout_and_ready() {
        let events = SharedEvents::new().expect("events should create");
        assert_eq!(
            events.wait_playback_ready_before(0),
            PlaybackReadyWait::Timeout
        );
        events.notify_playback_ready();
        assert_eq!(
            events.wait_playback_ready_before(monotonic_nanos() + 1_000_000_000),
            PlaybackReadyWait::Ready
        );
        let mut timer = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        };
        use std::os::fd::AsRawFd;
        assert_eq!(
            unsafe { libc::timerfd_gettime(events.playback_deadline.as_raw_fd(), &mut timer) },
            0
        );
        assert_eq!(timer.it_value.tv_sec, 0);
        assert_eq!(timer.it_value.tv_nsec, 0);
        events.drain_playback_ready();
        let cutoff = monotonic_nanos() + 2_000_000;
        assert_eq!(
            events.wait_playback_ready_before(cutoff),
            PlaybackReadyWait::Timeout
        );
        assert!(
            monotonic_nanos() >= cutoff,
            "stale expiration must not end a new wait"
        );
    }
}
