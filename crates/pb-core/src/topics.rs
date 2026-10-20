//! Topic naming shared by MCAP export and the Foxglove WebSocket server.
//!
//! - Envelope mode: `/rx/<member>` and `/tx/<member>`, one topic per oneof
//!   member, the payload being the member's message (schema = its type).
//! - Other modes: `/rx/<Type>` and `/tx/<Type>`, the payload being the
//!   message without the ID prefix.
//! - Text lines go to `/text` (`{"text": …}`) and markers to `/markers`
//!   (`{"label": …}`), JSON encoded.

use prost::Message as _;
use prost_reflect::{Kind, MessageDescriptor, ReflectMessage, Value};

use crate::codec::{Codec, DecodedMessage};
use crate::config::CodecMode;
use crate::session::Direction;

/// Topic of text lines.
pub const TEXT_TOPIC: &str = "/text";
/// Topic of markers.
pub const MARKER_TOPIC: &str = "/markers";

/// JSON Schema of `/text` messages.
pub const TEXT_SCHEMA: &str =
    r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#;
/// JSON Schema of `/markers` messages.
pub const MARKER_SCHEMA: &str =
    r#"{"type":"object","properties":{"label":{"type":"string"}},"required":["label"]}"#;

/// A message ready to publish on a topic.
#[derive(Debug, Clone, PartialEq)]
pub struct TopicMessage {
    /// Topic name.
    pub topic: String,
    /// Full protobuf type name, or `None` for JSON topics.
    pub schema_name: Option<String>,
    /// `"protobuf"` or `"json"`.
    pub encoding: &'static str,
    /// Encoded payload.
    pub data: Vec<u8>,
}

/// A topic that can be announced before any message arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicInfo {
    /// Topic name.
    pub topic: String,
    /// Full protobuf type name, or `None` for JSON topics.
    pub schema_name: Option<String>,
}

fn dir_name(dir: Direction) -> &'static str {
    match dir {
        Direction::Rx => "rx",
        Direction::Tx => "tx",
    }
}

/// Topic and payload of a decoded message.
pub fn for_message(m: &DecodedMessage, dir: Direction) -> TopicMessage {
    let dir = dir_name(dir);
    // Envelope: publish the oneof member's message on its own topic.
    let inner = m
        .member
        .as_deref()
        .and_then(|name| m.msg.get_field_by_name(name).map(|v| (name.to_string(), v)))
        .and_then(|(name, v)| match v.as_ref() {
            Value::Message(im) => Some((name, im.clone())),
            _ => None,
        });
    match inner {
        Some((name, im)) => TopicMessage {
            topic: format!("/{dir}/{name}"),
            schema_name: Some(im.descriptor().full_name().to_string()),
            encoding: "protobuf",
            data: im.encode_to_vec(),
        },
        None => TopicMessage {
            topic: format!(
                "/{dir}/{}",
                m.type_name.rsplit('.').next().unwrap_or(&m.type_name)
            ),
            schema_name: Some(m.type_name.clone()),
            encoding: "protobuf",
            data: m.raw[m.payload_offset.min(m.raw.len())..].to_vec(),
        },
    }
}

/// A `/text` message.
pub fn for_text(line: &str) -> TopicMessage {
    TopicMessage {
        topic: TEXT_TOPIC.into(),
        schema_name: None,
        encoding: "json",
        data: serde_json::json!({ "text": line }).to_string().into_bytes(),
    }
}

/// A `/markers` message.
pub fn for_marker(label: &str) -> TopicMessage {
    TopicMessage {
        topic: MARKER_TOPIC.into(),
        schema_name: None,
        encoding: "json",
        data: serde_json::json!({ "label": label })
            .to_string()
            .into_bytes(),
    }
}

fn type_topics(dir: Direction, types: &[MessageDescriptor], envelope: bool) -> Vec<TopicInfo> {
    let d = dir_name(dir);
    let mut out = Vec::new();
    for t in types {
        let members: Vec<_> = if envelope {
            t.fields()
                .filter(|f| f.containing_oneof().is_some())
                .filter_map(|f| match f.kind() {
                    Kind::Message(m) => Some((f.name().to_string(), m)),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        if members.is_empty() {
            out.push(TopicInfo {
                topic: format!("/{d}/{}", t.name()),
                schema_name: Some(t.full_name().to_string()),
            });
        }
        for (name, m) in members {
            out.push(TopicInfo {
                topic: format!("/{d}/{name}"),
                schema_name: Some(m.full_name().to_string()),
            });
        }
    }
    out
}

/// Every topic a session with this codec can produce.
pub fn known_topics(codec: &Codec) -> Vec<TopicInfo> {
    let envelope = codec.mode() == CodecMode::Envelope;
    let mut out = type_topics(Direction::Rx, &codec.rx_types(), envelope);
    out.extend(type_topics(Direction::Tx, &codec.tx_types(), envelope));
    out.push(TopicInfo {
        topic: TEXT_TOPIC.into(),
        schema_name: None,
    });
    out.push(TopicInfo {
        topic: MARKER_TOPIC.into(),
        schema_name: None,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Role;
    use crate::project::Project;

    #[test]
    fn demo_topics_and_payloads() {
        let p = Project::open(&crate::schema::tests::demo_dir());
        let s = p.schema.clone().unwrap();
        let host = Codec::new(&p.config, &s, Role::Host).unwrap();
        let dev = Codec::new(&p.config, &s, Role::Device).unwrap();
        let topics: Vec<String> = known_topics(&host).into_iter().map(|t| t.topic).collect();
        for t in [
            "/rx/telemetry",
            "/rx/pong",
            "/tx/set_bias",
            "/text",
            "/markers",
        ] {
            assert!(topics.contains(&t.to_string()), "{t} in {topics:?}");
        }
        let t = dev.resolve_tx("telemetry").unwrap();
        let m = dev.build(&t, serde_json::json!({"vout_raw": 7})).unwrap();
        let d = host.decode(&dev.encode(&m).unwrap(), 0).unwrap();
        let tm = for_message(&d, Direction::Rx);
        assert_eq!(tm.topic, "/rx/telemetry");
        assert_eq!(tm.schema_name.as_deref(), Some("demo.Telemetry"));
        let back = prost_reflect::DynamicMessage::decode(
            s.message("demo.Telemetry").unwrap(),
            tm.data.as_slice(),
        )
        .unwrap();
        assert_eq!(
            back.get_field_by_name("vout_raw").unwrap().as_u32(),
            Some(7)
        );
        assert_eq!(for_text("hi").data, br#"{"text":"hi"}"#.to_vec());
    }
}
