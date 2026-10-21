//! Decoding a recording without the original project folders.

use std::path::Path;
use std::sync::Arc;

use super::{Record, SessionHeader, SessionReader};
use crate::codec::{get_int_at_path, Codec, DecodedMessage, Role};
use crate::config::{channel_prefix, load_config_str, ProjectConfig, TimestampSource, MAIN_DEVICE};
use crate::error::CoreError;
use crate::schema::{FieldPath, Schema};
use crate::session::RecordKind;
use crate::telemetry::{DeviceTimeBase, Extractor, TelemetryStore};

/// A record with its decoded message.
#[derive(Debug, Clone)]
pub struct DecodedRecord {
    /// The raw record.
    pub record: Record,
    /// Name of the device it belongs to.
    pub device: String,
    /// Decoded message for rx/tx frames.
    pub msg: Option<DecodedMessage>,
    /// Decode error, if decoding failed.
    pub error: Option<String>,
}

impl DecodedRecord {
    /// JSON form: `{t, kind, type, msg}` or `{t, kind, text}`, plus
    /// `device` for a device other than `main`.
    pub fn to_json(&self) -> serde_json::Value {
        let t = self.record.host_ts_ns as f64 / 1e9;
        let kind = self.record.kind.as_str();
        let mut j = match (&self.msg, self.record.kind) {
            (Some(m), _) => serde_json::json!({
                "t": t, "kind": kind, "type": m.type_name, "name": m.kind(), "msg": m.to_json()
            }),
            (None, RecordKind::TextLine | RecordKind::Marker) => serde_json::json!({
                "t": t, "kind": kind, "text": String::from_utf8_lossy(&self.record.bytes)
            }),
            (None, _) => serde_json::json!({
                "t": t, "kind": kind, "error": self.error, "hex": crate::util::hex_compact(&self.record.bytes)
            }),
        };
        if self.device != MAIN_DEVICE {
            j["device"] = serde_json::Value::String(self.device.clone());
        }
        j
    }

    /// One-line text form. Messages and text of a device other than `main`
    /// are tagged `device:`.
    pub fn to_line(&self) -> String {
        let t = self.record.host_ts_ns as f64 / 1e9;
        let kind = self.record.kind.as_str();
        let tag = if self.device == MAIN_DEVICE {
            String::new()
        } else {
            format!("{}:", self.device)
        };
        match (&self.msg, self.record.kind) {
            (Some(m), _) => format!(
                "{t:12.6} {kind:<9} {:<12} {}",
                format!("{tag}{}", m.kind()),
                m.to_json()
            ),
            (None, RecordKind::TextLine | RecordKind::Marker) => {
                format!(
                    "{t:12.6} {kind:<9} {tag}{}",
                    String::from_utf8_lossy(&self.record.bytes)
                )
            }
            (None, _) => format!(
                "{t:12.6} {kind:<9} {tag}<{}> {}",
                self.error.as_deref().unwrap_or("undecoded"),
                crate::util::hex(&self.record.bytes)
            ),
        }
    }
}

/// One device of a recording, with its embedded config and schema.
pub struct OfflineDevice {
    /// Device name.
    pub name: String,
    /// Config parsed from the embedded `project.toml`.
    pub config: ProjectConfig,
    /// Schema from the embedded descriptor set.
    pub schema: Arc<Schema>,
    host: Codec,
    device: Codec,
}

impl OfflineDevice {
    /// Host-side codec (decodes received frames).
    pub fn codec(&self) -> &Codec {
        &self.host
    }
}

/// A recording with its embedded configs and schemas.
pub struct OfflineSession {
    /// Header.
    pub header: SessionHeader,
    /// Config of `main`.
    pub config: ProjectConfig,
    /// Schema of `main`.
    pub schema: Arc<Schema>,
    /// Every device, `main` first.
    pub devices: Vec<OfflineDevice>,
    /// All records.
    pub records: Vec<Record>,
    /// True if the file ended mid-record.
    pub truncated: bool,
}

