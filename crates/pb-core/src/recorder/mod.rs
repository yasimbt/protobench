//! Session recording (`.pbsession`), offline decoding and export.
//!
//! File layout (all integers little-endian):
//!
//! ```text
//! "PBSESS01"                       8-byte magic
//! u32 header_len, header JSON       tool version, project name, project.toml text, start time
//! u32 fds_len, FileDescriptorSet    compiled schema, so the file decodes on its own
//! records until EOF:
//!   u8 kind (1 rx_frame, 2 tx_frame, 3 text_line, 4 marker)
//!   u64 host_ts_ns                  session clock
//!   u32 len, bytes                  codec bytes (frames) or UTF-8 (text, marker)
//! ```

mod csv;
mod join;
pub mod mcap_export;
mod offline;
mod parquet;

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub use csv::{export_csv, CsvMode};
pub use offline::{DecodedRecord, OfflineSession};
pub use parquet::export_parquet;

use crate::error::CoreError;
use crate::project::Project;
use crate::session::{RecordKind, RecordSink};

/// File magic.
pub const MAGIC: &[u8; 8] = b"PBSESS01";
/// Largest record accepted when reading.
pub const MAX_RECORD: usize = 16 * 1024 * 1024;
/// Largest header or descriptor-set block accepted when reading.
pub const MAX_BLOCK: usize = 64 * 1024 * 1024;

/// Session file header.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionHeader {
    /// ProtoBench version that wrote the file.
    pub tool_version: String,
    /// Project name.
    pub project_name: String,
    /// Wall-clock start, ns since the Unix epoch.
    pub created_unix_ns: u64,
    /// Text of `project.toml` at recording time.
    pub project_toml: String,
    /// Free-form notes.
    #[serde(default)]
    pub notes: String,
}

/// One record.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// Kind.
    pub kind: RecordKind,
    /// Host time in ns.
    pub host_ts_ns: u64,
    /// Payload.
    pub bytes: Vec<u8>,
}

/// Default path for a new recording: `recordings/<project>_<YYYYmmdd_HHMMSS>.pbsession`.
pub fn recording_path(dir: &Path, project_name: &str) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let safe: String = project_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{safe}_{stamp}.pbsession"))
}

/// Writes a session file.
pub struct SessionWriter {
    path: PathBuf,
    out: Option<BufWriter<File>>,
    error: Option<String>,
    records: u64,
}

impl SessionWriter {
    /// Creates the file and writes the header (parent folders are created).
    pub fn create(path: &Path, project: &Project) -> Result<SessionWriter, CoreError> {
        let schema = project.require_schema()?;
        let header = SessionHeader {
            tool_version: crate::VERSION.to_string(),
            project_name: project.config.name.clone(),
            created_unix_ns: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
            project_toml: project.config_text.clone(),
            notes: String::new(),
        };
        Self::create_with(path, &header, schema.fds_bytes())
    }

    /// Creates a file with an explicit header and descriptor set.
    pub fn create_with(
        path: &Path,
        header: &SessionHeader,
        fds: &[u8],
    ) -> Result<SessionWriter, CoreError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| CoreError::io(parent, e))?;
            }
        }
        let f = File::create(path).map_err(|e| CoreError::io(path, e))?;
        let mut out = BufWriter::with_capacity(256 * 1024, f);
        let h = serde_json::to_vec(header).map_err(|e| CoreError::Recorder(e.to_string()))?;
        let io = |e| CoreError::io(path, e);
        out.write_all(MAGIC).map_err(io)?;
        out.write_all(&(h.len() as u32).to_le_bytes()).map_err(io)?;
        out.write_all(&h).map_err(io)?;
        out.write_all(&(fds.len() as u32).to_le_bytes())
            .map_err(io)?;
        out.write_all(fds).map_err(io)?;
        Ok(SessionWriter {
            path: path.to_path_buf(),
            out: Some(out),
            error: None,
            records: 0,
        })
    }

    /// File path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records written.
    pub fn records(&self) -> u64 {
        self.records
    }

    fn write_record(&mut self, kind: RecordKind, ts: u64, bytes: &[u8]) -> std::io::Result<()> {
        let Some(out) = self.out.as_mut() else {
            return Ok(());
        };
        out.write_all(&[kind as u8])?;
        out.write_all(&ts.to_le_bytes())?;
        out.write_all(&(bytes.len() as u32).to_le_bytes())?;
        out.write_all(bytes)
    }
}

