//! Message dispatch and encoding: envelope (`oneof`), message-ID prefix, or
//! one fixed type per direction.
//!
//! A [`Codec`] is built for one side of the link ([`Role::Host`] or
//! [`Role::Device`]). The simulator uses the device role, which swaps the
//! transmit and receive types.

use std::sync::Arc;

use prost::Message as _;
use prost_reflect::{
    DynamicMessage, FieldDescriptor, Kind, MessageDescriptor, ReflectMessage, Value,
};

use crate::config::{parse_id_key, CodecMode, IdWidth, ProjectConfig};
use crate::error::CoreError;
use crate::schema::{type_name_matches, wire::read_varint, FieldPath, Schema};

/// Which side of the link a codec serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The host (ProtoBench talking to a device).
    Host,
    /// The device (the simulator).
    Device,
}

/// A resolved message name: what to build or match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageTarget {
    /// The top-level type on the wire (e.g. `demo.HostToDevice`).
    pub root: MessageDescriptor,
    /// In envelope mode, the selected field of the root (usually a oneof member).
    pub member: Option<FieldDescriptor>,
}

impl MessageTarget {
    /// Type of the payload: the member's message type or the root.
    pub fn inner(&self) -> MessageDescriptor {
        match self.member.as_ref().map(|f| f.kind()) {
            Some(Kind::Message(m)) => m,
            _ => self.root.clone(),
        }
    }

    /// Short name: member name in envelope mode, else the full type name.
    pub fn name(&self) -> String {
        match &self.member {
            Some(m) => m.name().to_string(),
            None => self.root.full_name().to_string(),
        }
    }
}

/// A decoded message with its metadata.
#[derive(Debug, Clone)]
pub struct DecodedMessage {
    /// Host receive (or send) time in ns, from the session clock.
    pub host_ts_ns: u64,
    /// Device timestamp in seconds, when configured and present.
    pub device_ts: Option<f64>,
    /// Full name of the decoded top-level type.
    pub type_name: String,
    /// In envelope mode, the oneof member that is set.
    pub member: Option<String>,
    /// The decoded message (top-level type).
    pub msg: DynamicMessage,
    /// Codec input bytes (ID prefix included, CRC and framing excluded).
    pub raw: Vec<u8>,
    /// Length of the ID prefix inside `raw`.
    pub payload_offset: usize,
}

impl DecodedMessage {
    /// Short display name: the oneof member, else the simple type name.
    pub fn kind(&self) -> &str {
        match &self.member {
            Some(m) => m,
            None => self.type_name.rsplit('.').next().unwrap_or(&self.type_name),
        }
    }

    /// Protobuf JSON of the message.
    pub fn to_json(&self) -> serde_json::Value {
        message_to_json(&self.msg)
    }
}

/// Converts a message to Protobuf JSON (enum names, default fields omitted).
pub fn message_to_json(msg: &DynamicMessage) -> serde_json::Value {
    let opts = prost_reflect::SerializeOptions::new().skip_default_fields(false);
    msg.serialize_with_options(serde_json::value::Serializer, &opts)
        .unwrap_or(serde_json::Value::Null)
}

/// Why a frame could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The ID prefix does not map to a type.
    #[error("unknown message id {0}")]
    UnknownId(u32),
    /// The bytes are not a valid message of the expected type.
    #[error("cannot decode {type_name}: {reason}")]
    Invalid {
        /// Expected type.
        type_name: String,
        /// Decoder message.
        reason: String,
    },
    /// The frame is empty or too short for the ID prefix.
    #[error("frame too short")]
    TooShort,
}

/// Encoder/decoder for one side of the link.
#[derive(Debug, Clone)]
pub struct Codec {
    mode: CodecMode,
    tx_root: Option<MessageDescriptor>,
    rx_root: Option<MessageDescriptor>,
    ids: Vec<(u32, MessageDescriptor)>,
    id_width: IdWidth,
}

/// Shared codec handle.
pub type SharedCodec = Arc<Codec>;

