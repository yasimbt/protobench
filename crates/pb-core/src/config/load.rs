//! Parsing `project.toml`: syntax errors with lines, migration of older
//! `config_version`s, unknown-key warnings, and key location for diagnostics.

use std::path::Path;

use serde::de::DeserializeOwned;

use super::model::{ProjectConfig, CONFIG_VERSION};
use super::validate::validate;
use crate::diag::{line_col, Diagnostic};

/// One step of a TOML key path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    /// Table key.
    Key(String),
    /// Array index.
    Index(usize),
}

impl From<&str> for Seg {
    fn from(s: &str) -> Self {
        Seg::Key(s.to_string())
    }
}

impl From<usize> for Seg {
    fn from(i: usize) -> Self {
        Seg::Index(i)
    }
}

/// Builds a key path: `key_path!["commands", 2, "message"]`.
#[macro_export]
macro_rules! key_path {
    ($($s:expr),* $(,)?) => { vec![$($crate::config::Seg::from($s)),*] };
}

/// Formats a key path as `commands[2].message`.
pub fn path_to_string(path: &[Seg]) -> String {
    let mut s = String::new();
    for seg in path {
        match seg {
            Seg::Key(k) => {
                if !s.is_empty() {
                    s.push('.');
                }
                s.push_str(k);
            }
            Seg::Index(i) => s.push_str(&format!("[{i}]")),
        }
    }
    s
}

/// Finds the 1-based line of a key path in TOML source, falling back to the
/// nearest existing parent.
pub fn locate_line(src: &str, path: &[Seg]) -> Option<usize> {
    let doc = toml_edit::Document::parse(src.to_string()).ok()?;
    let mut best: Option<usize> = None;
    let mut cur = Node::Table(doc.as_table());
    for seg in path {
        let (span, next) = cur.step(seg)?.into_parts();
        if let Some(sp) = span {
            best = Some(line_col(src, sp.start).0);
        }
        match next {
            Some(n) => cur = n,
            None => break,
        }
    }
    best
}

enum Node<'a> {
    Table(&'a dyn toml_edit::TableLike),
    Item(&'a toml_edit::Item),
    Value(&'a toml_edit::Value),
}

struct Step<'a> {
    span: Option<std::ops::Range<usize>>,
    next: Option<Node<'a>>,
}

impl<'a> Step<'a> {
    fn into_parts(self) -> (Option<std::ops::Range<usize>>, Option<Node<'a>>) {
        (self.span, self.next)
    }
}

impl<'a> Node<'a> {
    fn step(&self, seg: &Seg) -> Option<Step<'a>> {
        match (self, seg) {
            (Node::Table(t), Seg::Key(k)) => {
                let (key, item) = t.get_key_value(k)?;
                Some(Step {
                    span: key.span().or_else(|| item.span()),
                    next: Some(Node::Item(item)),
                })
            }
            (Node::Item(item), seg) => match item {
                toml_edit::Item::Table(t) => Node::Table(t).step(seg),
                toml_edit::Item::Value(v) => Node::Value(v).step(seg),
                toml_edit::Item::ArrayOfTables(aot) => match seg {
                    Seg::Index(i) => {
                        let t = aot.get(*i)?;
                        Some(Step {
                            span: t.span(),
                            next: Some(Node::Table(t)),
                        })
                    }
                    Seg::Key(_) => None,
                },
                toml_edit::Item::None => None,
            },
            (Node::Value(v), seg) => match (v, seg) {
                (toml_edit::Value::InlineTable(t), Seg::Key(_)) => Node::Table(t).step(seg),
                (toml_edit::Value::Array(a), Seg::Index(i)) => {
                    let v = a.get(*i)?;
                    Some(Step {
                        span: v.span(),
                        next: Some(Node::Value(v)),
                    })
                }
                _ => None,
            },
            (Node::Table(_), Seg::Index(_)) => None,
        }
    }
}

/// Result of loading a config file.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// The parsed config, if the file could be parsed at all.
    pub config: Option<ProjectConfig>,
    /// Errors and warnings.
    pub diagnostics: Vec<Diagnostic>,
    /// Set when the file was migrated from an older `config_version`.
    pub migrated_from: Option<u32>,
}

