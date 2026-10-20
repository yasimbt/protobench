//! A project folder: `project.toml`, the compiled schema, sequences, layout
//! and recordings. Opening a project never fails because of bad content;
//! problems are collected as diagnostics so the UI can show them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{load_config_str, locate_line, CodecMode, ProjectConfig, Seg, TelemetryMode};
use crate::diag::Diagnostic;
use crate::key_path;
use crate::schema::{resolve_field, FieldPath, Schema};

/// File name of the project config inside a project folder.
pub const PROJECT_FILE: &str = "project.toml";
/// File name of the saved GUI layout.
pub const LAYOUT_FILE: &str = "layout.json";

/// An opened project.
#[derive(Debug, Clone)]
pub struct Project {
    /// Project folder.
    pub dir: PathBuf,
    /// Text of `project.toml` as loaded.
    pub config_text: String,
    /// Parsed config (defaults if it could not be parsed).
    pub config: ProjectConfig,
    /// False if `project.toml` could not be parsed.
    pub config_ok: bool,
    /// Compiled schema, if it compiled.
    pub schema: Option<Arc<Schema>>,
    /// Errors and warnings from config, schema and cross-checks.
    pub diagnostics: Vec<Diagnostic>,
}

impl Project {
    /// Opens a project folder (or a `project.toml` path).
    pub fn open(path: &Path) -> Project {
        let dir = if path.is_file() {
            path.parent().map(Path::to_path_buf).unwrap_or_default()
        } else {
            path.to_path_buf()
        };
        let file = dir.join(PROJECT_FILE);
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) => {
                return Project {
                    dir,
                    config_text: String::new(),
                    config: ProjectConfig::default(),
                    config_ok: false,
                    schema: None,
                    diagnostics: vec![Diagnostic::error(&file, None, format!("cannot read: {e}"))],
                }
            }
        };
        Self::from_text(dir, text)
    }

    /// Builds a project from config text (the folder is used to resolve
    /// relative paths).
    pub fn from_text(dir: PathBuf, text: String) -> Project {
        let file = dir.join(PROJECT_FILE);
        let loaded = load_config_str(&text, &file);
        let mut diagnostics = loaded.diagnostics;
        let config_ok = loaded.config.is_some();
        let config = loaded.config.unwrap_or_default();

        let schema = if config_ok {
            match load_schema(&dir, &config) {
                Ok(s) => Some(Arc::new(s)),
                Err(d) => {
                    diagnostics.push(d);
                    None
                }
            }
        } else {
            None
        };
        let mut p = Project {
            dir,
            config_text: text,
            config,
            config_ok,
            schema,
            diagnostics,
        };
        if let Some(schema) = p.schema.clone() {
            let found = check_against_schema(&p.config, &schema);
            for (path, msg) in found {
                let line = locate_line(&p.config_text, &path);
                p.diagnostics.push(Diagnostic::error(&file, line, msg));
            }
        }
        p
    }

    /// Re-reads the project from disk.
    pub fn reload(&self) -> Project {
        Project::open(&self.dir)
    }

    /// True if any diagnostic is an error.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }

    /// Only the errors.
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| d.is_error())
    }

    /// The schema, or an error naming why it is missing.
    pub fn require_schema(&self) -> crate::Result<Arc<Schema>> {
        self.schema.clone().ok_or_else(|| {
            let why = self
                .errors()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            crate::CoreError::Schema(if why.is_empty() {
                "schema not loaded".into()
            } else {
                why
            })
        })
    }

    /// Path of `project.toml`.
    pub fn config_path(&self) -> PathBuf {
        self.dir.join(PROJECT_FILE)
    }

    /// Path of `layout.json`.
    pub fn layout_path(&self) -> PathBuf {
        self.dir.join(LAYOUT_FILE)
    }

    /// The `sequences/` folder.
    pub fn sequences_dir(&self) -> PathBuf {
        self.dir.join("sequences")
    }

    /// The `recordings/` folder.
    pub fn recordings_dir(&self) -> PathBuf {
        self.dir.join("recordings")
    }

    /// Names of the sequences in `sequences/` (file stems), sorted.
    pub fn sequence_names(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.sequences_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.extension().and_then(|x| x.to_str()) == Some("toml"))
                    .then(|| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                    .flatten()
            })
            .collect();
        v.sort();
        v
    }

    /// Path of a sequence by name (or a path to a `.toml` file).
    pub fn sequence_path(&self, name: &str) -> PathBuf {
        let p = Path::new(name);
        if p.extension().is_some() && (p.is_absolute() || p.exists()) {
            p.to_path_buf()
        } else {
            self.sequences_dir().join(format!("{name}.toml"))
        }
    }

    /// Copies the project folder (config, protos, sequences, layout; not
    /// recordings) to `dest` and renames it.
    pub fn duplicate(&self, dest: &Path, new_name: &str) -> crate::Result<PathBuf> {
        if dest.exists() {
            return Err(crate::CoreError::Config(format!(
                "{} already exists",
                dest.display()
            )));
        }
        copy_dir(
            &self.dir,
            dest,
            &["recordings", "reports", "target", "layout.local.json"],
        )?;
        let cfg = dest.join(PROJECT_FILE);
        let text = std::fs::read_to_string(&cfg).map_err(|e| crate::CoreError::io(&cfg, e))?;
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| crate::CoreError::Config(format!("{e}")))?;
        doc["name"] = toml_edit::value(new_name);
        std::fs::write(&cfg, doc.to_string()).map_err(|e| crate::CoreError::io(&cfg, e))?;
        Ok(dest.to_path_buf())
    }
}