impl Codec {
    /// Builds a codec from the project config.
    pub fn new(config: &ProjectConfig, schema: &Schema, role: Role) -> Result<Codec, CoreError> {
        let c = &config.codec;
        let lookup =
            |name: &Option<String>, key: &str| -> Result<Option<MessageDescriptor>, CoreError> {
                match name {
                    None => Ok(None),
                    Some(n) => schema.message(n).map(Some).ok_or_else(|| {
                        CoreError::Codec(format!(
                            "codec.{key}: message type `{n}` not found in schema"
                        ))
                    }),
                }
            };
        let mut tx_root = lookup(&c.tx_root, "tx_root")?;
        let mut rx_root = lookup(&c.rx_root, "rx_root")?;
        let mut ids = Vec::new();
        if c.mode == CodecMode::IdPrefix {
            for (k, v) in &c.ids {
                let id = parse_id_key(k)
                    .ok_or_else(|| CoreError::Codec(format!("codec.ids: invalid id `{k}`")))?;
                let d = schema.message(v).ok_or_else(|| {
                    CoreError::Codec(format!(
                        "codec.ids.{k}: message type `{v}` not found in schema"
                    ))
                })?;
                ids.push((id, d));
            }
        } else {
            if tx_root.is_none() || rx_root.is_none() {
                return Err(CoreError::Codec(
                    "codec needs tx_root and rx_root in envelope and fixed modes".into(),
                ));
            }
        }
        if role == Role::Device {
            std::mem::swap(&mut tx_root, &mut rx_root);
        }
        Ok(Codec {
            mode: c.mode,
            tx_root,
            rx_root,
            ids,
            id_width: c.id_width,
        })
    }

    /// Dispatch mode.
    pub fn mode(&self) -> CodecMode {
        self.mode
    }

    /// Root type this side sends (envelope and fixed).
    pub fn tx_root(&self) -> Option<&MessageDescriptor> {
        self.tx_root.as_ref()
    }

    /// Root type this side receives (envelope and fixed).
    pub fn rx_root(&self) -> Option<&MessageDescriptor> {
        self.rx_root.as_ref()
    }

    /// Top-level types this side can send.
    pub fn tx_types(&self) -> Vec<MessageDescriptor> {
        match self.mode {
            CodecMode::IdPrefix => self.ids.iter().map(|(_, d)| d.clone()).collect(),
            _ => self.tx_root.iter().cloned().collect(),
        }
    }

    /// Top-level types this side can receive.
    pub fn rx_types(&self) -> Vec<MessageDescriptor> {
        match self.mode {
            CodecMode::IdPrefix => self.ids.iter().map(|(_, d)| d.clone()).collect(),
            _ => self.rx_root.iter().cloned().collect(),
        }
    }

    /// Resolves a name used for sending (command `message`, simulator `on`).
    pub fn resolve_tx(&self, name: &str) -> Result<MessageTarget, CoreError> {
        self.resolve(name, self.tx_root.as_ref(), "sent")
    }

    /// Resolves a name used for receiving (`expect`, telemetry `message`).
    pub fn resolve_rx(&self, name: &str) -> Result<MessageTarget, CoreError> {
        self.resolve(name, self.rx_root.as_ref(), "received")
    }

