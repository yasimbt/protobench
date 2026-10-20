//! Replay transport: plays back a `.pbsession` recording (filled in later).

use std::io;
use std::path::Path;

use super::Transport;
use crate::config::FramingConfig;
use crate::error::CoreError;

/// Plays back recorded rx frames.
pub struct ReplayTransport;

impl ReplayTransport {
    /// Opens a recording.
    pub fn open(path: &Path, _speed: f64, _framing: &FramingConfig) -> Result<Self, CoreError> {
        Err(CoreError::Transport(format!(
            "replay of {} not available yet",
            path.display()
        )))
    }
}

impl Transport for ReplayTransport {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
    fn write_all(&mut self, _data: &[u8]) -> io::Result<()> {
        Ok(())
    }
    fn describe(&self) -> String {
        "replay".into()
    }
}
