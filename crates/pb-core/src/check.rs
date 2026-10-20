//! Whole-project checks for CI (`protobench check`).
//!
//! Collects the project's own diagnostics (`project.toml` and schema), then
//! checks every sequence file against the project (commands, message names,
//! field paths, channels, statistics and variables) and the channels named
//! in `layout.json`. Each finding points at a file and, where possible, a
//! line.

use std::collections::BTreeSet;
use std::path::Path;

use crate::codec::{Codec, Role};
use crate::config::ProjectConfig;
use crate::diag::{line_col, Diagnostic};
use crate::project::Project;
use crate::schema::{resolve_field, FieldPath};
use crate::sequence::{Assertion, SequenceFile, Step};
use crate::telemetry::StatsSnapshot;

/// Every finding for a project, errors first.
pub fn check_project(project: &Project) -> Vec<Diagnostic> {
    let mut out = project.diagnostics.clone();
    let codec = project
        .schema
        .as_ref()
        .filter(|_| project.config_ok)
        .and_then(|s| Codec::new(&project.config, s, Role::Host).ok());
    for name in project.sequence_names() {
        let path = project.sequence_path(&name);
        out.extend(check_sequence_file(&path, &project.config, codec.as_ref()));
    }
    let layout = project.layout_path();
    if layout.exists() {
        out.extend(check_layout_file(&layout, &project.config));
    }
    out.sort_by_key(|d| !d.is_error());
    out
}

/// Checks one sequence file. Without a codec (broken schema) only the parse
/// and the project-level references are checked.
pub fn check_sequence_file(
    path: &Path,
    config: &ProjectConfig,
    codec: Option<&Codec>,
) -> Vec<Diagnostic> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return vec![Diagnostic::error(path, None, format!("cannot read: {e}"))],
    };
    check_sequence_str(&src, path, config, codec)
}

/// Checks sequence source text (see [`check_sequence_file`]).
pub fn check_sequence_str(
    src: &str,
    path: &Path,
    config: &ProjectConfig,
    codec: Option<&Codec>,
) -> Vec<Diagnostic> {
    let seq = match SequenceFile::parse(src, path) {
        Ok(s) => s,
        Err(d) => return vec![d],
    };
    let lines = step_lines(src);
    let mut c = SeqCheck {
        path,
        config,
        codec,
        lines: &lines,
        out: Vec::new(),
    };
    c.steps(&seq.steps, "");
    c.vars(src, &seq);
    c.out
}

/// Line of each step: top-level `[[steps]]` tables and the inline tables of
/// loop `steps` arrays, keyed by index (`"3"`, `"7.1"`).
fn step_lines(src: &str) -> Vec<(String, usize)> {
    fn push(
        src: &str,
        span: Option<std::ops::Range<usize>>,
        index: String,
        out: &mut Vec<(String, usize)>,
    ) {
        if let Some(span) = span {
            out.push((index, line_col(src, span.start).0));
        }
    }
    fn walk_array(src: &str, arr: &toml_edit::Array, prefix: &str, out: &mut Vec<(String, usize)>) {
        for (i, v) in arr.iter().enumerate() {
            let index = format!("{prefix}{}", i + 1);
            push(src, v.span(), index.clone(), out);
            if let Some(inner) = v
                .as_inline_table()
                .and_then(|t| t.get("steps"))
                .and_then(|x| x.as_array())
            {
                walk_array(src, inner, &format!("{index}."), out);
            }
        }
    }
    let mut out = Vec::new();
    let Ok(doc) = src.parse::<toml_edit::Document<String>>() else {
        return out;
    };
    if let Some(aot) = doc.get("steps").and_then(|s| s.as_array_of_tables()) {
        for (i, t) in aot.iter().enumerate() {
            let index = (i + 1).to_string();
            push(src, t.span(), index.clone(), &mut out);
            if let Some(inner) = t.get("steps").and_then(|x| x.as_array()) {
                walk_array(src, inner, &format!("{index}."), &mut out);
            }
        }
    }
    out
}

