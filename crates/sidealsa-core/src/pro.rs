pub trait ProCaptureSink: Send {
    /// Publishes one capture block without allocation, locking, or waiting for client work.
    fn process_capture(&mut self, sequence: u64, capture: &[i32]);

    fn process_capture_for_playback(
        &mut self,
        _hardware_sequence: u64,
        playback_sequence: u64,
        capture: &[i32],
    ) {
        self.process_capture(playback_sequence, capture);
    }

    /// Publishes non-PRO capture after the exact PRO handoff has completed.
    fn process_deferred_capture(&mut self, _hardware_sequence: u64, _capture: &[i32]) {}
}

pub trait ProPlaybackSource: Send {
    /// Changes whenever previously rendered output is no longer valid for reuse.
    fn playback_epoch(&self) -> u64 {
        0
    }

    /// Publishes the next sequence watermark before the hardware wait.
    fn prepare_playback(&mut self, _sequence: u64) {}

    /// Prepares non-PRO output before the hardware wait.
    fn prepare_playback_mix(&mut self, _sequence: u64) {}

    /// Takes one already-ready exact-sequence block without waiting.
    fn process_playback(&mut self, sequence: u64, playback: &mut [i32]);

    /// Takes an exact-sequence block only when it was published by the cutoff.
    fn process_playback_before(&mut self, sequence: u64, _cutoff_nanos: u64, playback: &mut [i32]) {
        self.process_playback(sequence, playback);
    }

    /// Adds any output not prepared before the hardware wait.
    fn commit_playback(&mut self, _sequence: u64, _playback: &mut [i32]) {}
}
