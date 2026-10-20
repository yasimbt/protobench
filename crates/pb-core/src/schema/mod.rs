//! Schema handling: compiling `.proto` sources at runtime with `protox` (no
//! external `protoc`), loading compiled descriptor sets, proto comments, field
//! paths and the field metadata overlay.

mod meta;
mod path;
pub mod wire;

use std::path::{Path, PathBuf};

use prost::Message as _;
use prost_reflect::{DescriptorPool, EnumDescriptor, FieldDescriptor, MessageDescriptor};

pub use meta::{format_value, FieldDisplay, LimitState};
pub use path::{
    extract, extract_values, resolve_field, to_snake, type_name_matches, value_to_f64, Extracted,
    FieldPath, PathIndex, PathSeg,
};

/// Source of the bundled `nanopb.proto` stub.
pub const NANOPB_PROTO: &str = include_str!("../../../../assets/proto/nanopb.proto");

/// A schema error with its location.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{}{}: {message}", file.as_deref().unwrap_or("<schema>"), line.map(|l| format!(":{l}")).unwrap_or_default())]
pub struct SchemaError {
    /// File name (as protox resolved it), when known.
    pub file: Option<String>,
    /// 1-based line, when known.
    pub line: Option<usize>,
    /// 1-based column, when known.
    pub column: Option<usize>,
    /// Message.
    pub message: String,
}

impl SchemaError {
    fn plain(message: impl Into<String>) -> Self {
        Self {
            file: None,
            line: None,
            column: None,
            message: message.into(),
        }
    }
}

/// A compiled schema: the descriptor pool plus its encoded
/// `FileDescriptorSet` (embedded in recordings and MCAP exports).
#[derive(Debug, Clone)]
pub struct Schema {
    pool: DescriptorPool,
    fds_bytes: Vec<u8>,
}

struct BundledResolver;

impl protox::file::FileResolver for BundledResolver {
    fn open_file(&self, name: &str) -> Result<protox::file::File, protox::Error> {
        if name == "nanopb.proto" {
            protox::file::File::from_source(name, NANOPB_PROTO)
        } else {
            Err(protox::Error::file_not_found(name))
        }
    }
}

impl Schema {
    /// Compiles `.proto` files. Paths are relative to `base_dir`, as are
    /// include paths. `nanopb.proto` and the Google well-known types are always
    /// importable. When no include path is given, the folder of each file and
    /// `base_dir` are used.
    pub fn compile(
        base_dir: &Path,
        proto_files: &[String],
        include_paths: &[String],
    ) -> Result<Schema, SchemaError> {
        use protox::file::{ChainFileResolver, GoogleFileResolver, IncludeFileResolver};

        if proto_files.is_empty() {
            return Err(SchemaError::plain("no proto files given"));
        }
        let mut includes: Vec<PathBuf> = include_paths.iter().map(|p| base_dir.join(p)).collect();
        if includes.is_empty() {
            for f in proto_files {
                if let Some(parent) = base_dir.join(f).parent() {
                    if !includes.iter().any(|i| i == parent) {
                        includes.push(parent.to_path_buf());
                    }
                }
            }
        }
        let mut chain = ChainFileResolver::new();
        for inc in &includes {
            chain.add(IncludeFileResolver::new(inc.clone()));
        }
        chain.add(BundledResolver);
        chain.add(GoogleFileResolver::new());

        let mut compiler = protox::Compiler::with_file_resolver(chain);
        compiler.include_imports(true).include_source_info(true);
        for f in proto_files {
            let abs = base_dir.join(f);
            if !abs.exists() {
                return Err(SchemaError {
                    file: Some(abs.display().to_string()),
                    line: None,
                    column: None,
                    message: "file not found".into(),
                });
            }
            // Resolve to the name relative to the include path that contains it.
            let name = includes
                .iter()
                .find_map(|inc| abs.strip_prefix(inc).ok())
                .map(|rel| rel.to_string_lossy().replace('\\', "/"));
            let r = match name {
                Some(n) => compiler.open_file(n),
                None => compiler.open_file(&abs),
            };
            if let Err(e) = r {
                return Err(protox_error(&e, &includes));
            }
        }
        let fds_bytes = compiler.encode_file_descriptor_set();
        let pool = compiler.descriptor_pool();
        Ok(Schema { pool, fds_bytes })
    }