struct SeqCheck<'a> {
    path: &'a Path,
    config: &'a ProjectConfig,
    codec: Option<&'a Codec>,
    lines: &'a [(String, usize)],
    out: Vec<Diagnostic>,
}

impl SeqCheck<'_> {
    /// Line of a step, falling back to the enclosing loop's line.
    fn line(&self, index: &str) -> Option<usize> {
        let mut idx = index;
        loop {
            if let Some((_, l)) = self.lines.iter().find(|(i, _)| i == idx) {
                return Some(*l);
            }
            idx = idx.rsplit_once('.')?.0;
        }
    }

    fn err(&mut self, index: &str, action: &str, msg: String) {
        let line = self.line(index);
        self.out.push(Diagnostic::error(
            self.path,
            line,
            format!("step {index} ({action}): {msg}"),
        ));
    }

    fn warn(&mut self, index: &str, action: &str, msg: String) {
        let line = self.line(index);
        self.out.push(Diagnostic::warning(
            self.path,
            line,
            format!("step {index} ({action}): {msg}"),
        ));
    }

    fn steps(&mut self, steps: &[Step], prefix: &str) {
        for (i, step) in steps.iter().enumerate() {
            let index = format!("{prefix}{}", i + 1);
            self.step(step, &index);
        }
    }

    fn step(&mut self, step: &Step, index: &str) {
        let action = step.action();
        match step {
            Step::Send {
                command,
                message,
                expect,
                assert,
                save,
                ..
            } => {
                if let Some(name) = command.as_deref().filter(|n| !n.contains("${")) {
                    match self.config.command(name) {
                        None => self.err(
                            index,
                            action,
                            format!("no command `{name}` in project.toml"),
                        ),
                        Some(cmd) => {
                            if expect.is_none() && cmd.expect.is_none() && !assert.is_empty() {
                                self.check_assert_paths(index, action, None, assert);
                            }
                        }
                    }
                }
                if let (Some(m), Some(codec)) = (message, self.codec) {
                    if let Err(e) = codec.resolve_tx(m) {
                        self.err(index, action, e.to_string());
                    }
                }
                // The reply type: the step's `expect`, else the command's.
                let reply = expect.clone().or_else(|| {
                    command
                        .as_deref()
                        .and_then(|n| self.config.command(n))
                        .and_then(|c| c.expect.clone())
                });
                match (&reply, self.codec) {
                    (Some(r), Some(codec)) => match codec.resolve_rx(r) {
                        Ok(t) => {
                            self.check_assert_paths(index, action, Some(&t.root), assert);
                            for (var, p) in save {
                                match p.as_str() {
                                    Some(p) => self.check_path(index, action, &t.root, p, "save"),
                                    None => self.err(
                                        index,
                                        action,
                                        format!("save.{var} must be a field path string"),
                                    ),
                                }
                            }
                        }
                        Err(e) => self.err(index, action, e.to_string()),
                    },
                    (None, _) if assert.iter().any(|a| a.path.is_some()) => self.err(
                        index,
                        action,
                        "field assertions need an expected reply (`expect`)".into(),
                    ),
                    (None, _) if !save.is_empty() => self.err(
                        index,
                        action,
                        "`save` needs an expected reply (`expect`)".into(),
                    ),
                    _ => {}
                }
                self.check_assert_channels(index, action, assert, false);
            }
            Step::Expect {
                message, assert, ..
            } => {
                if let Some(codec) = self.codec {
                    match codec.resolve_rx(message) {
                        Ok(t) => self.check_assert_paths(index, action, Some(&t.root), assert),
                        Err(e) => self.err(index, action, e.to_string()),
                    }
                }
                self.check_assert_channels(index, action, assert, false);
            }
            Step::Capture {
                channels, assert, ..
            } => {
                for ch in channels {
                    if !channel_known(self.config, ch) {
                        self.err(index, action, format!("unknown channel `{ch}`"));
                    }
                }
                for a in assert {
                    if a.path.is_some() {
                        self.err(
                            index,
                            action,
                            "capture assertions use `channel` and `stat`, not `path`".into(),
                        );
                    }
                    if a.channel.is_none() {
                        self.err(index, action, "capture assertion needs `channel`".into());
                    }
                }
                self.check_assert_channels(index, action, assert, true);
            }
            Step::Loop { steps, .. } => {
                self.steps(steps, &format!("{index}."));
            }
            Step::Script { code, .. } => {
                if let Err((msg, line)) =
                    crate::sequence::script::compile_check(code.as_deref().unwrap_or(""))
                {
                    let at = line
                        .map(|l| format!("script line {l}: "))
                        .unwrap_or_default();
                    self.err(index, action, format!("{at}{msg}"));
                }
            }
            _ => {}
        }
    }

    fn check_path(
        &mut self,
        index: &str,
        action: &str,
        root: &prost_reflect::MessageDescriptor,
        path: &str,
        what: &str,
    ) {
        match FieldPath::parse(path) {
            Ok(p) => {
                if let Err(e) = resolve_field(root, &p) {
                    self.err(index, action, format!("{what}: {e}"));
                }
            }
            Err(e) => self.err(index, action, format!("{what}: bad path `{path}`: {e}")),
        }
    }

    fn check_assert_paths(
        &mut self,
        index: &str,
        action: &str,
        root: Option<&prost_reflect::MessageDescriptor>,
        asserts: &[Assertion],
    ) {
        for a in asserts {
            let Some(path) = &a.path else { continue };
            match root {
                Some(r) => self.check_path(index, action, r, path, "assert"),
                None => self.err(
                    index,
                    action,
                    "field assertions need an expected reply (`expect`)".into(),
                ),
            }
        }
        for a in asserts {
            if a.eq.is_none() && a.ne.is_none() && a.min.is_none() && a.max.is_none() {
                self.err(
                    index,
                    action,
                    "assertion has no condition (eq, ne, min, max)".into(),
                );
            }
        }
    }

    fn check_assert_channels(
        &mut self,
        index: &str,
        action: &str,
        asserts: &[Assertion],
        capture: bool,
    ) {
        for a in asserts {
            if let Some(ch) = &a.channel {
                if !channel_known(self.config, ch) {
                    self.err(index, action, format!("unknown channel `{ch}`"));
                }
            }
            if let Some(stat) = &a.stat {
                if !capture {
                    self.warn(index, action, "`stat` is only used by capture steps".into());
                } else if StatsSnapshot::default().get(stat).is_none() {
                    self.err(
                        index,
                        action,
                        format!(
                            "unknown stat `{stat}` (mean, min, max, std, rate_hz, count, last)"
                        ),
                    );
                }
            }
        }
    }

    /// Warns about `${name}` references that nothing defines.
    fn vars(&mut self, src: &str, seq: &SequenceFile) {
        let mut defined: BTreeSet<String> = seq.vars.keys().cloned().collect();
        collect_defined(&seq.steps, &mut defined);
        let mut seen = BTreeSet::new();
        let mut rest = src;
        let mut offset = 0;
        while let Some(start) = rest.find("${") {
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else { break };
            let name = after[..end].trim().to_string();
            let at = offset + start;
            if !defined.contains(&name) && seen.insert(name.clone()) {
                self.out.push(Diagnostic::warning(
                    self.path,
                    Some(line_col(src, at).0),
                    format!("variable `{name}` is never defined ([vars], set or save)"),
                ));
            }
            offset += start + 2 + end;
            rest = &after[end..];
        }
    }
}