/// Parses and validates `project.toml` source text. Never panics; problems
/// come back as diagnostics.
pub fn load_config_str(src: &str, file: &Path) -> LoadedConfig {
    let mut diagnostics = Vec::new();
    let table: toml::Table = match src.parse() {
        Ok(t) => t,
        Err(e) => {
            diagnostics.push(toml_error_diag(src, file, &e));
            return LoadedConfig {
                config: None,
                diagnostics,
                migrated_from: None,
            };
        }
    };

    let version = match table.get("config_version") {
        None => 0,
        Some(toml::Value::Integer(v)) if *v >= 0 => *v as u32,
        Some(_) => {
            diagnostics.push(Diagnostic::error(
                file,
                locate_line(src, &key_path!["config_version"]),
                "config_version must be a non-negative integer",
            ));
            return LoadedConfig {
                config: None,
                diagnostics,
                migrated_from: None,
            };
        }
    };
    if version > CONFIG_VERSION {
        diagnostics.push(Diagnostic::error(
            file,
            locate_line(src, &key_path!["config_version"]),
            format!(
                "config_version {version} is newer than this tool supports ({CONFIG_VERSION}); upgrade ProtoBench"
            ),
        ));
        return LoadedConfig {
            config: None,
            diagnostics,
            migrated_from: None,
        };
    }

    let mut ignored = Vec::new();
    let (parsed, migrated_from) = if version < CONFIG_VERSION {
        let migrated = migrate(table, version);
        let r: Result<ProjectConfig, String> =
            deserialize_tracking(toml::Value::Table(migrated), &mut ignored)
                .map_err(|e| e.to_string());
        (r.map_err(|m| (m, None)), Some(version))
    } else {
        let r = match toml::Deserializer::parse(src) {
            Ok(de) => deserialize_tracking::<ProjectConfig, _>(de, &mut ignored)
                .map_err(|e| (e.message().to_string(), e.span())),
            Err(e) => Err((e.message().to_string(), e.span())),
        };
        (r, None)
    };

    let mut config = match parsed {
        Ok(c) => c,
        Err((msg, span)) => {
            let line = span
                .map(|s| line_col(src, s.start).0)
                .or_else(|| key_in_message(&msg).and_then(|p| locate_line(src, &p)));
            diagnostics.push(Diagnostic::error(file, line, msg.trim().to_string()));
            return LoadedConfig {
                config: None,
                diagnostics,
                migrated_from,
            };
        }
    };
    config.config_version = CONFIG_VERSION;

    for path in ignored {
        diagnostics.push(Diagnostic::warning(
            file,
            locate_line(src, &path),
            format!("unknown key `{}` (ignored)", path_to_string(&path)),
        ));
    }
    if let Some(v) = migrated_from {
        diagnostics.push(Diagnostic::warning(
            file,
            None,
            format!("migrated from config_version {v} to {CONFIG_VERSION}; save the project to update the file"),
        ));
    }
    for (path, sev_err, msg) in validate(&config) {
        let line = locate_line(src, &path);
        diagnostics.push(if sev_err {
            Diagnostic::error(file, line, msg)
        } else {
            Diagnostic::warning(file, line, msg)
        });
    }
    LoadedConfig {
        config: Some(config),
        diagnostics,
        migrated_from,
    }
}

/// Reads and parses a config file.
pub fn load_config_file(file: &Path) -> LoadedConfig {
    match std::fs::read_to_string(file) {
        Ok(src) => load_config_str(&src, file),
        Err(e) => LoadedConfig {
            config: None,
            diagnostics: vec![Diagnostic::error(file, None, format!("cannot read: {e}"))],
            migrated_from: None,
        },
    }
}

/// Pulls a key path out of messages like "... in `framing.type`".
fn key_in_message(msg: &str) -> Option<Vec<Seg>> {
    let start = msg.rfind("in `")? + 4;
    let end = start + msg[start..].find('`')?;
    let mut out = Vec::new();
    for part in msg[start..end].split('.') {
        let (key, idx) = match part.find('[') {
            Some(i) => (&part[..i], part[i + 1..].trim_end_matches(']').parse().ok()),
            None => (part, None),
        };
        out.push(Seg::Key(key.to_string()));
        if let Some(i) = idx {
            out.push(Seg::Index(i));
        }
    }
    Some(out)
}

fn toml_error_diag(src: &str, file: &Path, e: &toml::de::Error) -> Diagnostic {
    let mut d = Diagnostic::error(file, None, e.message().trim().to_string());
    if let Some(span) = e.span() {
        let (l, c) = line_col(src, span.start);
        d.line = Some(l);
        d.column = Some(c);
    }
    d
}

/// Deserializes while recording keys that the model does not know about.
pub fn deserialize_tracking<'de, T, D>(de: D, ignored: &mut Vec<Vec<Seg>>) -> Result<T, D::Error>
where
    T: DeserializeOwned,
    D: serde::Deserializer<'de>,
{
    serde_ignored::deserialize(de, |path| ignored.push(ignored_path(&path)))
}

