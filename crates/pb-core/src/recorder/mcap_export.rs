//! MCAP export: one channel per message type, `protobuf` schema and message
//! encoding with the embedded `FileDescriptorSet`, log time = host timestamp
//! (converted to Unix time using the recording's start time).

use std::collections::{BTreeMap, HashMap};
use std::io::BufWriter;
use std::path::Path;

use prost::Message as _;
use prost_reflect::{ReflectMessage, Value};

use super::OfflineSession;
use crate::error::CoreError;
use crate::session::RecordKind;

fn err(e: impl std::fmt::Display) -> CoreError {
    CoreError::Recorder(format!("mcap: {e}"))
}

/// Exports a recording. Returns the number of messages written.
pub fn export_mcap(session: &OfflineSession, path: &Path) -> Result<usize, CoreError> {
    let f = std::fs::File::create(path).map_err(|e| CoreError::io(path, e))?;
    let mut w = mcap::Writer::new(BufWriter::new(f)).map_err(err)?;
    let fds = session.schema.fds_bytes().to_vec();
    let mut schemas: HashMap<String, u16> = HashMap::new();
    let mut channels: HashMap<String, u16> = HashMap::new();
    let mut seq: HashMap<u16, u32> = HashMap::new();
    let empty = BTreeMap::new();
    let first = session.records.first().map(|r| r.host_ts_ns).unwrap_or(0);
    let base = session.header.created_unix_ns;
    let mut count = 0usize;
    for d in session.decoded() {
        let log_time = base + d.record.host_ts_ns.saturating_sub(first);
        let (topic, schema_name, data, encoding): (String, Option<String>, Vec<u8>, &str) =
            match (&d.msg, d.record.kind) {
                (Some(m), kind) => {
                    let dir = if kind == RecordKind::TxFrame {
                        "tx"
                    } else {
                        "rx"
                    };
                    // Envelope: publish the oneof member's message on its own topic.
                    let inner = m
                        .member
                        .as_deref()
                        .and_then(|name| {
                            m.msg.get_field_by_name(name).map(|v| (name.to_string(), v))
                        })
                        .and_then(|(name, v)| match v.as_ref() {
                            Value::Message(im) => Some((name, im.clone())),
                            _ => None,
                        });
                    match inner {
                        Some((name, im)) => (
                            format!("/{dir}/{name}"),
                            Some(im.descriptor().full_name().to_string()),
                            im.encode_to_vec(),
                            "protobuf",
                        ),
                        None => (
                            format!(
                                "/{dir}/{}",
                                m.type_name.rsplit('.').next().unwrap_or(&m.type_name)
                            ),
                            Some(m.type_name.clone()),
                            m.raw[m.payload_offset.min(m.raw.len())..].to_vec(),
                            "protobuf",
                        ),
                    }
                }
                (None, RecordKind::TextLine) => (
                    "/text".into(),
                    None,
                    serde_json::json!({ "text": String::from_utf8_lossy(&d.record.bytes) })
                        .to_string()
                        .into_bytes(),
                    "json",
                ),
                (None, RecordKind::Marker) => (
                    "/markers".into(),
                    None,
                    serde_json::json!({ "label": String::from_utf8_lossy(&d.record.bytes) })
                        .to_string()
                        .into_bytes(),
                    "json",
                ),
                (None, _) => continue,
            };
        let schema_id = match &schema_name {
            Some(n) => match schemas.get(n) {
                Some(id) => *id,
                None => {
                    let id = w.add_schema(n, "protobuf", &fds).map_err(err)?;
                    schemas.insert(n.clone(), id);
                    id
                }
            },
            None => 0,
        };
        let channel_id = match channels.get(&topic) {
            Some(id) => *id,
            None => {
                let id = w
                    .add_channel(schema_id, &topic, encoding, &empty)
                    .map_err(err)?;
                channels.insert(topic.clone(), id);
                id
            }
        };
        let s = seq.entry(channel_id).or_insert(0);
        *s += 1;
        w.write_to_known_channel(
            &mcap::records::MessageHeader {
                channel_id,
                sequence: *s,
                log_time,
                publish_time: log_time,
            },
            &data,
        )
        .map_err(err)?;
        count += 1;
    }
    w.finish().map_err(err)?;
    Ok(count)
}
