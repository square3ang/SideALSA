use std::time::Duration;

pub trait ProCaptureSink: Send {
    /// Publishes one capture block without allocation, locking, or waiting for client work.
    fn process_capture(&mut self, sequence: u64, capture: &[i32]);
}

pub trait ProPlaybackSource: Send {
    /// Tries to obtain one playback block without allocation or locking and returns within `wait_budget`.
    fn process_playback(&mut self, sequence: u64, playback: &mut [i32], wait_budget: Duration);
}