fn ignored_path(path: &serde_ignored::Path<'_>) -> Vec<Seg> {
    use serde_ignored::Path as P;
    match path {
        P::Root => Vec::new(),
        P::Seq { parent, index } => {
            let mut v = ignored_path(parent);
            v.push(Seg::Index(*index));
            v
        }
        P::Map { parent, key } => {
            let mut v = ignored_path(parent);
            v.push(Seg::Key(key.clone()));
            v
        }
        P::Some { parent } | P::NewtypeStruct { parent } | P::NewtypeVariant { parent } => {
            ignored_path(parent)
        }
    }
}

/// Upgrades a raw config table from `from` to [`CONFIG_VERSION`].
///
/// Version 0 is a file without `config_version`. It used `framing.kind`
/// instead of `framing.type` and `connection.default` instead of
/// `connection.profile`.
pub fn migrate(mut table: toml::Table, from: u32) -> toml::Table {
    if from < 1 {
        if let Some(toml::Value::Table(fr)) = table.get_mut("framing") {
            if let Some(k) = fr.remove("kind") {
                fr.entry("type").or_insert(k);
            }
        }
        if let Some(toml::Value::Table(conn)) = table.get_mut("connection") {
            if let Some(k) = conn.remove("default") {
                conn.entry("profile").or_insert(k);
            }
        }
    }
    table.insert(
        "config_version".into(),
        toml::Value::Integer(CONFIG_VERSION as i64),
    );
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::*;

    fn load(src: &str) -> LoadedConfig {
        load_config_str(src, Path::new("project.toml"))
    }

    const MIN: &str = r#"
config_version = 1
name = "x"
[schema]
proto_files = ["a.proto"]
[codec]
mode = "envelope"
tx_root = "a.Tx"
rx_root = "a.Rx"
"#;

    #[test]
    fn minimal_config_parses_with_defaults() {
        let l = load(MIN);
        assert!(
            l.diagnostics.iter().all(|d| !d.is_error()),
            "{:?}",
            l.diagnostics
        );
        let c = l.config.unwrap();
        assert_eq!(c.name, "x");
        assert_eq!(c.framing.kind, FramingType::Cobs);
        assert_eq!(c.framing.max_frame_len, 1024);
        assert_eq!(c.correlation.default_timeout_ms, 500);
    }

    #[test]
    fn syntax_error_has_line() {
        let l = load("name = \"x\"\n[framing\n");
        assert!(l.config.is_none());
        assert_eq!(l.diagnostics.len(), 1);
        assert_eq!(l.diagnostics[0].line, Some(2));
    }

    #[test]
    fn type_error_names_key_and_line() {
        let src = format!("{MIN}\n[framing]\nmax_frame_len = \"big\"\n");
        let l = load(&src);
        assert!(l.config.is_none());
        let d = &l.diagnostics[0];
        assert!(d.is_error());
        assert_eq!(d.line, Some(12), "{d}");
    }

    #[test]
    fn unknown_keys_warn_with_line() {
        let src = format!(
            "{MIN}bogus = 1\n\n[[commands]]\nname = \"a\"\nmessage = \"m\"\ncolr = \"#fff\"\n"
        );
        let l = load(&src);
        assert!(l.config.is_some());
        let w: Vec<_> = l.diagnostics.iter().filter(|d| !d.is_error()).collect();
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w
            .iter()
            .any(|d| d.message.contains("codec.bogus") && d.line == Some(10)));
        assert!(w
            .iter()
            .any(|d| d.message.contains("commands[0].colr") && d.line == Some(15)));
    }

    #[test]
    fn validation_error_points_at_key() {
        let src = format!("{MIN}\n[framing]\ntype = \"fixed_len\"\nlen_bytes = 3\n");
        let l = load(&src);
        let e = l.diagnostics.iter().find(|d| d.is_error()).unwrap();
        assert!(e.message.contains("len_bytes"), "{e}");
        assert_eq!(e.line, Some(13));
    }

    #[test]
    fn migrates_version_zero() {
        let src = "name = \"old\"\n[framing]\nkind = \"slip\"\n[codec]\ntx_root = \"a.Tx\"\nrx_root = \"a.Rx\"\n";
        let l = load(src);
        assert_eq!(l.migrated_from, Some(0));
        let c = l.config.unwrap();
        assert_eq!(c.framing.kind, FramingType::Slip);
        assert_eq!(c.config_version, CONFIG_VERSION);
    }

    #[test]
    fn rejects_newer_version() {
        let l = load("config_version = 99\n");
        assert!(l.config.is_none());
        assert!(l.diagnostics[0].message.contains("newer"));
    }

    #[test]
    fn id_width_parses() {
        let src = "config_version = 1\n[codec]\nmode = \"id_prefix\"\nid_width = \"varint\"\n[codec.ids]\n0x01 = \"a.B\"\n";
        let c = load(src).config.unwrap();
        assert_eq!(c.codec.id_width, IdWidth::Varint);
        assert_eq!(c.codec.ids.get("0x01").map(String::as_str), Some("a.B"));
    }
}
