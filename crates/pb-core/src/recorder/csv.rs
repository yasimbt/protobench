//! CSV export of telemetry channels.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::CoreError;
use crate::telemetry::TelemetryStore;

/// Layout of the exported CSV.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsvMode {
    /// One file: `time_s` plus one column per channel, as-of joined on the
    /// union of timestamps (each cell holds the channel's latest value at or
    /// before that time; empty before its first sample).
    Joined,
    /// One file per channel: `time_s,value`.
    PerChannel,
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Exports `channels` (all when empty) within `range` (seconds, inclusive).
/// Returns the files written.
pub fn export_csv(
    store: &TelemetryStore,
    channels: &[String],
    range: Option<(f64, f64)>,
    mode: CsvMode,
    path: &Path,
) -> Result<Vec<PathBuf>, CoreError> {
    let (chans, samples) = super::join::select(store, channels, range)?;
    let io = |p: &Path, e: std::io::Error| CoreError::io(p, e);
    match mode {
        CsvMode::Joined => {
            let f = std::fs::File::create(path).map_err(|e| io(path, e))?;
            let mut w = std::io::BufWriter::new(f);
            let mut header = vec!["time_s".to_string()];
            header.extend(chans.iter().map(|c| csv_field(&c.name)));
            writeln!(w, "{}", header.join(",")).map_err(|e| io(path, e))?;
            let mut rows = super::join::AsOf::new(&samples);
            while let Some(t) = rows.next_row() {
                let mut line = format!("{t}");
                for v in rows.values() {
                    line.push(',');
                    if let Some(v) = v {
                        line.push_str(&format!("{v}"));
                    }
                }
                writeln!(w, "{line}").map_err(|e| io(path, e))?;
            }
            w.flush().map_err(|e| io(path, e))?;
            Ok(vec![path.to_path_buf()])
        }
        CsvMode::PerChannel => {
            let stem = path.with_extension("");
            let mut out = Vec::new();
            for (c, s) in chans.iter().zip(&samples) {
                let p = PathBuf::from(format!("{}_{}.csv", stem.display(), sanitize(&c.name)));
                let f = std::fs::File::create(&p).map_err(|e| io(&p, e))?;
                let mut w = std::io::BufWriter::new(f);
                writeln!(w, "time_s,{}", csv_field(&c.name)).map_err(|e| io(&p, e))?;
                for (t, v) in s {
                    writeln!(w, "{t},{v}").map_err(|e| io(&p, e))?;
                }
                w.flush().map_err(|e| io(&p, e))?;
                out.push(p);
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::FieldDisplay;
    use crate::telemetry::ChannelKind;

    fn store() -> TelemetryStore {
        let s = TelemetryStore::new(100);
        let a = s.get_or_create("a", || (FieldDisplay::plain("a"), ChannelKind::Number));
        let b = s.get_or_create("b.x[0]", || (FieldDisplay::plain("b"), ChannelKind::Number));
        a.push(1.0, 10.0);
        a.push(3.0, 30.0);
        b.push(2.0, 2.5);
        b.push(3.0, 3.5);
        s
    }

    #[test]
    fn joined_as_of() {
        let s = store();
        let p = std::env::temp_dir().join(format!("pb-csv-{}.csv", std::process::id()));
        export_csv(&s, &[], None, CsvMode::Joined, &p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text, "time_s,a,b.x[0]\n1,10,\n2,10,2.5\n3,30,3.5\n");
        export_csv(
            &s,
            &["b.x[0]".into()],
            Some((2.5, 10.0)),
            CsvMode::Joined,
            &p,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "time_s,b.x[0]\n3,3.5\n"
        );
        assert!(export_csv(&s, &["zz".into()], None, CsvMode::Joined, &p).is_err());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn per_channel_files() {
        let s = store();
        let p = std::env::temp_dir().join(format!("pb-csvpc-{}.csv", std::process::id()));
        let files = export_csv(&s, &[], None, CsvMode::PerChannel, &p).unwrap();
        assert_eq!(files.len(), 2);
        let b = std::fs::read_to_string(&files[1]).unwrap();
        assert_eq!(b, "time_s,b.x[0]\n2,2.5\n3,3.5\n");
        for f in files {
            let _ = std::fs::remove_file(f);
        }
    }
}