/// Creates a new, minimal project in `dir`.
pub fn create_project(dir: &Path, name: &str) -> crate::Result<PathBuf> {
    std::fs::create_dir_all(dir.join("proto")).map_err(|e| crate::CoreError::io(dir, e))?;
    std::fs::create_dir_all(dir.join("sequences")).map_err(|e| crate::CoreError::io(dir, e))?;
    let cfg = dir.join(PROJECT_FILE);
    if cfg.exists() {
        return Err(crate::CoreError::Config(format!(
            "{} already exists",
            cfg.display()
        )));
    }
    let proto = r#"syntax = "proto3";
package app;

message HostToDevice {
  uint32 seq = 1;
  oneof cmd {
    Ping ping = 2;
  }
}

message DeviceToHost {
  uint32 seq = 1;
  oneof msg {
    Pong pong = 2;
  }
}

message Ping {}
message Pong {}
"#;
    let toml = format!(
        r#"config_version = 1
name = {name:?}
description = ""

[schema]
proto_files = ["proto/device.proto"]
include_paths = ["proto"]

[connection]
profile = "sim"

[[connection.profiles]]
name = "sim"
transport = "sim"

[[connection.profiles]]
name = "usb"
transport = "serial"
port = "auto"
baud = 115200

[framing]
type = "cobs"
crc = "crc16_ccitt_false"
max_frame_len = 1024
text_passthrough = true

[codec]
mode = "envelope"
tx_root = "app.HostToDevice"
rx_root = "app.DeviceToHost"

[correlation]
request_field = "seq"
response_field = "seq"

[[commands]]
name = "Ping"
group = "System"
message = "ping"
expect = "pong"

[[simulator.responses]]
on = "ping"
reply = "pong"
"#
    );
    let write =
        |p: PathBuf, s: &str| std::fs::write(&p, s).map_err(|e| crate::CoreError::io(&p, e));
    write(dir.join("proto/device.proto"), proto)?;
    write(
        dir.join(".gitignore"),
        "# ProtoBench: per-user layout, recordings and reports\nlayout.local.json\nrecordings/\nreports/\n",
    )?;
    write(cfg.clone(), &toml)?;
    Ok(cfg)
}