    /// Loads an encoded `FileDescriptorSet` (`.pb` / `.desc`).
    pub fn from_descriptor_set(bytes: &[u8]) -> Result<Schema, SchemaError> {
        let pool = DescriptorPool::decode(bytes)
            .map_err(|e| SchemaError::plain(format!("invalid FileDescriptorSet: {e}")))?;
        Ok(Schema {
            pool,
            fds_bytes: bytes.to_vec(),
        })
    }

    /// Loads a descriptor set file.
    pub fn from_descriptor_set_file(path: &Path) -> Result<Schema, SchemaError> {
        let bytes = std::fs::read(path).map_err(|e| SchemaError {
            file: Some(path.display().to_string()),
            line: None,
            column: None,
            message: e.to_string(),
        })?;
        Self::from_descriptor_set(&bytes).map_err(|mut e| {
            e.file = Some(path.display().to_string());
            e
        })
    }

    /// The descriptor pool.
    pub fn pool(&self) -> &DescriptorPool {
        &self.pool
    }

    /// Encoded `FileDescriptorSet` of all files, including imports.
    pub fn fds_bytes(&self) -> &[u8] {
        &self.fds_bytes
    }

    /// Looks up a message by full name (a leading `.` is accepted).
    pub fn message(&self, name: &str) -> Option<MessageDescriptor> {
        self.pool.get_message_by_name(name.trim_start_matches('.'))
    }

    /// Looks up an enum by full name.
    pub fn enumeration(&self, name: &str) -> Option<EnumDescriptor> {
        self.pool.get_enum_by_name(name.trim_start_matches('.'))
    }

    /// All messages, excluding those from `google/protobuf/*` and `nanopb.proto`.
    pub fn user_messages(&self) -> Vec<MessageDescriptor> {
        let mut v: Vec<_> = self
            .pool
            .all_messages()
            .filter(|m| !is_support_file(m.parent_file().name()))
            .collect();
        v.sort_by(|a, b| a.full_name().cmp(b.full_name()));
        v
    }

    /// Leading (else trailing) comment of a message.
    pub fn message_comment(&self, m: &MessageDescriptor) -> Option<String> {
        comment(m.parent_file().file_descriptor_proto(), m.path())
    }

    /// Leading (else trailing) comment of a field.
    pub fn field_comment(&self, f: &FieldDescriptor) -> Option<String> {
        comment(f.parent_file().file_descriptor_proto(), f.path())
    }

    /// Leading (else trailing) comment of an enum.
    pub fn enum_comment(&self, e: &EnumDescriptor) -> Option<String> {
        comment(e.parent_file().file_descriptor_proto(), e.path())
    }

    /// Comment of an enum value.
    pub fn enum_value_comment(&self, v: &prost_reflect::EnumValueDescriptor) -> Option<String> {
        comment(v.parent_file().file_descriptor_proto(), v.path())
    }

    /// Encodes the schema as a `prost_types::FileDescriptorSet`.
    pub fn file_descriptor_set(&self) -> prost_types::FileDescriptorSet {
        prost_types::FileDescriptorSet::decode(self.fds_bytes.as_slice()).unwrap_or_default()
    }
}

/// True for bundled support files that the schema browser hides by default.
pub fn is_support_file(name: &str) -> bool {
    name.starts_with("google/protobuf/") || name == "nanopb.proto"
}

