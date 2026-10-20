//! Raw protobuf wire-format walker. Produces byte spans for each field so the
//! message inspector can highlight the bytes of a selected field.

use prost_reflect::{Kind, MessageDescriptor};

/// Byte span of one encoded field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpan {
    /// Field number.
    pub number: u32,
    /// Field name, when the descriptor knows it.
    pub name: Option<String>,
    /// Start of the tag.
    pub start: usize,
    /// One past the last byte of the value.
    pub end: usize,
    /// Spans inside a nested message.
    pub children: Vec<FieldSpan>,
}

/// Reads a varint at `pos`. Returns (value, bytes used).
pub fn read_varint(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for i in 0..10 {
        let b = *buf.get(pos + i)?;
        v |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// Walks `buf` as a message of type `desc` (or untyped when `None`).
/// Returns the spans parsed before the first malformed byte.
pub fn field_spans(buf: &[u8], desc: Option<&MessageDescriptor>) -> Vec<FieldSpan> {
    walk(buf, 0, buf.len(), desc, 0)
}

fn walk(
    buf: &[u8],
    start: usize,
    end: usize,
    desc: Option<&MessageDescriptor>,
    depth: usize,
) -> Vec<FieldSpan> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos < end {
        let Some((tag, n)) = read_varint(&buf[..end], pos) else {
            break;
        };
        let number = (tag >> 3) as u32;
        let wire = (tag & 7) as u8;
        let field_start = pos;
        pos += n;
        let field = desc.and_then(|d| d.get_field(number));
        let mut children = Vec::new();
        let value_end = match wire {
            0 => match read_varint(&buf[..end], pos) {
                Some((_, n)) => pos + n,
                None => break,
            },
            1 => pos + 8,
            5 => pos + 4,
            2 => {
                let Some((len, n)) = read_varint(&buf[..end], pos) else {
                    break;
                };
                let body = pos + n;
                let Some(body_end) = body.checked_add(len as usize) else {
                    break;
                };
                if body_end > end {
                    break;
                }
                if depth < 32 {
                    if let Some(Kind::Message(m)) = field.as_ref().map(|f| f.kind()) {
                        children = walk(buf, body, body_end, Some(&m), depth + 1);
                    }
                }
                body_end
            }
            _ => break,
        };
        if value_end > end {
            break;
        }
        out.push(FieldSpan {
            number,
            name: field.map(|f| f.name().to_string()),
            start: field_start,
            end: value_end,
            children,
        });
        pos = value_end;
    }
    out
}

/// Finds the span for a path of field names (e.g. `["telemetry", "vout_raw"]`).
/// For repeated fields the first occurrence is returned.
pub fn find_span<'a>(spans: &'a [FieldSpan], names: &[&str]) -> Option<&'a FieldSpan> {
    let (first, rest) = names.split_first()?;
    let s = spans.iter().find(|s| s.name.as_deref() == Some(*first))?;
    if rest.is_empty() {
        Some(s)
    } else {
        find_span(&s.children, rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::tests::demo_schema;
    use prost::Message;
    use prost_reflect::DynamicMessage;

    #[test]
    fn spans_cover_nested_fields() {
        let s = demo_schema();
        let d = s.message("demo.DeviceToHost").unwrap();
        let m = DynamicMessage::deserialize(
            d.clone(),
            serde_json::json!({"seq": 5, "telemetry": {"vout_raw": 300, "adc": [1,2]}}),
        )
        .unwrap();
        let bytes = m.encode_to_vec();
        let spans = field_spans(&bytes, Some(&d));
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].name.as_deref(), Some("seq"));
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans[1].end, bytes.len());
        let v = find_span(&spans, &["telemetry", "vout_raw"]).unwrap();
        // vout_raw = 300: tag 0x10, varint AC 02
        assert_eq!(&bytes[v.start..v.end], &[0x10, 0xAC, 0x02]);
    }

    #[test]
    fn garbage_does_not_panic() {
        let spans = field_spans(&[0xFF, 0xFF, 0xFF], None);
        assert!(spans.is_empty());
        let spans = field_spans(&[0x0A, 0x7F, 0x00], None);
        assert!(spans.is_empty());
    }
}
