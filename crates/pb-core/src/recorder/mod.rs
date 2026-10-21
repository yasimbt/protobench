//! Session recording (`.pbsession`), offline decoding and export.
//!
//! File layout, version 2 (all integers little-endian):
//!
//! ```text
//! "PBSESS02"                       8-byte magic
//! u32 header_len, header JSON       tool version, project name, project.toml text, start
//!                                   time, and `devices`: name, project name and
//!                                   project.toml of every device, `main` first
//! u32 device_count
//! device_count × (u32 fds_len, FileDescriptorSet)
//!                                   each device's compiled schema, in `devices` order,
//!                                   so the file decodes on its own
//! records until EOF:
//!   u8 kind (1 rx_frame, 2 tx_frame, 3 text_line, 4 marker)
//!   u8 device                       index into `devices`
//!   u64 host_ts_ns                  session clock, shared by all devices
//!   u32 len, bytes                  codec bytes (frames) or UTF-8 (text, marker)
//! ```
//!
//! Version 1 (`"PBSESS01"`, written up to round 3) has one device: a single
//! descriptor set after the header and no device byte in records. It is
//! still read, as device `main`.

mod csv;
mod join;
pub mod mcap_export;
mod offline;
mod parquet;

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

pub use csv::{export_csv, CsvMode};
pub use offline::{DecodedRecord, OfflineDevice, OfflineSession};
pub use parquet::export_parquet;

use crate::error::CoreError;
use crate::project::Project;
use crate::session::{RecordKind, RecordSink};

/// File magic of the current format (version 2).
pub const MAGIC: &[u8; 8] = b"PBSESS02";
/// File magic of version 1 (one device), still read.
pub const MAGIC_V1: &[u8; 8] = b"PBSESS01";
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
    /// Every device, `main` first (version 2). A version-1 file has none;
    /// readers fill in `main` from the fields above.
    #[serde(default)]
    pub devices: Vec<DeviceHeader>,
}

/// A device in a recording.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeviceHeader {
    /// Device name (`main` or a `[[devices]]` name).
    pub name: String,
    /// Its project's name.
    pub project_name: String,
    /// Text of its `project.toml` at recording time.
    pub project_toml: String,
}

/// One record.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// Kind.
    pub kind: RecordKind,
    /// Index of the device in the header (0 = `main`).
    pub device: u8,
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
    devices: Vec<String>,
}

impl SessionWriter {
    /// Creates the file and writes the header (parent folders are created).
    /// The header lists the project's own device and every `[[devices]]`
    /// entry, with their schemas.
    pub fn create(path: &Path, project: &Project) -> Result<SessionWriter, CoreError> {
        let mut devices = Vec::new();
        let mut schemas = Vec::new();
        for d in project.all_devices() {
            let schema = d.project.require_schema().map_err(|e| {
                CoreError::Recorder(format!("device `{}` has no schema: {e}", d.name))
            })?;
            devices.push(DeviceHeader {
                name: d.name.to_string(),
                project_name: d.project.config.name.clone(),
                project_toml: d.project.config_text.clone(),
            });
            schemas.push(schema.fds_bytes().to_vec());
        }
        let header = SessionHeader {
            tool_version: crate::VERSION.to_string(),
            project_name: project.config.name.clone(),
            created_unix_ns: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
            project_toml: project.config_text.clone(),
            notes: String::new(),
            devices,
        };
        let refs: Vec<&[u8]> = schemas.iter().map(Vec::as_slice).collect();
        Self::create_with(path, &header, &refs)
    }

    /// Creates a file with an explicit header and one descriptor set per
    /// device (`header.devices` order; one for a header without devices).
    pub fn create_with(
        path: &Path,
        header: &SessionHeader,
        fds: &[&[u8]],
    ) -> Result<SessionWriter, CoreError> {
        let devices: Vec<String> = if header.devices.is_empty() {
            vec![crate::config::MAIN_DEVICE.to_string()]
        } else {
            header.devices.iter().map(|d| d.name.clone()).collect()
        };
        if fds.len() != devices.len() || devices.len() > usize::from(u8::MAX) {
            return Err(CoreError::Recorder(format!(
                "{} descriptor sets for {} devices",
                fds.len(),
                devices.len()
            )));
        }
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
        for f in fds {
            out.write_all(&(f.len() as u32).to_le_bytes()).map_err(io)?;
            out.write_all(f).map_err(io)?;
        }
        Ok(SessionWriter {
            path: path.to_path_buf(),
            out: Some(out),
            error: None,
            records: 0,
            devices,
        })
    }

    /// Device names in header order.
    pub fn devices(&self) -> &[String] {
        &self.devices
    }

