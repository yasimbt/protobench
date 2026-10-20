//! Field paths such as `telemetry.adc[2]`, `telemetry.adc[*]` or `cal["gain"]`,
//! and extraction of values from a `DynamicMessage`.

use std::fmt;

use prost_reflect::{
    DynamicMessage, FieldDescriptor, Kind, MapKey, MessageDescriptor, ReflectMessage, Value,
};

/// Index part of a path segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathIndex {
    /// No index. On a repeated or map field this selects every element.
    None,
    /// `[n]`
    At(usize),
    /// `[*]`
    All,
    /// `["key"]` or `[key]` for maps.
    Key(String),
}

/// One dot-separated segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSeg {
    /// Field name.
    pub name: String,
    /// Optional index.
    pub index: PathIndex,
}

/// A parsed field path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath {
    segs: Vec<PathSeg>,
    text: String,
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl FieldPath {
    /// Parses a path. Errors describe what is wrong.
    pub fn parse(s: &str) -> Result<FieldPath, String> {
        let text = s.trim();
        if text.is_empty() {
            return Err("empty field path".into());
        }
        let mut segs = Vec::new();
        for part in split_dots(text) {
            let part = part.trim();
            let (name, index) = match part.find('[') {
                None => (part, PathIndex::None),
                Some(open) => {
                    if !part.ends_with(']') {
                        return Err(format!("field path `{text}`: missing `]` in `{part}`"));
                    }
                    let inner = part[open + 1..part.len() - 1].trim();
                    let index = if inner == "*" {
                        PathIndex::All
                    } else if let Ok(n) = inner.parse::<usize>() {
                        PathIndex::At(n)
                    } else if inner.is_empty() {
                        return Err(format!("field path `{text}`: empty index"));
                    } else {
                        PathIndex::Key(inner.trim_matches('"').trim_matches('\'').to_string())
                    };
                    (&part[..open], index)
                }
            };
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(format!("field path `{text}`: invalid field name `{name}`"));
            }
            segs.push(PathSeg {
                name: name.to_string(),
                index,
            });
        }
        Ok(FieldPath {
            segs,
            text: text.to_string(),
        })
    }

    /// The segments.
    pub fn segs(&self) -> &[PathSeg] {
        &self.segs
    }

    /// The path as written.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// True if the path contains a wildcard (or an index-less repeated field
    /// would expand; that is only known with a descriptor).
    pub fn has_wildcard(&self) -> bool {
        self.segs.iter().any(|s| s.index == PathIndex::All)
    }

    /// Number of leading segments to skip when resolving against `desc`.
    ///
    /// Paths normally start at the root message's fields. In `id_prefix` and
    /// `fixed` modes a path may also start with the message's own name
    /// (`telemetry.vout_raw` against `demo.Telemetry`); that first segment is
    /// skipped when it is not a field but matches the type name.
    pub fn skip_for(&self, desc: &MessageDescriptor) -> usize {
        match self.segs.first() {
            Some(first)
                if desc.get_field_by_name(&first.name).is_none()
                    && first.index == PathIndex::None
                    && type_name_matches(desc, &first.name) =>
            {
                1
            }
            _ => 0,
        }
    }
}

/// True if `name` refers to the message type: its simple name or its simple
/// name in snake_case, compared case-insensitively.
pub fn type_name_matches(desc: &MessageDescriptor, name: &str) -> bool {
    let simple = desc.name();
    simple.eq_ignore_ascii_case(name) || to_snake(simple) == name.to_ascii_lowercase()
}

