//! `protobench run` (sequences).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Result};

use crate::NonInteractive;

pub fn run(
    _dir: &Path,
    _sequence: &str,
    _profile: Option<&str>,
    _port: Option<String>,
    _report_dir: Option<PathBuf>,
    _ni: Option<NonInteractive>,
) -> Result<ExitCode> {
    bail!("sequences are not implemented yet")
}
