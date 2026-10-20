//! `protobench decode`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;

use pb_core::recorder::{export_csv, CsvMode, OfflineSession};

pub fn decode(
    file: &Path,
    json: bool,
    csv: Option<PathBuf>,
    mcap: Option<PathBuf>,
) -> Result<ExitCode> {
    let off = OfflineSession::open(file)?;
    if off.truncated {
        eprintln!("warning: {} ends in the middle of a record", file.display());
    }
    let wants_dump = csv.is_none() && mcap.is_none() || json;
    if wants_dump {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        if !json {
            writeln!(
                out,
                "# {} — recorded with ProtoBench {} — {} records",
                off.header.project_name,
                off.header.tool_version,
                off.records.len()
            )?;
        }
        for r in off.decoded() {
            if json {
                writeln!(out, "{}", r.to_json())?;
            } else {
                writeln!(out, "{}", r.to_line())?;
            }
        }
    }
    if let Some(path) = csv {
        let store = off.telemetry(usize::MAX / 4)?;
        let files = export_csv(&store, &[], None, CsvMode::Joined, &path)?;
        for f in files {
            eprintln!("wrote {}", f.display());
        }
    }
    if let Some(path) = mcap {
        let n = pb_core::recorder::mcap_export::export_mcap(&off, &path)?;
        eprintln!("wrote {} ({n} messages)", path.display());
    }
    Ok(ExitCode::SUCCESS)
}