/// `SetBias` → `set_bias`.
pub fn to_snake(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn split_dots(s: &str) -> Vec<&str> {
    // Split on '.' outside brackets.
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            '.' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// A value extracted by a path.
#[derive(Debug, Clone)]
pub struct Extracted {
    /// Concrete channel name, e.g. `telemetry.adc[0]` for `telemetry.adc[*]`.
    pub name: String,
    /// The value.
    pub value: Value,
    /// The leaf field.
    pub field: FieldDescriptor,
}

/// Extracts all values selected by `path` from `msg`. Absent oneof members
/// and unset message fields select nothing; proto3 scalars select their
/// default value.
pub fn extract(msg: &DynamicMessage, path: &FieldPath) -> Vec<Extracted> {
    let skip = path.skip_for(&msg.descriptor());
    let mut out = Vec::new();
    let prefix = path.segs[..skip]
        .iter()
        .map(|s| s.name.clone())
        .collect::<Vec<_>>()
        .join(".");
    walk(msg, &path.segs[skip..], prefix, &mut out);
    out
}

/// Extracts numeric values as `(channel name, f64)`.
pub fn extract_values(msg: &DynamicMessage, path: &FieldPath) -> Vec<(String, f64)> {
    extract(msg, path)
        .into_iter()
        .filter_map(|e| value_to_f64(&e.value).map(|v| (e.name, v)))
        .collect()
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

fn walk(msg: &DynamicMessage, segs: &[PathSeg], prefix: String, out: &mut Vec<Extracted>) {
    let Some((seg, rest)) = segs.split_first() else {
        return;
    };
    let desc = msg.descriptor();
    let Some(field) = desc.get_field_by_name(&seg.name) else {
        return;
    };
    if (field.containing_oneof().is_some() || field.supports_presence()) && !msg.has_field(&field) {
        return;
    }
    let value = msg.get_field(&field);
    let base = join(&prefix, &seg.name);

    // Collect (name, value) candidates after applying the index.
    let mut picked: Vec<(String, Value)> = Vec::new();
    match value.as_ref() {
        Value::List(items) => match &seg.index {
            PathIndex::At(i) => {
                if let Some(v) = items.get(*i) {
                    picked.push((format!("{base}[{i}]"), v.clone()));
                }
            }
            PathIndex::All | PathIndex::None => {
                for (i, v) in items.iter().enumerate() {
                    picked.push((format!("{base}[{i}]"), v.clone()));
                }
            }
            PathIndex::Key(_) => {}
        },
        Value::Map(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by_key(|(k, _)| map_key_string(k));
            match &seg.index {
                PathIndex::Key(k) => {
                    if let Some((mk, v)) = entries.iter().find(|(mk, _)| map_key_string(mk) == *k) {
                        picked.push((format!("{base}[{}]", map_key_string(mk)), (*v).clone()));
                    }
                }
                PathIndex::At(n) => {
                    if let Some((mk, v)) = entries
                        .iter()
                        .find(|(mk, _)| map_key_string(mk) == n.to_string())
                    {
                        picked.push((format!("{base}[{}]", map_key_string(mk)), (*v).clone()));
                    }
                }
                PathIndex::All | PathIndex::None => {
                    for (mk, v) in entries {
                        picked.push((format!("{base}[{}]", map_key_string(mk)), v.clone()));
                    }
                }
            }
        }
        v => {
            if seg.index == PathIndex::None {
                picked.push((base, v.clone()));
            }
        }
    }

    for (name, v) in picked {
        if rest.is_empty() {
            out.push(Extracted {
                name,
                value: v,
                field: field.clone(),
            });
        } else if let Value::Message(m) = &v {
            walk(m, rest, name, out);
        }
    }
}

fn map_key_string(k: &MapKey) -> String {
    match k {
        MapKey::Bool(b) => b.to_string(),
        MapKey::I32(v) => v.to_string(),
        MapKey::I64(v) => v.to_string(),
        MapKey::U32(v) => v.to_string(),
        MapKey::U64(v) => v.to_string(),
        MapKey::String(s) => s.clone(),
    }
}

/// Converts a scalar value to `f64`. Bools are 0/1, enums their number.
pub fn value_to_f64(v: &Value) -> Option<f64> {
    Some(match v {
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::I32(x) => f64::from(*x),
        Value::I64(x) => *x as f64,
        Value::U32(x) => f64::from(*x),
        Value::U64(x) => *x as f64,
        Value::F32(x) => f64::from(*x),
        Value::F64(x) => *x,
        Value::EnumNumber(x) => f64::from(*x),
        _ => return None,
    })
}

/// Resolves a path against a descriptor and returns the leaf field. Used to
/// validate configuration before any message arrives.
pub fn resolve_field(
    desc: &MessageDescriptor,
    path: &FieldPath,
) -> Result<FieldDescriptor, String> {
    let skip = path.skip_for(desc);
    let mut cur = desc.clone();
    let segs = &path.segs()[skip..];
    if segs.is_empty() {
        return Err(format!("`{path}` names the message, not a field"));
    }
    for (i, seg) in segs.iter().enumerate() {
        let field = cur.get_field_by_name(&seg.name).ok_or_else(|| {
            format!(
                "`{path}`: `{}` has no field `{}`",
                cur.full_name(),
                seg.name
            )
        })?;
        if seg.index != PathIndex::None && !(field.is_list() || field.is_map()) {
            return Err(format!("`{path}`: `{}` is not repeated", seg.name));
        }
        let last = i + 1 == segs.len();
        if last {
            return Ok(field);
        }
        let kind = if field.is_map() {
            match field.kind() {
                Kind::Message(entry) => entry.map_entry_value_field().kind(),
                k => k,
            }
        } else {
            field.kind()
        };
        match kind {
            Kind::Message(m) => cur = m,
            _ => {
                return Err(format!(
                    "`{path}`: `{}` is a scalar and has no sub-fields",
                    seg.name
                ))
            }
        }
    }
    Err(format!("`{path}` could not be resolved"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::tests::demo_schema;

    fn telemetry_msg() -> DynamicMessage {
        let s = demo_schema();
        let d = s.message("demo.DeviceToHost").unwrap();
        let json = serde_json::json!({
            "seq": 0,
            "telemetry": { "vout_raw": 1234, "current_na": 2.5, "adc": [10, 20, 30, 40], "state": "MEASURING" }
        });
        DynamicMessage::deserialize(d, json).unwrap()
    }

    #[test]
    fn parse_paths() {
        let p = FieldPath::parse("telemetry.adc[2]").unwrap();
        assert_eq!(p.segs().len(), 2);
        assert_eq!(p.segs()[1].index, PathIndex::At(2));
        assert_eq!(
            FieldPath::parse("a.b[*]").unwrap().segs()[1].index,
            PathIndex::All
        );
        assert_eq!(
            FieldPath::parse("m[\"x.y\"]").unwrap().segs()[0].index,
            PathIndex::Key("x.y".into())
        );
        assert!(FieldPath::parse("").is_err());
        assert!(FieldPath::parse("a.[1]").is_err());
        assert!(FieldPath::parse("a[1").is_err());
        assert!(FieldPath::parse("a-b").is_err());
    }

    #[test]
    fn extract_scalar_index_wildcard_enum() {
        let m = telemetry_msg();
        let v = extract_values(&m, &FieldPath::parse("telemetry.vout_raw").unwrap());
        assert_eq!(v, vec![("telemetry.vout_raw".into(), 1234.0)]);
        let v = extract_values(&m, &FieldPath::parse("telemetry.adc[2]").unwrap());
        assert_eq!(v, vec![("telemetry.adc[2]".into(), 30.0)]);
        let v = extract_values(&m, &FieldPath::parse("telemetry.adc[*]").unwrap());
        assert_eq!(v.len(), 4);
        assert_eq!(v[3], ("telemetry.adc[3]".into(), 40.0));
        assert!(extract_values(&m, &FieldPath::parse("telemetry.adc[9]").unwrap()).is_empty());
        let v = extract(&m, &FieldPath::parse("telemetry.state").unwrap());
        assert!(matches!(v[0].value, Value::EnumNumber(2)));
        // Unset proto3 scalar gives its default.
        let v = extract_values(&m, &FieldPath::parse("telemetry.temperature_c").unwrap());
        assert_eq!(v, vec![("telemetry.temperature_c".into(), 0.0)]);
    }

    #[test]
    fn absent_oneof_member_selects_nothing() {
        let m = telemetry_msg();
        assert!(extract(&m, &FieldPath::parse("status.battery_mv").unwrap()).is_empty());
        assert!(extract(&m, &FieldPath::parse("nope.x").unwrap()).is_empty());
    }

    #[test]
    fn type_name_prefix_is_skipped() {
        let s = demo_schema();
        let d = s.message("demo.Telemetry").unwrap();
        let m = DynamicMessage::deserialize(d.clone(), serde_json::json!({"vout_raw": 7})).unwrap();
        let v = extract_values(&m, &FieldPath::parse("telemetry.vout_raw").unwrap());
        assert_eq!(v, vec![("telemetry.vout_raw".into(), 7.0)]);
        assert!(resolve_field(&d, &FieldPath::parse("telemetry.vout_raw").unwrap()).is_ok());
        assert!(resolve_field(&d, &FieldPath::parse("vout_raw").unwrap()).is_ok());
    }

    #[test]
    fn resolve_errors() {
        let s = demo_schema();
        let d = s.message("demo.DeviceToHost").unwrap();
        assert!(resolve_field(&d, &FieldPath::parse("telemetry.adc[*]").unwrap()).is_ok());
        let e = resolve_field(&d, &FieldPath::parse("telemetry.nope").unwrap()).unwrap_err();
        assert!(e.contains("no field `nope`"), "{e}");
        assert!(resolve_field(&d, &FieldPath::parse("telemetry.vout_raw[1]").unwrap()).is_err());
        assert!(resolve_field(&d, &FieldPath::parse("seq.x").unwrap()).is_err());
    }

    #[test]
    fn snake_case() {
        assert_eq!(to_snake("SetBias"), "set_bias");
        assert_eq!(to_snake("Ping"), "ping");
    }
}
