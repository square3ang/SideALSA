use std::sync::{Arc, atomic::AtomicBool};

use crate::{
    DuplexEngine, EngineError, HardwareBackend, HardwareConfig, HardwareStats, HardwareTimeline,
    ProCaptureSink, ProPlaybackSource, Profile, RoutingTable, SyconEngine,
};

/// The hardware engine a profile selects with `device.backend`.
pub enum HardwareEngine {
    Alsa(Box<DuplexEngine>),
    Sycon(Box<SyconEngine>),
}

macro_rules! each {
    ($self:expr, $engine:ident => $body:expr) => {
        match $self {
            HardwareEngine::Alsa($engine) => $body,
            HardwareEngine::Sycon($engine) => $body,
        }
    };
}

impl HardwareEngine {
    pub fn open(profile: Profile) -> Result<Self, EngineError> {
        match profile.device.backend {
            HardwareBackend::Alsa => {
                DuplexEngine::open(profile).map(|engine| Self::Alsa(engine.into()))
            }
            HardwareBackend::Sycon => {
                SyconEngine::open(profile).map(|engine| Self::Sycon(engine.into()))
            }
        }
    }

    pub fn config(&self) -> &HardwareConfig {
        each!(self, engine => engine.config())
    }

    pub fn timeline(&self) -> &HardwareTimeline {
        each!(self, engine => engine.timeline())
    }

    pub fn timeline_handle(&self) -> Arc<HardwareTimeline> {
        each!(self, engine => engine.timeline_handle())
    }

    pub fn routing(&self) -> &RoutingTable {
        each!(self, engine => engine.routing())
    }

    pub fn stats(&self) -> HardwareStats {
        each!(self, engine => engine.stats())
    }

    pub fn start(&mut self) -> Result<(), EngineError> {
        each!(self, engine => engine.start())
    }

    pub fn run(&mut self, stop: &AtomicBool, max_periods: Option<u64>) -> Result<(), EngineError> {
        each!(self, engine => engine.run(stop, max_periods))
    }

    pub fn run_pro<C: ProCaptureSink, P: ProPlaybackSource>(
        &mut self,
        stop: &AtomicBool,
        max_periods: Option<u64>,
        capture_sink: C,
        playback_source: P,
    ) -> Result<(), EngineError> {
        each!(self, engine => engine.run_pro(stop, max_periods, capture_sink, playback_source))
    }

    pub fn run_pro_with_ready<C: ProCaptureSink, P: ProPlaybackSource>(
        &mut self,
        stop: &AtomicBool,
        max_periods: Option<u64>,
        capture_sink: C,
        playback_source: P,
        hardware_ready: &AtomicBool,
    ) -> Result<(), EngineError> {
        each!(self, engine => engine.run_pro_with_ready(
            stop,
            max_periods,
            capture_sink,
            playback_source,
            hardware_ready,
        ))
    }

    pub fn stop(&mut self) -> Result<(), EngineError> {
        each!(self, engine => engine.stop())
    }
}
