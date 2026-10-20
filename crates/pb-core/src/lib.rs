//! ProtoBench core library.
//!
//! Everything that is not user interface lives here: the project model, schema
//! loading, framing, codecs, transports, the session runtime, telemetry,
//! commands, recording and test sequences. The GUI and CLI are thin layers on
//! top of this crate, and nothing in here depends on a UI toolkit.

pub mod check;
pub mod clock;
pub mod codec;
pub mod commands;
pub mod config;
pub mod diag;
pub mod error;
pub mod framing;
pub mod project;
pub mod recorder;
pub mod schema;
pub mod sequence;
pub mod session;
pub mod telemetry;
pub mod transport;
pub mod util;

pub use error::{CoreError, Result};

/// Tool version, written into session files and reports.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