fn copy_dir(src: &Path, dst: &Path, skip: &[&str]) -> crate::Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| crate::CoreError::io(dst, e))?;
    for entry in std::fs::read_dir(src).map_err(|e| crate::CoreError::io(src, e))? {
        let entry = entry.map_err(|e| crate::CoreError::io(src, e))?;
        let name = entry.file_name();
        if skip.iter().any(|s| name == *s) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            copy_dir(&from, &to, &[])?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| crate::CoreError::io(&from, e))?;
        }
    }
    Ok(())
}

fn load_schema(dir: &Path, config: &ProjectConfig) -> Result<Schema, Diagnostic> {
    let cfg_file = dir.join(PROJECT_FILE);
    if let Some(ds) = &config.schema.descriptor_set {
        let path = dir.join(ds);
        return Schema::from_descriptor_set_file(&path)
            .map_err(|e| Diagnostic::error(&path, e.line, e.message));
    }
    if config.schema.proto_files.is_empty() {
        return Err(Diagnostic::error(cfg_file, None, "no schema configured"));
    }
    Schema::compile(
        dir,
        &config.schema.proto_files,
        &config.schema.include_paths,
    )
    .map_err(|e| {
        let mut d = Diagnostic::error(
            e.file.clone().map(PathBuf::from).unwrap_or(cfg_file),
            e.line,
            e.message,
        );
        d.column = e.column;
        d
    })
}