fn collect_defined(steps: &[Step], out: &mut BTreeSet<String>) {
    for s in steps {
        match s {
            Step::Set { var, vars, .. } => {
                out.extend(var.iter().cloned());
                out.extend(vars.keys().cloned());
            }
            Step::Send { save, .. } => out.extend(save.keys().cloned()),
            Step::Loop { steps, .. } => collect_defined(steps, out),
            Step::Script { code: Some(c), .. } => out.extend(script_set_vars(c)),
            _ => {}
        }
    }
}

/// Names a script sets with `set_var("name", …)`.
fn script_set_vars(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = code;
    while let Some(i) = rest.find("set_var(") {
        rest = &rest[i + "set_var(".len()..];
        let r = rest.trim_start();
        if let Some(q) = r.chars().next().filter(|c| *c == '"' || *c == '\'') {
            if let Some(end) = r[1..].find(q) {
                out.push(r[1..1 + end].to_string());
            }
        }
    }
    out
}

/// True if `name` is a channel the project can produce: a telemetry channel
/// (with `[*]` or an unindexed repeated field expanded) or a derived channel.
pub fn channel_known(config: &ProjectConfig, name: &str) -> bool {
    config
        .telemetry
        .iter()
        .flat_map(|t| t.channels.iter())
        .any(|p| channel_matches(p, name))
        || config.derived.iter().any(|d| d.name == name)
}