    fn resolve(
        &self,
        name: &str,
        root: Option<&MessageDescriptor>,
        dir: &str,
    ) -> Result<MessageTarget, CoreError> {
        let name = name.trim().trim_start_matches('.');
        match self.mode {
            CodecMode::Envelope => {
                let root = root.ok_or_else(|| CoreError::Codec("no root type".into()))?;
                if name == root.full_name() || name == root.name() {
                    return Ok(MessageTarget {
                        root: root.clone(),
                        member: None,
                    });
                }
                if let Some(f) = root.get_field_by_name(name) {
                    return Ok(MessageTarget {
                        root: root.clone(),
                        member: Some(f),
                    });
                }
                let by_type: Vec<FieldDescriptor> = root
                    .fields()
                    .filter(|f| f.containing_oneof().is_some())
                    .filter(|f| matches!(f.kind(), Kind::Message(m) if m.full_name() == name))
                    .collect();
                match by_type.len() {
                    1 => Ok(MessageTarget {
                        root: root.clone(),
                        member: by_type.into_iter().next(),
                    }),
                    0 => Err(CoreError::Codec(format!(
                        "`{name}` is not a field of {} (messages {dir}: {})",
                        root.full_name(),
                        root.fields()
                            .filter(|f| f.containing_oneof().is_some())
                            .map(|f| f.name().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))),
                    _ => Err(CoreError::Codec(format!(
                        "type `{name}` is used by several fields of {}; use the field name",
                        root.full_name()
                    ))),
                }
            }
            CodecMode::Fixed => {
                let root = root.ok_or_else(|| CoreError::Codec("no root type".into()))?;
                if name == root.full_name() || type_name_matches(root, name) {
                    Ok(MessageTarget {
                        root: root.clone(),
                        member: None,
                    })
                } else {
                    Err(CoreError::Codec(format!(
                        "fixed mode: `{name}` is not the {dir} type {}",
                        root.full_name()
                    )))
                }
            }
            CodecMode::IdPrefix => {
                let found = self
                    .ids
                    .iter()
                    .find(|(_, d)| d.full_name() == name)
                    .or_else(|| self.ids.iter().find(|(_, d)| type_name_matches(d, name)));
                match found {
                    Some((_, d)) => Ok(MessageTarget {
                        root: d.clone(),
                        member: None,
                    }),
                    None => Err(CoreError::Codec(format!(
                        "`{name}` is not listed in [codec.ids]"
                    ))),
                }
            }
        }
    }

    /// Builds a top-level message for `target` from a Protobuf-JSON payload
    /// of the inner type.
    pub fn build(
        &self,
        target: &MessageTarget,
        payload: serde_json::Value,
    ) -> Result<DynamicMessage, CoreError> {
        let inner = target.inner();
        let payload = crate::commands::normalize_json(&inner, payload)?;
        let json = match &target.member {
            Some(m) => {
                let mut obj = serde_json::Map::new();
                obj.insert(m.json_name().to_string(), payload);
                serde_json::Value::Object(obj)
            }
            None => payload,
        };
        DynamicMessage::deserialize(target.root.clone(), json)
            .map_err(|e| CoreError::Payload(format!("{}: {e}", target.name())))
    }

    /// Encodes a top-level message, adding the ID prefix in id_prefix mode.
    pub fn encode(&self, msg: &DynamicMessage) -> Result<Vec<u8>, CoreError> {
        let body = msg.encode_to_vec();
        match self.mode {
            CodecMode::IdPrefix => {
                let full = msg.descriptor();
                let id = self
                    .ids
                    .iter()
                    .find(|(_, d)| d == &full)
                    .map(|(id, _)| *id)
                    .ok_or_else(|| CoreError::Codec(format!("no id for {}", full.full_name())))?;
                let mut out = Vec::with_capacity(body.len() + 5);
                match self.id_width {
                    IdWidth::U8 => out.push(id as u8),
                    IdWidth::U16 => out.extend_from_slice(&(id as u16).to_le_bytes()),
                    IdWidth::Varint => write_varint(&mut out, u64::from(id)),
                }
                out.extend_from_slice(&body);
                Ok(out)
            }
            _ => Ok(body),
        }
    }

    /// Decodes codec input bytes.
    pub fn decode(&self, bytes: &[u8], host_ts_ns: u64) -> Result<DecodedMessage, DecodeError> {
        let (desc, offset) = match self.mode {
            CodecMode::IdPrefix => {
                let (id, n) = match self.id_width {
                    IdWidth::U8 => (u32::from(*bytes.first().ok_or(DecodeError::TooShort)?), 1),
                    IdWidth::U16 => {
                        if bytes.len() < 2 {
                            return Err(DecodeError::TooShort);
                        }
                        (u32::from(u16::from_le_bytes([bytes[0], bytes[1]])), 2)
                    }
                    IdWidth::Varint => {
                        let (v, n) = read_varint(bytes, 0).ok_or(DecodeError::TooShort)?;
                        (
                            u32::try_from(v).map_err(|_| DecodeError::UnknownId(u32::MAX))?,
                            n,
                        )
                    }
                };
                let d = self
                    .ids
                    .iter()
                    .find(|(i, _)| *i == id)
                    .map(|(_, d)| d.clone())
                    .ok_or(DecodeError::UnknownId(id))?;
                (d, n)
            }
            _ => (
                self.rx_root.clone().ok_or(DecodeError::Invalid {
                    type_name: "?".into(),
                    reason: "no rx type".into(),
                })?,
                0,
            ),
        };
        let msg = DynamicMessage::decode(desc.clone(), &bytes[offset..]).map_err(|e| {
            DecodeError::Invalid {
                type_name: desc.full_name().to_string(),
                reason: e.to_string(),
            }
        })?;
        let member = if self.mode == CodecMode::Envelope {
            set_oneof_member(&msg)
        } else {
            None
        };
        Ok(DecodedMessage {
            host_ts_ns,
            device_ts: None,
            type_name: desc.full_name().to_string(),
            member,
            msg,
            raw: bytes.to_vec(),
            payload_offset: offset,
        })
    }

    /// True if a decoded message is the target (same oneof member, or same
    /// type outside envelope mode).
    pub fn matches(&self, decoded: &DecodedMessage, target: &MessageTarget) -> bool {
        match &target.member {
            Some(m) => decoded.member.as_deref() == Some(m.name()),
            None => decoded.type_name == target.root.full_name(),
        }
    }
}

/// The name of the set member of the first non-synthetic oneof, if any.
pub fn set_oneof_member(msg: &DynamicMessage) -> Option<String> {
    let d = msg.descriptor();
    for oneof in d.oneofs().filter(|o| !o.is_synthetic()) {
        for f in oneof.fields() {
            if msg.has_field(&f) {
                return Some(f.name().to_string());
            }
        }
    }
    None
}

/// Appends a protobuf varint.
pub fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Sets an integer field at a path (creating intermediate messages). Returns
/// false if the path does not exist or is not an integer field.
pub fn set_int_at_path(msg: &mut DynamicMessage, path: &FieldPath, value: u64) -> bool {
    let skip = path.skip_for(&msg.descriptor());
    let segs = &path.segs()[skip..];
    set_rec(msg, segs, value)
}

fn set_rec(msg: &mut DynamicMessage, segs: &[crate::schema::PathSeg], value: u64) -> bool {
    let Some((seg, rest)) = segs.split_first() else {
        return false;
    };
    let Some(field) = msg.descriptor().get_field_by_name(&seg.name) else {
        return false;
    };
    if rest.is_empty() {
        let v = match field.kind() {
            Kind::Uint32 | Kind::Fixed32 => Value::U32(value as u32),
            Kind::Uint64 | Kind::Fixed64 => Value::U64(value),
            Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => Value::I32(value as i32),
            Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => Value::I64(value as i64),
            _ => return false,
        };
        if field.is_list() {
            return false;
        }
        msg.set_field(&field, v);
        return true;
    }
    if !matches!(field.kind(), Kind::Message(_)) || field.is_list() || field.is_map() {
        return false;
    }
    match msg.get_field_mut(&field) {
        Value::Message(m) => set_rec(m, rest, value),
        _ => false,
    }
}

/// Reads an integer field at a path.
pub fn get_int_at_path(msg: &DynamicMessage, path: &FieldPath) -> Option<u64> {
    crate::schema::extract(msg, path)
        .into_iter()
        .next()
        .and_then(|e| match e.value {
            Value::U32(v) => Some(u64::from(v)),
            Value::U64(v) => Some(v),
            Value::I32(v) => u64::try_from(v).ok(),
            Value::I64(v) => u64::try_from(v).ok(),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_config_str, ProjectConfig};
    use crate::schema::tests::{demo_dir, demo_schema};
    use std::path::Path;

    fn demo_cfg() -> ProjectConfig {
        let text = std::fs::read_to_string(demo_dir().join("project.toml")).unwrap();
        load_config_str(&text, Path::new("p.toml")).config.unwrap()
    }

    #[test]
    fn envelope_round_trip_both_roles() {
        let s = demo_schema();
        let cfg = demo_cfg();
        let host = Codec::new(&cfg, &s, Role::Host).unwrap();
        let dev = Codec::new(&cfg, &s, Role::Device).unwrap();
        let t = host.resolve_tx("set_bias").unwrap();
        assert_eq!(t.inner().full_name(), "demo.SetBias");
        let m = host
            .build(&t, serde_json::json!({"millivolts": -200}))
            .unwrap();
        let bytes = host.encode(&m).unwrap();
        let d = dev.decode(&bytes, 7).unwrap();
        assert_eq!(d.type_name, "demo.HostToDevice");
        assert_eq!(d.member.as_deref(), Some("set_bias"));
        assert_eq!(d.kind(), "set_bias");
        assert_eq!(d.host_ts_ns, 7);
        assert!(dev.matches(&d, &dev.resolve_rx("set_bias").unwrap()));
        assert!(!dev.matches(&d, &dev.resolve_rx("ping").unwrap()));
        assert_eq!(d.to_json()["setBias"]["millivolts"], -200);
        // Full type name wraps automatically.
        let t2 = host.resolve_tx("demo.SetBias").unwrap();
        assert_eq!(t2, t);
        assert!(host.resolve_tx("telemetry").is_err());
        assert!(host.resolve_rx("telemetry").is_ok());
    }

    #[test]
    fn id_prefix_round_trip_and_unknown_id() {
        let s = demo_schema();
        for width in ["1", "2", "\"varint\""] {
            let src = format!(
                "config_version = 1\n[codec]\nmode = \"id_prefix\"\nid_width = {width}\n[codec.ids]\n0x01 = \"demo.Telemetry\"\n0x02 = \"demo.Ack\"\n300 = \"demo.Ping\"\n"
            );
            let src = if width == "1" {
                src.replace("300 = \"demo.Ping\"\n", "")
            } else {
                src
            };
            let cfg = load_config_str(&src, Path::new("p.toml")).config.unwrap();
            let c = Codec::new(&cfg, &s, Role::Host).unwrap();
            let t = c.resolve_tx("ack").unwrap();
            assert_eq!(t.root.full_name(), "demo.Ack");
            let m = c.build(&t, serde_json::json!({"ok": true})).unwrap();
            let bytes = c.encode(&m).unwrap();
            let d = c.decode(&bytes, 0).unwrap();
            assert_eq!(d.type_name, "demo.Ack");
            assert_eq!(d.kind(), "Ack");
            assert!(c.matches(&d, &t));
            assert_eq!(
                c.decode(&[0x7F, 0x00], 0).unwrap_err(),
                DecodeError::UnknownId(0x7F)
            );
            assert!(c.decode(&[], 0).is_err());
            if width != "1" {
                let p = c.resolve_tx("demo.Ping").unwrap();
                let bytes = c
                    .encode(&c.build(&p, serde_json::json!({})).unwrap())
                    .unwrap();
                assert_eq!(c.decode(&bytes, 0).unwrap().type_name, "demo.Ping");
            }
        }
    }

    #[test]
    fn fixed_mode_round_trip() {
        let s = demo_schema();
        let src = "config_version = 1\n[codec]\nmode = \"fixed\"\ntx_root = \"demo.SetBias\"\nrx_root = \"demo.Telemetry\"\n";
        let cfg = load_config_str(src, Path::new("p.toml")).config.unwrap();
        let host = Codec::new(&cfg, &s, Role::Host).unwrap();
        let dev = Codec::new(&cfg, &s, Role::Device).unwrap();
        let t = host.resolve_tx("set_bias").unwrap();
        let bytes = host
            .encode(
                &host
                    .build(&t, serde_json::json!({"millivolts": 5}))
                    .unwrap(),
            )
            .unwrap();
        let d = dev.decode(&bytes, 0).unwrap();
        assert_eq!(d.type_name, "demo.SetBias");
        let t = dev.resolve_tx("demo.Telemetry").unwrap();
        let bytes = dev
            .encode(&dev.build(&t, serde_json::json!({"vout_raw": 9})).unwrap())
            .unwrap();
        let d = host.decode(&bytes, 0).unwrap();
        assert_eq!(d.type_name, "demo.Telemetry");
        assert!(host.resolve_tx("ping").is_err());
    }

    #[test]
    fn invalid_bytes_do_not_panic() {
        let s = demo_schema();
        let c = Codec::new(&demo_cfg(), &s, Role::Host).unwrap();
        let e = c.decode(&[0x0A, 0xFF, 0xFF, 0xFF], 0).unwrap_err();
        assert!(matches!(e, DecodeError::Invalid { .. }));
    }

    #[test]
    fn seq_get_set() {
        let s = demo_schema();
        let c = Codec::new(&demo_cfg(), &s, Role::Host).unwrap();
        let t = c.resolve_tx("ping").unwrap();
        let mut m = c.build(&t, serde_json::json!({})).unwrap();
        let p = FieldPath::parse("seq").unwrap();
        assert!(set_int_at_path(&mut m, &p, 42));
        assert_eq!(get_int_at_path(&m, &p), Some(42));
        assert!(!set_int_at_path(
            &mut m,
            &FieldPath::parse("nope").unwrap(),
            1
        ));
    }
}