impl OfflineSession {
    /// Opens and fully reads a recording (format version 1 or 2).
    pub fn open(path: &Path) -> Result<OfflineSession, CoreError> {
        let mut r = SessionReader::open(path)?;
        let records = r.read_all()?;
        let mut devices = Vec::new();
        for (d, fds) in r.header.devices.iter().zip(&r.device_fds) {
            let loaded = load_config_str(
                &d.project_toml,
                Path::new(&format!("project.toml of `{}` (embedded)", d.name)),
            );
            let config = loaded.config.ok_or_else(|| {
                CoreError::Recorder(format!(
                    "{}: embedded project.toml of `{}` is invalid: {}",
                    path.display(),
                    d.name,
                    loaded
                        .diagnostics
                        .first()
                        .map(|d| d.to_string())
                        .unwrap_or_default()
                ))
            })?;
            let schema = Arc::new(
                Schema::from_descriptor_set(fds).map_err(|e| CoreError::Schema(e.to_string()))?,
            );
            let host = Codec::new(&config, &schema, Role::Host)?;
            let device = Codec::new(&config, &schema, Role::Device)?;
            devices.push(OfflineDevice {
                name: d.name.clone(),
                config,
                schema,
                host,
                device,
            });
        }
        let main = devices.first().ok_or_else(|| {
            CoreError::Recorder(format!("{}: recording lists no devices", path.display()))
        })?;
        Ok(OfflineSession {
            header: r.header.clone(),
            config: main.config.clone(),
            schema: main.schema.clone(),
            devices,
            records,
            truncated: r.truncated,
        })
    }

    /// The host-side codec of `main`.
    pub fn codec(&self) -> &Codec {
        &self.devices[0].host
    }

    /// Name of the device a record belongs to (`?3` for an index the
    /// header does not list).
    pub fn device_name(&self, index: u8) -> String {
        self.devices
            .get(usize::from(index))
            .map(|d| d.name.clone())
            .unwrap_or_else(|| format!("?{index}"))
    }

    /// Decodes every record with its device's schema. Rx frames use the
    /// host codec, tx frames the device codec (they are host-to-device
    /// messages).
    pub fn decoded(&self) -> Vec<DecodedRecord> {
        self.records
            .iter()
            .map(|r| {
                let dev = self.devices.get(usize::from(r.device));
                let res = match (r.kind, dev) {
                    (RecordKind::RxFrame, Some(d)) => Some(
                        d.host
                            .decode(&r.bytes, r.host_ts_ns)
                            .map_err(|e| e.to_string()),
                    ),
                    (RecordKind::TxFrame, Some(d)) => Some(
                        d.device
                            .decode(&r.bytes, r.host_ts_ns)
                            .map_err(|e| e.to_string()),
                    ),
                    (RecordKind::RxFrame | RecordKind::TxFrame, None) => {
                        Some(Err(format!("unknown device index {}", r.device)))
                    }
                    _ => None,
                };
                let (msg, error) = match res {
                    Some(Ok(m)) => (Some(m), None),
                    Some(Err(e)) => (None, Some(e)),
                    None => (None, None),
                };
                DecodedRecord {
                    record: r.clone(),
                    device: self.device_name(r.device),
                    msg,
                    error,
                }
            })
            .collect()
    }

    /// Re-runs telemetry extraction over the received messages of every
    /// device, into one store with the live channel names (`ref.` prefix
    /// for other devices).
    pub fn telemetry(&self, capacity: usize) -> Result<TelemetryStore, CoreError> {
        let store = TelemetryStore::new(capacity);
        struct Dev<'a> {
            d: &'a OfflineDevice,
            ex: Extractor,
            ts_field: Option<FieldPath>,
            tb: DeviceTimeBase,
        }
        let mut devs = Vec::new();
        for d in &self.devices {
            let ts_field = match d.config.timestamp.source {
                TimestampSource::Device => d
                    .config
                    .timestamp
                    .field
                    .as_deref()
                    .and_then(|p| FieldPath::parse(p).ok()),
                TimestampSource::Host => None,
            };
            devs.push(Dev {
                d,
                ex: Extractor::new(&d.config, &d.host)?.with_prefix(&channel_prefix(&d.name)),
                ts_field,
                tb: DeviceTimeBase::new(d.config.timestamp.unit, d.config.timestamp.wrap_bits),
            });
        }
        for r in &self.records {
            if r.kind != RecordKind::RxFrame {
                continue;
            }
            let Some(dev) = devs.get_mut(usize::from(r.device)) else {
                continue;
            };
            let Ok(mut m) = dev.d.host.decode(&r.bytes, r.host_ts_ns) else {
                continue;
            };
            let host_s = r.host_ts_ns as f64 / 1e9;
            if let Some(p) = &dev.ts_field {
                if let Some(raw) = get_int_at_path(&m.msg, p) {
                    m.device_ts = Some(dev.tb.map(raw, host_s));
                }
            }
            let t = m.device_ts.unwrap_or(host_s);
            dev.ex.process(&dev.d.host, &m, t, &store);
        }
        Ok(store)
    }
}