/// Cross-checks the config against the schema: root types, command messages,
/// telemetry paths and simulator messages.
pub fn check_against_schema(config: &ProjectConfig, schema: &Schema) -> Vec<(Vec<Seg>, String)> {
    let mut out = Vec::new();
    let codec = match crate::codec::Codec::new(config, schema, crate::codec::Role::Host) {
        Ok(c) => c,
        Err(e) => {
            out.push((key_path!["codec"], e.to_string()));
            return out;
        }
    };
    for (i, c) in config.commands.iter().enumerate() {
        if let Err(e) = codec.resolve_tx(&c.message) {
            out.push((key_path!["commands", i, "message"], e.to_string()));
        }
        if let Some(exp) = &c.expect {
            if let Err(e) = codec.resolve_rx(exp) {
                out.push((key_path!["commands", i, "expect"], e.to_string()));
            }
        }
    }
    for (i, t) in config.telemetry.iter().enumerate() {
        let msg = match t.mode {
            TelemetryMode::Poll => t.response.as_deref(),
            _ => t.message.as_deref(),
        };
        if t.mode == TelemetryMode::Poll {
            if let Some(req) = &t.request {
                if let Err(e) = codec.resolve_tx(req) {
                    out.push((key_path!["telemetry", i, "request"], e.to_string()));
                }
            }
        }
        let Some(msg) = msg else { continue };
        let target = match codec.resolve_rx(msg) {
            Ok(t) => t,
            Err(e) => {
                out.push((key_path!["telemetry", i], e.to_string()));
                continue;
            }
        };
        for (j, ch) in t.channels.iter().enumerate() {
            let Ok(p) = FieldPath::parse(ch) else {
                continue;
            };
            if let Err(e) = resolve_field(&target.root, &p) {
                out.push((key_path!["telemetry", i, "channels", j], e));
            }
        }
    }
    if let Some(field) = &config.timestamp.field {
        if let Ok(p) = FieldPath::parse(field) {
            let ok = codec
                .rx_types()
                .iter()
                .any(|d| resolve_field(d, &p).is_ok());
            if !ok {
                out.push((
                    key_path!["timestamp", "field"],
                    format!("timestamp field `{field}` does not exist in any received message"),
                ));
            }
        }
    }
    if config.codec.mode != CodecMode::IdPrefix {
        if let Some(rf) = &config.correlation.request_field {
            if let Ok(p) = FieldPath::parse(rf) {
                if let Some(tx) = codec.tx_types().first() {
                    if let Err(e) = resolve_field(tx, &p) {
                        out.push((key_path!["correlation", "request_field"], e));
                    }
                }
            }
        }
        if let Some(rf) = &config.correlation.response_field {
            if let Ok(p) = FieldPath::parse(rf) {
                if let Some(rx) = codec.rx_types().first() {
                    if let Err(e) = resolve_field(rx, &p) {
                        out.push((key_path!["correlation", "response_field"], e));
                    }
                }
            }
        }
    }
    for (i, s) in config.simulator.streams.iter().enumerate() {
        if let Err(e) = codec.resolve_rx(&s.message) {
            out.push((
                key_path!["simulator", "streams", i, "message"],
                e.to_string(),
            ));
        }
    }
    for (i, r) in config.simulator.responses.iter().enumerate() {
        if let Err(e) = codec.resolve_tx(&r.on) {
            out.push((key_path!["simulator", "responses", i, "on"], e.to_string()));
        }
        if let Err(e) = codec.resolve_rx(&r.reply) {
            out.push((
                key_path!["simulator", "responses", i, "reply"],
                e.to_string(),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn demo() -> Project {
        Project::open(&crate::schema::tests::demo_dir())
    }

    #[test]
    fn demo_project_opens_cleanly() {
        let p = demo();
        let errs: Vec<_> = p.diagnostics.iter().map(|d| d.to_string()).collect();
        assert!(p.diagnostics.is_empty(), "{errs:#?}");
        assert!(p.schema.is_some());
        assert_eq!(p.config.connection.profiles.len(), 3);
        assert_eq!(
            p.sequence_names(),
            vec!["scripted".to_string(), "smoke".to_string()]
        );
    }

    #[test]
    fn broken_proto_opens_with_error() {
        let dir = std::env::temp_dir().join(format!("pb-proj-badproto-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        create_project(&dir, "Bad").unwrap();
        std::fs::write(
            dir.join("proto/device.proto"),
            "syntax = \"proto3\";\nmessage {\n",
        )
        .unwrap();
        let p = Project::open(&dir);
        assert!(p.has_errors());
        assert!(p.schema.is_none());
        let e = p.errors().next().unwrap();
        assert!(e.file.ends_with("device.proto"), "{e}");
        assert_eq!(e.line, Some(2), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_toml_opens_with_error() {
        let dir = std::env::temp_dir().join(format!("pb-proj-badtoml-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(PROJECT_FILE),
            "name = \"x\"\n[framing]\ntype = 5\n",
        )
        .unwrap();
        let p = Project::open(&dir);
        assert!(!p.config_ok);
        let e = p.errors().next().unwrap();
        assert_eq!(e.line, Some(3), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_cross_checks_name_keys() {
        let mut text = std::fs::read_to_string(demo().config_path()).unwrap();
        text = text.replace("message = \"set_bias\"", "message = \"set_biass\"");
        text = text.replace("\"telemetry.current_na\",", "\"telemetry.current_nA\",");
        let p = Project::from_text(crate::schema::tests::demo_dir(), text);
        let msgs: Vec<_> = p.errors().map(|d| (d.line, d.message.clone())).collect();
        assert!(
            msgs.iter()
                .any(|(l, m)| m.contains("set_biass") && l.is_some()),
            "{msgs:?}"
        );
        assert!(
            msgs.iter().any(|(_, m)| m.contains("current_nA")),
            "{msgs:?}"
        );
    }

    #[test]
    fn create_and_duplicate() {
        let dir = std::env::temp_dir().join(format!("pb-proj-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        create_project(&dir, "New").unwrap();
        let p = Project::open(&dir);
        assert!(!p.has_errors(), "{:?}", p.diagnostics);
        let dup = dir.with_extension("copy");
        let _ = std::fs::remove_dir_all(&dup);
        p.duplicate(&dup, "Copy").unwrap();
        let q = Project::open(&dup);
        assert_eq!(q.config.name, "Copy");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dup);
    }
}
