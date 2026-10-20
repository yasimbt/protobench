//! Decoding a recording without the original project folder.

use std::path::Path;
use std::sync::Arc;

use super::{Record, SessionHeader, SessionReader};
use crate::codec::{get_int_at_path, Codec, DecodedMessage, Role};
use crate::config::{load_config_str, ProjectConfig, TimestampSource};
use crate::error::CoreError;
use crate::schema::{FieldPath, Schema};
use crate::session::RecordKind;
use crate::telemetry::{DeviceTimeBase, Extractor, TelemetryStore};

/// A record with its decoded message.
#[derive(Debug, Clone)]
pub struct DecodedRecord {
    /// The raw record.
    pub record: Record,
    /// Decoded message for rx/tx frames.
    pub msg: Option<DecodedMessage>,
    /// Decode error, if decoding failed.
    pub error: Option<String>,
}

impl DecodedRecord {
    /// JSON form: `{t, kind, type, msg}` or `{t, kind, text}`.
    pub fn to_json(&self) -> serde_json::Value {
        let t = self.record.host_ts_ns as f64 / 1e9;
        let kind = self.record.kind.as_str();
        match (&self.msg, self.record.kind) {
            (Some(m), _) => serde_json::json!({
                "t": t, "kind": kind, "type": m.type_name, "name": m.kind(), "msg": m.to_json()
            }),
            (None, RecordKind::TextLine | RecordKind::Marker) => serde_json::json!({
                "t": t, "kind": kind, "text": String::from_utf8_lossy(&self.record.bytes)
            }),
            (None, _) => serde_json::json!({
                "t": t, "kind": kind, "error": self.error, "hex": crate::util::hex_compact(&self.record.bytes)
            }),
        }
    }

    /// One-line text form.
    pub fn to_line(&self) -> String {
        let t = self.record.host_ts_ns as f64 / 1e9;
        let kind = self.record.kind.as_str();
        match (&self.msg, self.record.kind) {
            (Some(m), _) => format!("{t:12.6} {kind:<9} {:<12} {}", m.kind(), m.to_json()),
            (None, RecordKind::TextLine | RecordKind::Marker) => {
                format!(
                    "{t:12.6} {kind:<9} {}",
                    String::from_utf8_lossy(&self.record.bytes)
                )
            }
            (None, _) => format!(
                "{t:12.6} {kind:<9} <{}> {}",
                self.error.as_deref().unwrap_or("undecoded"),
                crate::util::hex(&self.record.bytes)
            ),
        }
    }
}

/// A recording with its embedded config and schema.
pub struct OfflineSession {
    /// Header.
    pub header: SessionHeader,
    /// Config parsed from the embedded `project.toml`.
    pub config: ProjectConfig,
    /// Schema from the embedded descriptor set.
    pub schema: Arc<Schema>,
    /// All records.
    pub records: Vec<Record>,
    /// True if the file ended mid-record.
    pub truncated: bool,
    host: Codec,
    device: Codec,
}

impl OfflineSession {
    /// Opens and fully reads a recording.
    pub fn open(path: &Path) -> Result<OfflineSession, CoreError> {
        let mut r = SessionReader::open(path)?;
        let records = r.read_all()?;
        let loaded = load_config_str(&r.header.project_toml, Path::new("project.toml (embedded)"));
        let config = loaded.config.ok_or_else(|| {
            CoreError::Recorder(format!(
                "{}: embedded project.toml is invalid: {}",
                path.display(),
                loaded
                    .diagnostics
                    .first()
                    .map(|d| d.to_string())
                    .unwrap_or_default()
            ))
        })?;
        let schema = Arc::new(
            Schema::from_descriptor_set(&r.fds).map_err(|e| CoreError::Schema(e.to_string()))?,
        );
        let host = Codec::new(&config, &schema, Role::Host)?;
        let device = Codec::new(&config, &schema, Role::Device)?;
        Ok(OfflineSession {
            header: r.header.clone(),
            config,
            schema,
            records,
            truncated: r.truncated,
            host,
            device,
        })
    }

    /// The host-side codec.
    pub fn codec(&self) -> &Codec {
        &self.host
    }

    /// Decodes every record. Rx frames use the host codec, tx frames the
    /// device codec (they are host-to-device messages).
    pub fn decoded(&self) -> Vec<DecodedRecord> {
        self.records
            .iter()
            .map(|r| {
                let res = match r.kind {
                    RecordKind::RxFrame => Some(self.host.decode(&r.bytes, r.host_ts_ns)),
                    RecordKind::TxFrame => Some(self.device.decode(&r.bytes, r.host_ts_ns)),
                    _ => None,
                };
                match res {
                    Some(Ok(m)) => DecodedRecord {
                        record: r.clone(),
                        msg: Some(m),
                        error: None,
                    },
                    Some(Err(e)) => DecodedRecord {
                        record: r.clone(),
                        msg: None,
                        error: Some(e.to_string()),
                    },
                    None => DecodedRecord {
                        record: r.clone(),
                        msg: None,
                        error: None,
                    },
                }
            })
            .collect()
    }

    /// Re-runs telemetry extraction over the received messages.
    pub fn telemetry(&self, capacity: usize) -> Result<TelemetryStore, CoreError> {
        let store = TelemetryStore::new(capacity);
        let mut ex = Extractor::new(&self.config, &self.host)?;
        let ts_field = match self.config.timestamp.source {
            TimestampSource::Device => self
                .config
                .timestamp
                .field
                .as_deref()
                .and_then(|p| FieldPath::parse(p).ok()),
            TimestampSource::Host => None,
        };
        let mut tb =
            DeviceTimeBase::new(self.config.timestamp.unit, self.config.timestamp.wrap_bits);
        for r in &self.records {
            if r.kind != RecordKind::RxFrame {
                continue;
            }
            let Ok(mut m) = self.host.decode(&r.bytes, r.host_ts_ns) else {
                continue;
            };
            let host_s = r.host_ts_ns as f64 / 1e9;
            if let Some(p) = &ts_field {
                if let Some(raw) = get_int_at_path(&m.msg, p) {
                    m.device_ts = Some(tb.map(raw, host_s));
                }
            }
            let t = m.device_ts.unwrap_or(host_s);
            ex.process(&self.host, &m, t, &store);
        }
        Ok(store)
    }
}
