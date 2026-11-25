mod backend;
mod engine;
mod latency;
mod pro;
mod routing;
mod sycon;
mod timeline;

pub use backend::HardwareEngine;
pub use engine::{DuplexEngine, EngineError, StreamDirection};
pub use pro::{ProCaptureSink, ProPlaybackSource};
pub use routing::{CompiledPort, RoutingError, RoutingTable};
pub use sidealsa_config::{
    DEFAULT_PRO_HANDOFF_US, DIRECT_MIN_PLAYBACK_QUEUE_PERIODS, DIRECT_WRITE_RESERVE_DIVISOR,
    HardwareBackend, HardwareConfig, LINKED_PHASE_OVERHEAD_DIVISOR, MAX_PRO_LATENCY_PERIODS,
    MAX_REALTIME_PRIORITY, PcmConfig, PortConfig, PortsConfig, Profile, ProfileError, SampleFormat,
    SyconConfig,
};
pub use sycon::{SyconEngine, release_sycon_device};
pub use timeline::{HardwareStats, HardwareTimeline};