/// Matches a configured channel path against a concrete channel name:
/// `[*]` matches any index, and trailing indices are allowed for a path
/// that names a repeated field without one.
pub fn channel_matches(pattern: &str, name: &str) -> bool {
    let mut rest = name;
    let mut parts = pattern.split("[*]").peekable();
    while let Some(part) = parts.next() {
        let Some(r) = rest.strip_prefix(part) else {
            return false;
        };
        rest = r;
        if parts.peek().is_some() {
            match strip_index(rest) {
                Some(r) => rest = r,
                None => return false,
            }
        }
    }
    while !rest.is_empty() {
        match strip_index(rest) {
            Some(r) => rest = r,
            None => return false,
        }
    }
    true
}

/// Strips a leading `[digits]`.
fn strip_index(s: &str) -> Option<&str> {
    let inner = s.strip_prefix('[')?;
    let end = inner.find(']')?;
    let digits = &inner[..end];
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then(|| &inner[end + 1..])
}

/// Checks `layout.json`: it must parse, and every channel it names should be
/// one the project produces (unknown channels are warnings: the panel just
/// stays empty).
pub fn check_layout_file(path: &Path, config: &ProjectConfig) -> Vec<Diagnostic> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return vec![Diagnostic::error(path, None, format!("cannot read: {e}"))],
    };
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(j) => j,
        Err(e) => {
            let mut d = Diagnostic::error(path, Some(e.line()), format!("invalid JSON: {e}"));
            d.column = Some(e.column());
            return vec![d];
        }
    };
    let mut names = BTreeSet::new();
    collect_layout_channels(&json, false, &mut names);
    names
        .into_iter()
        .filter(|n| !channel_known(config, n))
        .map(|n| {
            let line = text.find(&format!("\"{n}\"")).map(|o| line_col(&text, o).0);
            Diagnostic::warning(path, line, format!("layout uses unknown channel `{n}`"))
        })
        .collect()
}