fn comment(file: &prost_types::FileDescriptorProto, path: &[i32]) -> Option<String> {
    let info = file.source_code_info.as_ref()?;
    let loc = info.location.iter().find(|l| l.path == path)?;
    let text = loc
        .leading_comments
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or(loc.trailing_comments.as_deref())?;
    let cleaned = text
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

fn protox_error(e: &protox::Error, includes: &[PathBuf]) -> SchemaError {
    use miette::Diagnostic;

    let mut out = SchemaError {
        file: e.file().map(|f| resolve_file_name(f, includes)),
        line: None,
        column: None,
        message: e.to_string(),
    };
    if let (Some(src), Some(mut labels)) = (e.source_code(), e.labels()) {
        if let Some(label) = labels.next() {
            if let Ok(contents) = src.read_span(label.inner(), 0, 0) {
                out.line = Some(contents.line() + 1);
                out.column = Some(contents.column() + 1);
            }
            if let Some(l) = label.label() {
                if !out.message.contains(l) {
                    out.message = format!("{} ({l})", out.message);
                }
            }
        }
    }
    out
}

fn resolve_file_name(name: &str, includes: &[PathBuf]) -> String {
    includes
        .iter()
        .map(|i| i.join(name))
        .find(|p| p.exists())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| name.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn demo_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/demo-sensor")
    }

    pub fn demo_schema() -> Schema {
        Schema::compile(
            &demo_dir(),
            &["proto/device.proto".into()],
            &["proto".into()],
        )
        .expect("demo schema compiles")
    }

    #[test]
    fn example_proto_compiles_with_nanopb_import() {
        let s = demo_schema();
        let m = s.message("demo.Telemetry").unwrap();
        assert!(m.get_field_by_name("adc").unwrap().is_list());
        assert!(s.message("demo.HostToDevice").is_some());
        assert!(s.enumeration("demo.SensorState").is_some());
        assert!(s.pool().get_file_by_name("nanopb.proto").is_some());
        // Descriptor set round trip.
        let s2 = Schema::from_descriptor_set(s.fds_bytes()).unwrap();
        assert!(s2.message("demo.DeviceToHost").is_some());
    }

    #[test]
    fn comments_are_available() {
        let s = demo_schema();
        let m = s.message("demo.Telemetry").unwrap();
        assert_eq!(
            s.message_comment(&m).as_deref(),
            Some("One sample of the analogue front end.")
        );
        let f = m.get_field_by_name("current_na").unwrap();
        assert_eq!(
            s.field_comment(&f).as_deref(),
            Some("Sensor current in nanoamps.")
        );
        assert!(s
            .user_messages()
            .iter()
            .all(|m| !m.full_name().starts_with("google")));
    }

    #[test]
    fn malformed_proto_reports_file_and_line() {
        let dir = std::env::temp_dir().join(format!("pb-schema-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("bad.proto"),
            "syntax = \"proto3\";\npackage x;\n\nmessage A {\n  uint32 a = 1\n}\n",
        )
        .unwrap();
        let e = Schema::compile(&dir, &["bad.proto".into()], &[]).unwrap_err();
        assert!(
            e.file.as_deref().unwrap_or("").ends_with("bad.proto"),
            "{e:?}"
        );
        assert!(matches!(e.line, Some(5) | Some(6)), "{e:?}");
        assert!(e.to_string().contains("bad.proto:"), "{e}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_type_reports_line() {
        let dir = std::env::temp_dir().join(format!("pb-schema-bad2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("t.proto"),
            "syntax = \"proto3\";\npackage x;\nmessage A {\n  Missing m = 1;\n}\n",
        )
        .unwrap();
        let e = Schema::compile(&dir, &["t.proto".into()], &[]).unwrap_err();
        assert_eq!(e.line, Some(4), "{e:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn proto2_and_maps_and_wkt() {
        let dir = std::env::temp_dir().join(format!("pb-schema-p2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("p.proto"),
            r#"syntax = "proto2";
package p;
import "google/protobuf/timestamp.proto";
message M {
  required int32 a = 1;
  optional string b = 2 [default = "x"];
  map<string, int32> m = 3;
  optional google.protobuf.Timestamp t = 4;
  message Inner { optional bytes data = 1; }
  repeated Inner inner = 5;
}
"#,
        )
        .unwrap();
        let s = Schema::compile(&dir, &["p.proto".into()], &[]).unwrap();
        let m = s.message("p.M").unwrap();
        assert!(m.get_field_by_name("m").unwrap().is_map());
        assert!(s.message("p.M.Inner").is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
