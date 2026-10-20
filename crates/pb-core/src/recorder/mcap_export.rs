//! MCAP export: one channel per message type, `protobuf` schema and message
//! encoding with the embedded `FileDescriptorSet`, log time = host timestamp
//! (converted to Unix time using the recording's start time).

use std::collections::{BTreeMap, HashMap};
use std::io::BufWriter;
use std::path::Path;

use super::OfflineSession;
use crate::error::CoreError;
use crate::session::{Direction, RecordKind};
use crate::topics;

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
        let tm = match (&d.msg, d.record.kind) {
            (Some(m), RecordKind::TxFrame) => topics::for_message(m, Direction::Tx),
            (Some(m), _) => topics::for_message(m, Direction::Rx),
            (None, RecordKind::TextLine) => {
                topics::for_text(&String::from_utf8_lossy(&d.record.bytes))
            }
            (None, RecordKind::Marker) => {
                topics::for_marker(&String::from_utf8_lossy(&d.record.bytes))
            }
            (None, _) => continue,
        };
        let topics::TopicMessage {
            topic,
            schema_name,
            encoding,
            data,
        } = tm;
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