    /// Writes one record of device `device` (an index into
    /// [`SessionWriter::devices`]).
    pub fn record_device(&mut self, kind: RecordKind, device: u8, host_ts_ns: u64, bytes: &[u8]) {
        if self.error.is_some() {
            return;
        }
        match self.write_record(kind, device, host_ts_ns, bytes) {
            Ok(()) => self.records += 1,
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Flushes buffered records, keeping the file open.
    pub fn flush(&mut self) -> Result<(), CoreError> {
        if let Some(out) = self.out.as_mut() {
            if let Err(e) = out.flush() {
                self.error.get_or_insert(e.to_string());
            }
        }
        match &self.error {
            Some(e) => Err(CoreError::Recorder(format!("{}: {e}", self.path.display()))),
            None => Ok(()),
        }
    }

    /// File path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records written.
    pub fn records(&self) -> u64 {
        self.records
    }

    fn write_record(
        &mut self,
        kind: RecordKind,
        device: u8,
        ts: u64,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        let Some(out) = self.out.as_mut() else {
            return Ok(());
        };
        out.write_all(&[kind as u8, device])?;
        out.write_all(&ts.to_le_bytes())?;
        out.write_all(&(bytes.len() as u32).to_le_bytes())?;
        out.write_all(bytes)
    }
}

/// A sink writes records of the project's own device (`main`).
impl RecordSink for SessionWriter {
    fn record(&mut self, kind: RecordKind, host_ts_ns: u64, bytes: &[u8]) {
        self.record_device(kind, 0, host_ts_ns, bytes);
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

/// One recording written by several sessions: each device's session gets
/// a sink that tags its records with the device's index. The file closes
/// when the last sink and this handle are gone; finishing a sink only
/// flushes, so one device can stop while the others keep recording.
#[derive(Clone)]
pub struct SharedRecording {
    writer: Arc<Mutex<SessionWriter>>,
    devices: Vec<String>,
    path: PathBuf,
}

impl SharedRecording {
    /// Wraps a writer created with [`SessionWriter::create`].
    pub fn new(writer: SessionWriter) -> SharedRecording {
        SharedRecording {
            devices: writer.devices().to_vec(),
            path: writer.path().to_path_buf(),
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    /// The sink for device `name`, or None if the header does not list it.
    pub fn sink(&self, name: &str) -> Option<Box<dyn RecordSink>> {
        let i = self.devices.iter().position(|d| d == name)?;
        Some(Box::new(DeviceSink {
            device: i as u8,
            writer: self.writer.clone(),
        }))
    }

    /// File path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records written so far.
    pub fn records(&self) -> u64 {
        self.writer.lock().records()
    }

    /// Flushes and closes the file (later records are dropped).
    pub fn finish(&self) -> Result<(), CoreError> {
        self.writer.lock().finish()
    }
}

struct DeviceSink {
    device: u8,
    writer: Arc<Mutex<SessionWriter>>,
}

impl RecordSink for DeviceSink {
    fn record(&mut self, kind: RecordKind, host_ts_ns: u64, bytes: &[u8]) {
        self.writer
            .lock()
            .record_device(kind, self.device, host_ts_ns, bytes);
    }

    fn finish(&mut self) -> Result<(), CoreError> {
        self.writer.lock().flush()
    }

    fn error(&self) -> Option<String> {
        self.writer.lock().error()
    }
}

/// Reads a session file.
pub struct SessionReader {
    /// Header. For a version-1 file, `devices` is filled in with `main`.
    pub header: SessionHeader,
    /// Encoded `FileDescriptorSet` of `main`.
    pub fds: Vec<u8>,
    /// Encoded `FileDescriptorSet` of every device, in `header.devices`
    /// order.
    pub device_fds: Vec<Vec<u8>>,
    /// Format version (1 or 2).
    pub version: u8,
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
        let version = match &magic {
            m if m == MAGIC => 2,
            m if m == MAGIC_V1 => 1,
            m if m.starts_with(b"PBSESS") => {
                return Err(bad(
                    "written by a newer ProtoBench (unknown format version); update to read it",
                ))
            }
            _ => return Err(bad("not a ProtoBench session file (bad magic)")),
        };
        let hlen = read_u32(&mut input).map_err(|_| bad("truncated header"))? as usize;
        if hlen > MAX_BLOCK {
            return Err(bad("header too large"));
        }
        let mut h = vec![0u8; hlen];
        input
            .read_exact(&mut h)
            .map_err(|_| bad("truncated header"))?;
        let mut header: SessionHeader =
            serde_json::from_slice(&h).map_err(|e| bad(&format!("bad header: {e}")))?;
        let count = if version == 1 {
            1
        } else {
            read_u32(&mut input).map_err(|_| bad("truncated descriptor sets"))? as usize
        };
        if version == 1 || header.devices.is_empty() {
            header.devices = vec![DeviceHeader {
                name: crate::config::MAIN_DEVICE.into(),
                project_name: header.project_name.clone(),
                project_toml: header.project_toml.clone(),
            }];
        }
        if count != header.devices.len() {
            return Err(bad(&format!(
                "{count} descriptor sets for {} devices",
                header.devices.len()
            )));
        }
        let mut device_fds = Vec::with_capacity(count);
        for _ in 0..count {
            let flen = read_u32(&mut input).map_err(|_| bad("truncated descriptor set"))? as usize;
            if flen > MAX_BLOCK {
                return Err(bad("descriptor set too large"));
            }
            let mut fds = vec![0u8; flen];
            input
                .read_exact(&mut fds)
                .map_err(|_| bad("truncated descriptor set"))?;
            device_fds.push(fds);
        }
        Ok(SessionReader {
            header,
            fds: device_fds[0].clone(),
            device_fds,
            version,
            input,
            path: path.to_path_buf(),
            truncated: false,
        })
    }

    /// Index of a device by name. A recording with one device plays it
    /// for any name (a replay profile in another project, an old file).
    pub fn device_index(&self, name: &str) -> Option<u8> {
        match self.header.devices.iter().position(|d| d.name == name) {
            Some(i) => Some(i as u8),
            None if self.header.devices.len() == 1 => Some(0),
            None => None,
        }
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
        let mut device = [0u8; 1];
        let mut ts = [0u8; 8];
        let mut len = [0u8; 4];
        let device_ok = self.version == 1 || self.input.read_exact(&mut device).is_ok();
        if !device_ok
            || self.input.read_exact(&mut ts).is_err()
            || self.input.read_exact(&mut len).is_err()
        {
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
            device: device[0],
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
                device: 0,
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

    /// A recording written by round 3 (format version 1, one device).
    fn round3_fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/round3-demo.pbsession")
    }

    #[test]
    fn round3_recording_still_opens_and_exports() {
        let path = round3_fixture();
        let mut r = SessionReader::open(&path).unwrap();
        assert_eq!(r.version, 1);
        assert_eq!(r.header.devices.len(), 1);
        assert_eq!(r.header.devices[0].name, "main");
        assert_eq!(r.device_index("main"), Some(0));
        // A single-device recording plays for any device name.
        assert_eq!(r.device_index("ref"), Some(0));
        let recs = r.read_all().unwrap();
        assert_eq!(recs.len(), 306);
        assert!(recs.iter().all(|x| x.device == 0));
        let off = OfflineSession::open(&path).unwrap();
        assert_eq!(off.devices.len(), 1);
        let decoded = off.decoded();
        assert!(decoded
            .iter()
            .all(|d| d.error.is_none() && d.device == "main"));
        assert!(decoded[0].to_json().get("device").is_none());
        let store = off.telemetry(10_000).unwrap();
        assert!(store.get("telemetry.vout_raw").unwrap().total() > 290);
        let dir = tmp("r3");
        std::fs::create_dir_all(&dir).unwrap();
        export_csv(&store, &[], None, CsvMode::Joined, &dir.join("r3.csv")).unwrap();
        assert!(mcap_export::export_mcap(&off, &dir.join("r3.mcap")).unwrap() > 300);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn newer_format_versions_are_refused_clearly() {
        let path = tmp("v9.pbsession");
        std::fs::write(&path, b"PBSESS09\0\0\0\0").unwrap();
        let e = SessionReader::open(&path).err().unwrap().to_string();
        assert!(e.contains("newer ProtoBench"), "{e}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn shared_recording_tags_records_by_device() {
        let p = Project::open(&crate::schema::tests::demo_dir());
        let path = tmp("shared.pbsession");
        let rec = SharedRecording::new(SessionWriter::create(&path, &p).unwrap());
        let mut main = rec.sink("main").unwrap();
        let mut r = rec.sink("ref").unwrap();
        assert!(rec.sink("nope").is_none());
        main.record(RecordKind::RxFrame, 1, &[1]);
        r.record(RecordKind::RxFrame, 2, &[2]);
        // Finishing one device's sink keeps the file open for the others.
        r.finish().unwrap();
        main.record(RecordKind::TextLine, 3, b"x");
        main.finish().unwrap();
        rec.finish().unwrap();
        drop((main, r, rec));
        let mut rd = SessionReader::open(&path).unwrap();
        assert_eq!(rd.version, 2);
        let names: Vec<_> = rd.header.devices.iter().map(|d| d.name.clone()).collect();
        assert_eq!(names, ["main", "ref"]);
        assert_eq!(rd.device_fds.len(), 2);
        assert_eq!(rd.device_index("ref"), Some(1));
        assert_eq!(rd.device_index("nope"), None);
        let recs: Vec<(u8, u64)> = rd
            .read_all()
            .unwrap()
            .iter()
            .map(|x| (x.device, x.host_ts_ns))
            .collect();
        assert_eq!(recs, [(0, 1), (1, 2), (0, 3)]);
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
