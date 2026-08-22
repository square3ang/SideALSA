pub trait ProCaptureSink: Send {
    fn process_capture(&mut self, sequence: u64, capture: &[i32]);
}

pub trait ProPlaybackSource: Send {
    fn process_playback(&mut self, sequence: u64, playback: &mut [i32]);
}
