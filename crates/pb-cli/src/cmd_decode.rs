//! `protobench decode`.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, Result};

use pb_core::recorder::{export_csv, CsvMode, OfflineSession};

/// Arguments of `decode`.
pub struct DecodeArgs {
    pub file: PathBuf,
    pub json: bool,
    pub csv: Option<PathBuf>,
    pub mcap: Option<PathBuf>,
    pub channels: Vec<String>,
    pub range: Option<String>,
    pub per_channel: bool,
}

/// Parses `t0:t1`; either side may be empty.
pub fn parse_range(s: &str) -> Result<(f64, f64)> {
    let (a, b) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("--range must look like T0:T1, e.g. 1.5:20"))?;
    let num = |x: &str, open: f64| -> Result<f64> {
        let x = x.trim();
        if x.is_empty() {
            Ok(open)
        } else {
            x.parse()
                .map_err(|_| anyhow!("--range: `{x}` is not a number of seconds"))
        }
    };
    let (t0, t1) = (num(a, f64::NEG_INFINITY)?, num(b, f64::INFINITY)?);
    if t1 < t0 {
        return Err(anyhow!("--range: end {t1} is before start {t0}"));
    }
    Ok((t0, t1))
}

pub fn decode(a: DecodeArgs) -> Result<ExitCode> {
    let range = a.range.as_deref().map(parse_range).transpose()?;
    let off = OfflineSession::open(&a.file)?;
    if off.truncated {
        eprintln!(
            "warning: {} ends in the middle of a record",
            a.file.display()
        );
    }
    let wants_dump = a.csv.is_none() && a.mcap.is_none() || a.json;
    if wants_dump {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        if !a.json {
            writeln!(
                out,
                "# {} — recorded with ProtoBench {} — {} records",
                off.header.project_name,
                off.header.tool_version,
                off.records.len()
            )?;
        }
        for r in off.decoded() {
            if a.json {
                writeln!(out, "{}", r.to_json())?;
            } else {
                writeln!(out, "{}", r.to_line())?;
            }
        }
    }
    if let Some(path) = &a.csv {
        let store = off.telemetry(usize::MAX / 4)?;
        for c in &a.channels {
            if store.get(c).is_none() {
                return Err(anyhow!("no channel `{c}` in this recording"));
            }
        }
        let mode = if a.per_channel {
            CsvMode::PerChannel
        } else {
            CsvMode::Joined
        };
        let files = export_csv(&store, &a.channels, range, mode, path)?;
        for f in files {
            eprintln!("wrote {}", f.display());
        }
    }
    if let Some(path) = &a.mcap {
        let n = pb_core::recorder::mcap_export::export_mcap(&off, path)?;
        eprintln!("wrote {} ({n} messages)", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("1.5:20").unwrap(), (1.5, 20.0));
        assert_eq!(parse_range(":2").unwrap(), (f64::NEG_INFINITY, 2.0));
        assert_eq!(parse_range("3:").unwrap(), (3.0, f64::INFINITY));
        assert!(parse_range("5:1").is_err());
        assert!(parse_range("5").is_err());
        assert!(parse_range("a:b").is_err());
    }
}
