//! Telemetry: channels, ring buffers, extraction, derived expressions,
//! statistics and decimation.

mod decimate;
mod expr;
mod extract;
mod ring;
mod stats;
mod store;
mod timebase;

pub use decimate::decimate_minmax;
pub use expr::Expr;
pub use extract::{Extractor, TriggerState};
pub use ring::RingBuffer;
pub use stats::{stats_of, RunningStats, StatsSnapshot};
pub use store::{Channel, ChannelData, ChannelKind, TelemetryStore, DEFAULT_CAPACITY};
pub use timebase::DeviceTimeBase;
