//! MCAP export (filled in with the V1 features).

use std::path::Path;

use super::OfflineSession;
use crate::error::CoreError;

/// Exports a recording to MCAP.
pub fn export_mcap(_session: &OfflineSession, path: &Path) -> Result<usize, CoreError> {
    Err(CoreError::Recorder(format!(
        "MCAP export to {} not available yet",
        path.display()
    )))
}