/// Channel names inside panels: `channel` and `channels` keys, and the `x`
/// and `y` channels of XY plots.
fn collect_layout_channels(v: &serde_json::Value, in_panel: bool, out: &mut BTreeSet<String>) {
    match v {
        serde_json::Value::Object(map) => {
            let panel = in_panel || map.contains_key("type");
            for (k, val) in map {
                match (k.as_str(), val) {
                    ("channel", serde_json::Value::String(s)) if panel => {
                        out.insert(s.clone());
                    }
                    ("x" | "y", serde_json::Value::String(s)) if panel => {
                        out.insert(s.clone());
                    }
                    ("channels", serde_json::Value::Array(a)) if panel => {
                        out.extend(a.iter().filter_map(|x| x.as_str().map(str::to_string)));
                    }
                    (_, val) => collect_layout_channels(val, panel || k == "panel", out),
                }
            }
        }
        serde_json::Value::Array(a) => {
            for x in a {
                collect_layout_channels(x, in_panel, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> Project {
        Project::open(&crate::schema::tests::demo_dir())
    }

    #[test]
    fn demo_project_is_clean() {
        let p = demo();
        let d = check_project(&p);
        assert!(d.iter().all(|d| !d.is_error()), "{d:#?}");
    }

    #[test]
    fn channel_patterns() {
        assert!(channel_matches("telemetry.adc[*]", "telemetry.adc[3]"));
        assert!(channel_matches("telemetry.adc", "telemetry.adc[0]"));
        assert!(channel_matches("telemetry.vout_raw", "telemetry.vout_raw"));
        assert!(!channel_matches("telemetry.vout_raw", "telemetry.vout"));
        assert!(!channel_matches("telemetry.adc[*]", "telemetry.adc"));
        assert!(!channel_matches("telemetry.adc[*]", "telemetry.adc[x]"));
        assert!(channel_matches("a[*].b[*]", "a[1].b[22]"));
        let p = demo();
        assert!(channel_known(&p.config, "power_uw"));
        assert!(channel_known(&p.config, "status.battery_mv"));
        assert!(!channel_known(&p.config, "telemetry.nope"));
    }

    #[test]
    fn broken_sequence_names_file_line_and_step() {
        let p = demo();
        let codec = Codec::new(&p.config, p.schema.as_ref().unwrap(), Role::Host).unwrap();
        let src = r#"name = "broken"

[[steps]]
action = "send"
command = "Pnig"

[[steps]]
action = "send"
message = "set_bias"
payload = { millivolts = "${bias}" }
expect = "ack"
assert = [ { path = "ack.okay", eq = true } ]

[[steps]]
action = "capture"
channels = ["telemetry.vout"]
duration_ms = 10
assert = [ { channel = "telemetry.current_na", stat = "median", min = 1 } ]

[[steps]]
action = "loop"
count = 2
steps = [ { action = "expect", message = "teleport" } ]
"#;
        let d = check_sequence_str(src, Path::new("seq/broken.toml"), &p.config, Some(&codec));
        let text: Vec<String> = d.iter().map(|d| d.to_string()).collect();
        let has = |needle: &str, line: usize| {
            text.iter()
                .any(|t| t.contains(needle) && t.starts_with(&format!("seq/broken.toml:{line}:")))
        };
        assert!(has("step 1 (send): no command `Pnig`", 3), "{text:#?}");
        assert!(has("step 2 (send): assert: `ack.okay`", 7), "{text:#?}");
        assert!(
            has("step 3 (capture): unknown channel `telemetry.vout`", 14),
            "{text:#?}"
        );
        assert!(has("unknown stat `median`", 14), "{text:#?}");
        assert!(
            text.iter()
                .any(|t| t.contains("step 4.1 (expect)") && t.contains("teleport")),
            "{text:#?}"
        );
        assert!(
            has("warning: variable `bias` is never defined", 10),
            "{text:#?}"
        );
        assert_eq!(d.iter().filter(|d| d.is_error()).count(), 5, "{text:#?}");
    }

    #[test]
    fn script_steps_are_compiled_and_define_vars() {
        let p = demo();
        let src = r#"
[[steps]]
action = "script"
code = """
set_var("gain", 2);
"""

[[steps]]
action = "log"
text = "gain=${gain}"

[[steps]]
action = "script"
code = "let x = ;"
"#;
        let d = check_sequence_str(src, Path::new("s.toml"), &p.config, None);
        let text: Vec<String> = d.iter().map(|d| d.to_string()).collect();
        assert_eq!(d.len(), 1, "{text:#?}");
        assert!(
            text[0].starts_with("s.toml:12: error: step 3 (script): script line 1:"),
            "{text:#?}"
        );
    }

    #[test]
    fn parse_errors_and_layout_findings() {
        let p = demo();
        let d = check_sequence_str(
            "[[steps]]\naction = \"warp\"\n",
            Path::new("s.toml"),
            &p.config,
            None,
        );
        assert_eq!(d.len(), 1);
        assert!(d[0].is_error() && d[0].line.is_some(), "{d:?}");

        let dir = std::env::temp_dir().join(format!("pb_check_layout_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("layout.json");
        std::fs::write(&f, "{\n  \"x\": [1,\n}").unwrap();
        let d = check_layout_file(&f, &p.config);
        assert!(d[0].is_error() && d[0].line == Some(3), "{d:?}");
        std::fs::write(
            &f,
            r#"{"tabs": [{"panel": {"type": "time_plot", "traces": [{"channel": "telemetry.vout_raw"}, {"channel": "ghost"}]}}]}"#,
        )
        .unwrap();
        let d = check_layout_file(&f, &p.config);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(!d[0].is_error() && d[0].message.contains("ghost"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
