//! Commands (host → device): payload building from TOML/JSON, variable
//! substitution, prompt overrides, and the request model handed to the
//! session.

use std::collections::BTreeMap;

use prost_reflect::{DynamicMessage, Kind, MessageDescriptor};
use serde_json::Value as Json;

use crate::codec::{Codec, MessageTarget};
use crate::config::{CommandConfig, ProjectConfig};
use crate::error::CoreError;

mod history;
pub use history::{CommandHistory, HistoryEntry};

/// A fully resolved request, ready for the session's TX queue.
#[derive(Debug, Clone)]
pub struct SendRequest {
    /// Label for UI, history and latency statistics (command name or type).
    pub name: String,
    /// Top-level message to send. The session fills in the correlation id.
    pub msg: DynamicMessage,
    /// Expected reply; `None` is fire-and-forget.
    pub expect: Option<MessageTarget>,
    /// Response timeout.
    pub timeout_ms: u64,
    /// Retries after a timeout.
    pub retries: u32,
}

/// Converts a TOML value to JSON.
pub fn toml_to_json(v: &toml::Value) -> Json {
    match v {
        toml::Value::String(s) => Json::String(s.clone()),
        toml::Value::Integer(i) => Json::from(*i),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        toml::Value::Boolean(b) => Json::Bool(*b),
        toml::Value::Datetime(d) => Json::String(d.to_string()),
        toml::Value::Array(a) => Json::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => Json::Object(
            t.iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

/// Converts a TOML table to a JSON object.
pub fn table_to_json(t: &toml::Table) -> Json {
    toml_to_json(&toml::Value::Table(t.clone()))
}

/// Adapts TOML-friendly payloads to the Protobuf JSON mapping for `desc`:
/// `bytes` fields accept arrays of integers or `"hex:0A0B"` strings as well
/// as base64. Unknown field names are reported with the type name.
pub fn normalize_json(desc: &MessageDescriptor, v: Json) -> Result<Json, CoreError> {
    let Json::Object(obj) = v else {
        return Ok(v);
    };
    if is_wkt(desc) {
        return Ok(Json::Object(obj));
    }
    let mut out = serde_json::Map::new();
    for (k, val) in obj {
        let field = desc
            .get_field_by_name(&k)
            .or_else(|| desc.get_field_by_json_name(&k))
            .ok_or_else(|| {
                CoreError::Payload(format!(
                    "{} has no field `{k}` (fields: {})",
                    desc.full_name(),
                    desc.fields()
                        .map(|f| f.name().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
        let kind = field.kind();
        let val = if field.is_map() {
            let value_kind = match &kind {
                Kind::Message(entry) => entry.map_entry_value_field().kind(),
                k => k.clone(),
            };
            match val {
                Json::Object(m) => Json::Object(
                    m.into_iter()
                        .map(|(mk, mv)| Ok((mk, normalize_kind(&value_kind, mv)?)))
                        .collect::<Result<_, CoreError>>()?,
                ),
                other => other,
            }
        } else if field.is_list() {
            match val {
                Json::Array(items) if !(matches!(kind, Kind::Bytes) && is_byte_array(&items)) => {
                    Json::Array(
                        items
                            .into_iter()
                            .map(|i| normalize_kind(&kind, i))
                            .collect::<Result<_, _>>()?,
                    )
                }
                other => normalize_kind(&kind, other)?,
            }
        } else {
            normalize_kind(&kind, val)?
        };
        out.insert(k, val);
    }
    Ok(Json::Object(out))
}

fn is_wkt(desc: &MessageDescriptor) -> bool {
    desc.full_name().starts_with("google.protobuf.")
}

fn is_byte_array(items: &[Json]) -> bool {
    !items.is_empty() && items.iter().all(|i| i.as_u64().is_some_and(|n| n <= 255))
}

fn normalize_kind(kind: &Kind, v: Json) -> Result<Json, CoreError> {
    match kind {
        Kind::Bytes => match v {
            Json::Array(items) => {
                let bytes: Option<Vec<u8>> = items
                    .iter()
                    .map(|i| i.as_u64().and_then(|n| u8::try_from(n).ok()))
                    .collect();
                bytes
                    .map(|b| Json::String(crate::util::base64_encode(&b)))
                    .ok_or_else(|| CoreError::Payload("bytes arrays must contain 0..255".into()))
            }
            Json::String(s) if s.starts_with("hex:") => {
                let b = crate::util::parse_hex(&s[4..]).map_err(CoreError::Payload)?;
                Ok(Json::String(crate::util::base64_encode(&b)))
            }
            other => Ok(other),
        },
        Kind::Message(m) => normalize_json(m, v),
        _ => Ok(v),
    }
}

/// Replaces `${var}` in every string. A string that is exactly `${var}`
/// takes the variable's JSON value (so numbers stay numbers).
pub fn substitute_vars(v: Json, vars: &BTreeMap<String, Json>) -> Result<Json, CoreError> {
    Ok(match v {
        Json::String(s) => substitute_str(&s, vars)?,
        Json::Array(a) => Json::Array(
            a.into_iter()
                .map(|x| substitute_vars(x, vars))
                .collect::<Result<_, _>>()?,
        ),
        Json::Object(o) => Json::Object(
            o.into_iter()
                .map(|(k, x)| Ok((k, substitute_vars(x, vars)?)))
                .collect::<Result<_, CoreError>>()?,
        ),
        other => other,
    })
}

fn substitute_str(s: &str, vars: &BTreeMap<String, Json>) -> Result<Json, CoreError> {
    let t = s.trim();
    if t.starts_with("${") && t.ends_with('}') && t.matches("${").count() == 1 {
        let name = &t[2..t.len() - 1];
        return vars
            .get(name)
            .cloned()
            .ok_or_else(|| CoreError::Sequence(format!("undefined variable `{name}`")));
    }
    Ok(Json::String(interpolate(s, vars)?))
}

/// Replaces `${var}` occurrences inside a string with their text form.
pub fn interpolate(s: &str, vars: &BTreeMap<String, Json>) -> Result<String, CoreError> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| CoreError::Sequence(format!("unterminated `${{` in {s:?}")))?;
        let name = &after[..end];
        let v = vars
            .get(name)
            .ok_or_else(|| CoreError::Sequence(format!("undefined variable `{name}`")))?;
        match v {
            Json::String(x) => out.push_str(x),
            other => out.push_str(&other.to_string()),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Sets `path` (dot-separated) inside a JSON object, creating objects.
pub fn set_json_path(root: &mut Json, path: &str, value: Json) -> Result<(), CoreError> {
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err(CoreError::Payload("empty field name".into()));
    }
    let mut cur = root;
    for (i, part) in parts.iter().enumerate() {
        if !cur.is_object() {
            *cur = Json::Object(serde_json::Map::new());
        }
        let Json::Object(obj) = cur else {
            unreachable!("ensured object above")
        };
        if i + 1 == parts.len() {
            obj.insert((*part).to_string(), value);
            return Ok(());
        }
        cur = obj
            .entry((*part).to_string())
            .or_insert_with(|| Json::Object(serde_json::Map::new()));
    }
    Ok(())
}

/// Reads `path` inside a JSON object.
pub fn get_json_path<'a>(root: &'a Json, path: &str) -> Option<&'a Json> {
    path.split('.')
        .filter(|p| !p.is_empty())
        .try_fold(root, |cur, p| cur.get(p))
}

/// Parses a `--set` / prompt value: TOML syntax (`5`, `true`, `"x"`,
/// `[1, 2]`, `{ a = 1 }`), falling back to a plain string.
pub fn parse_value(s: &str) -> Json {
    let doc = format!("v = {s}");
    match doc.parse::<toml::Table>() {
        Ok(t) => t.get("v").map(toml_to_json).unwrap_or(Json::Null),
        Err(_) => Json::String(s.to_string()),
    }
}

/// Builds the request for a configured command, applying overrides
/// (`field.path`, value) such as prompt edits.
pub fn build_command(
    config: &ProjectConfig,
    codec: &Codec,
    cmd: &CommandConfig,
    overrides: &[(String, Json)],
    vars: &BTreeMap<String, Json>,
) -> Result<SendRequest, CoreError> {
    let target = codec.resolve_tx(&cmd.message)?;
    let mut payload = substitute_vars(table_to_json(&cmd.payload), vars)?;
    for (path, v) in overrides {
        set_json_path(&mut payload, path, v.clone())?;
    }
    let msg = codec.build(&target, payload)?;
    let expect = cmd
        .expect
        .as_deref()
        .map(|e| codec.resolve_rx(e))
        .transpose()?;
    Ok(SendRequest {
        name: cmd.name.clone(),
        msg,
        expect,
        timeout_ms: cmd
            .timeout_ms
            .unwrap_or(config.correlation.default_timeout_ms),
        retries: cmd.retries.unwrap_or(config.correlation.default_retries),
    })
}

/// Builds a request for an inline message (`message` + payload JSON).
pub fn build_inline(
    config: &ProjectConfig,
    codec: &Codec,
    message: &str,
    payload: Json,
    expect: Option<&str>,
    timeout_ms: Option<u64>,
    retries: Option<u32>,
) -> Result<SendRequest, CoreError> {
    let target = codec.resolve_tx(message)?;
    let msg = codec.build(&target, payload)?;
    let expect = expect.map(|e| codec.resolve_rx(e)).transpose()?;
    Ok(SendRequest {
        name: target.name(),
        msg,
        expect,
        timeout_ms: timeout_ms.unwrap_or(config.correlation.default_timeout_ms),
        retries: retries.unwrap_or(config.correlation.default_retries),
    })
}

/// Groups commands by `group`, keeping first-seen group order.
pub fn group_commands(cmds: &[CommandConfig]) -> Vec<(String, Vec<&CommandConfig>)> {
    let mut out: Vec<(String, Vec<&CommandConfig>)> = Vec::new();
    for c in cmds {
        match out.iter_mut().find(|(g, _)| *g == c.group) {
            Some((_, v)) => v.push(c),
            None => out.push((c.group.clone(), vec![c])),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Role;
    use crate::config::load_config_str;
    use crate::schema::tests::{demo_dir, demo_schema};
    use crate::schema::Schema;
    use prost_reflect::{ReflectMessage, Value};
    use std::path::Path;

    fn demo() -> (ProjectConfig, Codec) {
        let text = std::fs::read_to_string(demo_dir().join("project.toml")).unwrap();
        let cfg = load_config_str(&text, Path::new("p.toml")).config.unwrap();
        let c = Codec::new(&cfg, &demo_schema(), Role::Host).unwrap();
        (cfg, c)
    }

    fn rich_schema() -> Schema {
        let dir = std::env::temp_dir().join(format!("pb-cmd-rich-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("r.proto"),
            r#"syntax = "proto3";
package r;
enum Mode { OFF = 0; ON = 1; AUTO = 2; }
message Inner { int32 x = 1; string label = 2; }
message Cfg {
  Mode mode = 1;
  Inner inner = 2;
  repeated uint32 values = 3;
  bytes key = 4;
  repeated Inner list = 5;
  map<string, int32> gains = 6;
  double ratio = 7;
}
"#,
        )
        .unwrap();
        Schema::compile(&dir, &["r.proto".into()], &[]).unwrap()
    }

    #[test]
    fn payload_from_toml_nested_enum_repeated_bytes() {
        let s = rich_schema();
        let d = s.message("r.Cfg").unwrap();
        let t: toml::Table = r#"
mode = "AUTO"
inner = { x = -3, label = "hi" }
values = [1, 2, 3]
key = [0xDE, 0xAD]
list = [{ x = 1 }, { x = 2 }]
gains = { a = 5 }
ratio = 2
"#
        .parse()
        .unwrap();
        let json = normalize_json(&d, table_to_json(&t)).unwrap();
        let m = DynamicMessage::deserialize(d.clone(), json).unwrap();
        assert_eq!(
            m.get_field_by_name("mode").unwrap().as_enum_number(),
            Some(2)
        );
        let inner = m.get_field_by_name("inner").unwrap();
        let inner = inner.as_message().unwrap();
        assert_eq!(inner.get_field_by_name("x").unwrap().as_i32(), Some(-3));
        let vals = m.get_field_by_name("values").unwrap();
        assert_eq!(vals.as_list().unwrap().len(), 3);
        assert_eq!(
            m.get_field_by_name("key")
                .unwrap()
                .as_bytes()
                .unwrap()
                .as_ref(),
            &[0xDE, 0xAD]
        );
        assert_eq!(
            m.get_field_by_name("list")
                .unwrap()
                .as_list()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(m.get_field_by_name("ratio").unwrap().as_f64(), Some(2.0));
        let gains = m.get_field_by_name("gains").unwrap();
        assert_eq!(gains.as_map().unwrap().len(), 1);

        // hex: strings for bytes
        let j = serde_json::json!({"key": "hex:01 02"});
        let m = DynamicMessage::deserialize(d.clone(), normalize_json(&d, j).unwrap()).unwrap();
        assert_eq!(
            m.get_field_by_name("key")
                .unwrap()
                .as_bytes()
                .unwrap()
                .as_ref(),
            &[1, 2]
        );

        // unknown field is a clear error
        let e = normalize_json(&d, serde_json::json!({"nope": 1})).unwrap_err();
        assert!(e.to_string().contains("r.Cfg has no field `nope`"), "{e}");
    }

    #[test]
    fn build_command_with_prompt_override_and_vars() {
        let (cfg, codec) = demo();
        let cmd = cfg.command("Set Bias").unwrap();
        let r = build_command(
            &cfg,
            &codec,
            cmd,
            &[("millivolts".into(), Json::from(350))],
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(r.timeout_ms, 1000);
        assert_eq!(r.expect.as_ref().unwrap().name(), "ack");
        let sb = r.msg.get_field_by_name("set_bias").unwrap();
        assert_eq!(
            sb.as_message()
                .unwrap()
                .get_field_by_name("millivolts")
                .unwrap()
                .as_i32(),
            Some(350)
        );
        let mut vars = BTreeMap::new();
        vars.insert("bias_mv".to_string(), Json::from(123));
        let p = substitute_vars(
            serde_json::json!({"millivolts": "${bias_mv}", "s": "v=${bias_mv}"}),
            &vars,
        )
        .unwrap();
        assert_eq!(p["millivolts"], 123);
        assert_eq!(p["s"], "v=123");
        assert!(substitute_vars(serde_json::json!("${nope}"), &vars).is_err());
        let r = build_inline(
            &cfg,
            &codec,
            "stream_ctrl",
            serde_json::json!({"enable": true}),
            Some("ack"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(r.msg.descriptor().full_name(), "demo.HostToDevice");
        assert!(matches!(
            r.msg.get_field_by_name("stream_ctrl").as_deref(),
            Some(Value::Message(_))
        ));
    }

    #[test]
    fn parse_values_and_paths() {
        assert_eq!(parse_value("5"), Json::from(5));
        assert_eq!(parse_value("true"), Json::Bool(true));
        assert_eq!(
            parse_value("hello world"),
            Json::String("hello world".into())
        );
        assert_eq!(parse_value("[1,2]"), serde_json::json!([1, 2]));
        let mut j = serde_json::json!({});
        set_json_path(&mut j, "a.b", Json::from(1)).unwrap();
        assert_eq!(get_json_path(&j, "a.b"), Some(&Json::from(1)));
    }

    #[test]
    fn grouping_keeps_order() {
        let (cfg, _) = demo();
        let g = group_commands(&cfg.commands);
        let names: Vec<_> = g.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["System", "AFE", "Stream", "Danger"]);
        assert_eq!(g[0].1.len(), 2);
    }
}