impl RecordSink for SessionWriter {
    fn record(&mut self, kind: RecordKind, host_ts_ns: u64, bytes: &[u8]) {
        if self.error.is_some() {
            return;
        }
        match self.write_record(kind, host_ts_ns, bytes) {
            Ok(()) => self.records += 1,
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn finish(&mut self) -> Result<(), CoreError> {
        if let Some(mut out) = self.out.take() {
            out.flush().map_err(|e| CoreError::io(&self.path, e))?;
        }
        match &self.error {
            Some(e) => Err(CoreError::Recorder(format!("{}: {e}", self.path.display()))),
            None => Ok(()),
        }
    }

    fn error(&self) -> Option<String> {
        self.error.clone()
    }
}

impl Drop for SessionWriter {
    fn drop(&mut self) {
        if let Some(mut out) = self.out.take() {
            let _ = out.flush();
        }
    }
}

/// Reads a session file.
pub struct SessionReader {
    /// Header.
    pub header: SessionHeader,
    /// Encoded `FileDescriptorSet`.
    pub fds: Vec<u8>,
    input: BufReader<File>,
    path: PathBuf,
    /// Set if the file ended in the middle of a record.
    pub truncated: bool,
}

fn read_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

impl SessionReader {
    /// Opens a file and reads its header.
    pub fn open(path: &Path) -> Result<SessionReader, CoreError> {
        let f = File::open(path).map_err(|e| CoreError::io(path, e))?;
        let mut input = BufReader::with_capacity(256 * 1024, f);
        let bad = |m: &str| CoreError::Recorder(format!("{}: {m}", path.display()));
        let mut magic = [0u8; 8];
        input
            .read_exact(&mut magic)
            .map_err(|_| bad("not a ProtoBench session file (too short)"))?;
        if &magic != MAGIC {
            return Err(bad("not a ProtoBench session file (bad magic)"));
        }
        let hlen = read_u32(&mut input).map_err(|_| bad("truncated header"))? as usize;
        if hlen > MAX_BLOCK {
            return Err(bad("header too large"));
        }
        let mut h = vec![0u8; hlen];
        input
            .read_exact(&mut h)
            .map_err(|_| bad("truncated header"))?;
        let header: SessionHeader =
            serde_json::from_slice(&h).map_err(|e| bad(&format!("bad header: {e}")))?;
        let flen = read_u32(&mut input).map_err(|_| bad("truncated descriptor set"))? as usize;
        if flen > MAX_BLOCK {
            return Err(bad("descriptor set too large"));
        }
        let mut fds = vec![0u8; flen];
        input
            .read_exact(&mut fds)
            .map_err(|_| bad("truncated descriptor set"))?;
        Ok(SessionReader {
            header,
            fds,
            input,
            path: path.to_path_buf(),
            truncated: false,
        })
    }

    /// Reads the next record; `Ok(None)` at the end. A partial trailing
    /// record (e.g. after a crash) ends the file and sets `truncated`.
    pub fn next_record(&mut self) -> Result<Option<Record>, CoreError> {
        let mut kind = [0u8; 1];
        match self.input.read(&mut kind) {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(e) => return Err(CoreError::io(&self.path, e)),
        }
        let mut ts = [0u8; 8];
        let mut len = [0u8; 4];
        if self.input.read_exact(&mut ts).is_err() || self.input.read_exact(&mut len).is_err() {
            self.truncated = true;
            return Ok(None);
        }
        let len = u32::from_le_bytes(len) as usize;
        if len > MAX_RECORD {
            return Err(CoreError::Recorder(format!(
                "{}: record of {len} bytes exceeds limit",
                self.path.display()
            )));
        }
        let Some(kind) = RecordKind::from_u8(kind[0]) else {
            return Err(CoreError::Recorder(format!(
                "{}: unknown record kind {}",
                self.path.display(),
                kind[0]
            )));
        };
        let mut bytes = vec![0u8; len];
        if self.input.read_exact(&mut bytes).is_err() {
            self.truncated = true;
            return Ok(None);
        }
        Ok(Some(Record {
            kind,
            host_ts_ns: u64::from_le_bytes(ts),
            bytes,
        }))
    }

    /// Reads all remaining records.
    pub fn read_all(&mut self) -> Result<Vec<Record>, CoreError> {
        let mut v = Vec::new();
        while let Some(r) = self.next_record()? {
            v.push(r);
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pb-rec-{}-{name}", std::process::id()))
    }

    #[test]
    fn write_read_round_trip() {
        let p = Project::open(&crate::schema::tests::demo_dir());
        let path = tmp("rt.pbsession");
        let mut w = SessionWriter::create(&path, &p).unwrap();
        w.record(RecordKind::TxFrame, 10, &[1, 2, 3]);
        w.record(RecordKind::RxFrame, 20, &[4, 5]);
        w.record(RecordKind::TextLine, 30, b"I: hi");
        w.record(RecordKind::Marker, 40, b"load on");
        w.finish().unwrap();
        assert_eq!(w.records(), 4);
        let mut r = SessionReader::open(&path).unwrap();
        assert_eq!(r.header.project_name, "Demo Sensor");
        assert!(r.header.project_toml.contains("[framing]"));
        assert!(!r.fds.is_empty());
        let recs = r.read_all().unwrap();
        assert_eq!(recs.len(), 4);
        assert_eq!(
            recs[0],
            Record {
                kind: RecordKind::TxFrame,
                host_ts_ns: 10,
                bytes: vec![1, 2, 3]
            }
        );
        assert_eq!(recs[3].bytes, b"load on");
        assert!(!r.truncated);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn truncated_and_bad_files() {
        let p = Project::open(&crate::schema::tests::demo_dir());
        let path = tmp("tr.pbsession");
        let mut w = SessionWriter::create(&path, &p).unwrap();
        w.record(RecordKind::RxFrame, 1, &[9; 100]);
        w.record(RecordKind::RxFrame, 2, &[9; 100]);
        w.finish().unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(len - 10).unwrap();
        let mut r = SessionReader::open(&path).unwrap();
        assert_eq!(r.read_all().unwrap().len(), 1);
        assert!(r.truncated);
        std::fs::write(&path, b"garbage").unwrap();
        assert!(SessionReader::open(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn default_path_has_timestamp() {
        let p = recording_path(Path::new("rec"), "Demo Sensor");
        let s = p.to_string_lossy();
        assert!(s.starts_with("rec/Demo_Sensor_"), "{s}");
        assert!(s.ends_with(".pbsession"));
    }
}
