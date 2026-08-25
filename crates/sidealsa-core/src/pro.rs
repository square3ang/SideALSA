use std::time::Instant;

pub trait ProCaptureSink: Send {
    /// Publishes one capture block without allocation, locking, or waiting for client work.
    fn process_capture(&mut self, sequence: u64, capture: &[i32]);
}

pub trait ProPlaybackSource: Send {
    /// Tries an already-ready block, otherwise waits without allocation or locking until `deadline`.
    fn process_playback(&mut self, sequence: u64, playback: &mut [i32], deadline: Instant);
}
