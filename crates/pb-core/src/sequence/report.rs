//! Run reports: JSON (full detail) and JUnit XML (one testcase per step).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value as Json;

use super::assert::AssertionResult;
use crate::error::CoreError;

/// Step result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    /// Passed.
    Pass,
    /// An assertion or expectation failed.
    Fail,
    /// The step could not run (bad config, disconnected).
    Error,
    /// Not run (after a failure or an abort).
    Skipped,
}

impl StepStatus {
    /// Label.
    pub fn as_str(self) -> &'static str {
        match self {
            StepStatus::Pass => "pass",
            StepStatus::Fail => "fail",
            StepStatus::Error => "error",
            StepStatus::Skipped => "skipped",
        }
    }
}

/// Report for one executed step.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StepReport {
    /// Position, e.g. `7` or `7.2.1` (loop 7, iteration 2, step 1).
    pub index: String,
    /// Action name.
    pub action: String,
    /// Human-readable label.
    pub label: String,
    /// Result.
    pub status: StepStatus,
    /// Wall time.
    pub duration_ms: f64,
    /// Explanation for failures and errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Measured values (responses, statistics, latency).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub measured: BTreeMap<String, Json>,
    /// Assertion results.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<AssertionResult>,
}

/// Report for a sequence run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunReport {
    /// Sequence name.
    pub sequence: String,
    /// Sequence description.
    pub description: String,
    /// Project name.
    pub project: String,
    /// Connection endpoint.
    pub endpoint: String,
    /// ProtoBench version.
    pub tool_version: String,
    /// Start time (RFC 3339).
    pub started: String,
    /// Total duration.
    pub duration_s: f64,
    /// True if every executed step passed and nothing was aborted.
    pub passed: bool,
    /// True if the run was aborted.
    pub aborted: bool,
    /// Counts.
    pub summary: Summary,
    /// Steps in execution order.
    pub steps: Vec<StepReport>,
    /// Variables at the end of the run.
    pub vars: BTreeMap<String, Json>,
    /// Log lines.
    pub log: Vec<String>,
}

/// Step counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Steps reported.
    pub total: usize,
    /// Passed.
    pub passed: usize,
    /// Failed.
    pub failed: usize,
    /// Errored.
    pub errors: usize,
    /// Skipped.
    pub skipped: usize,
}

impl RunReport {
    /// Recomputes `summary` and `passed` from the steps.
    pub fn finalize(&mut self) {
        let mut s = Summary::default();
        for st in &self.steps {
            s.total += 1;
            match st.status {
                StepStatus::Pass => s.passed += 1,
                StepStatus::Fail => s.failed += 1,
                StepStatus::Error => s.errors += 1,
                StepStatus::Skipped => s.skipped += 1,
            }
        }
        self.summary = s;
        self.passed = s.failed == 0 && s.errors == 0 && !self.aborted;
    }

    /// Pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// JUnit XML: one `<testsuite>` with a `<testcase>` per step.
    pub fn to_junit(&self) -> String {
        let s = &self.summary;
        let mut x = String::new();
        x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        x.push_str(&format!(
            "<testsuites name=\"protobench\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\">\n",
            s.total, s.failed, s.errors, s.skipped, self.duration_s
        ));
        x.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\" timestamp=\"{}\">\n",
            esc(&self.sequence), s.total, s.failed, s.errors, s.skipped, self.duration_s, esc(&self.started)
        ));
        x.push_str("    <properties>\n");
        for (k, v) in [
            ("project", self.project.as_str()),
            ("endpoint", self.endpoint.as_str()),
            ("tool_version", self.tool_version.as_str()),
        ] {
            x.push_str(&format!(
                "      <property name=\"{k}\" value=\"{}\"/>\n",
                esc(v)
            ));
        }
        x.push_str("    </properties>\n");
        let classname = format!("protobench.{}", class_ident(&self.sequence));
        for st in &self.steps {
            let name = format!("{} {}", st.index, st.label);
            let time = st.duration_ms / 1000.0;
            x.push_str(&format!(
                "    <testcase name=\"{}\" classname=\"{}\" time=\"{time:.3}\"",
                esc(&name),
                esc(&classname)
            ));
            let details = details(st);
            match st.status {
                StepStatus::Pass if details.is_empty() => x.push_str("/>\n"),
                StepStatus::Pass => {
                    x.push_str(">\n");
                    x.push_str(&format!(
                        "      <system-out>{}</system-out>\n",
                        esc(&details)
                    ));
                    x.push_str("    </testcase>\n");
                }
                StepStatus::Fail | StepStatus::Error => {
                    let tag = if st.status == StepStatus::Fail {
                        "failure"
                    } else {
                        "error"
                    };
                    x.push_str(">\n");
                    x.push_str(&format!(
                        "      <{tag} message=\"{}\" type=\"{}\">{}</{tag}>\n",
                        esc(st.message.as_deref().unwrap_or(st.status.as_str())),
                        st.action,
                        esc(&details)
                    ));
                    x.push_str("    </testcase>\n");
                }
                StepStatus::Skipped => {
                    x.push_str(">\n");
                    x.push_str(&format!(
                        "      <skipped message=\"{}\"/>\n",
                        esc(st.message.as_deref().unwrap_or("skipped"))
                    ));
                    x.push_str("    </testcase>\n");
                }
            }
        }
        x.push_str("  </testsuite>\n</testsuites>\n");
        x
    }

    /// Writes `<dir>/<sequence>_<timestamp>.json` and `.xml`. Returns both paths.
    pub fn write(&self, dir: &Path, file_stem: &str) -> Result<(PathBuf, PathBuf), CoreError> {
        std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let base = format!("{}_{stamp}", class_ident(file_stem));
        let json = dir.join(format!("{base}.json"));
        let xml = dir.join(format!("{base}.xml"));
        std::fs::write(&json, self.to_json()).map_err(|e| CoreError::io(&json, e))?;
        std::fs::write(&xml, self.to_junit()).map_err(|e| CoreError::io(&xml, e))?;
        Ok((json, xml))
    }

    /// Plain-text summary for terminals.
    pub fn summary_text(&self) -> String {
        let mut out = String::new();
        for st in &self.steps {
            let mark = match st.status {
                StepStatus::Pass => "PASS",
                StepStatus::Fail => "FAIL",
                StepStatus::Error => "ERR ",
                StepStatus::Skipped => "SKIP",
            };
            out.push_str(&format!(
                "{mark} {:>7} {} ({:.0} ms)",
                st.index, st.label, st.duration_ms
            ));
            if let Some(m) = &st.message {
                out.push_str(&format!(" — {m}"));
            }
            out.push('\n');
            for a in &st.assertions {
                out.push_str(&format!(
                    "     {} {} = {}\n",
                    if a.passed { "✓" } else { "✗" },
                    a.description,
                    a.measured
                ));
            }
        }
        let s = &self.summary;
        out.push_str(&format!(
            "{}: {} passed, {} failed, {} errors, {} skipped in {:.2} s{}\n",
            if self.passed { "PASSED" } else { "FAILED" },
            s.passed,
            s.failed,
            s.errors,
            s.skipped,
            self.duration_s,
            if self.aborted { " (aborted)" } else { "" }
        ));
        out
    }
}

fn details(st: &StepReport) -> String {
    let mut lines = Vec::new();
    for a in &st.assertions {
        lines.push(format!(
            "{} {} (measured {}){}",
            if a.passed { "PASS" } else { "FAIL" },
            a.description,
            a.measured,
            a.message
                .as_ref()
                .map(|m| format!(": {m}"))
                .unwrap_or_default()
        ));
    }
    for (k, v) in &st.measured {
        lines.push(format!("{k} = {v}"));
    }
    lines.join("\n")
}

fn class_ident(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Escapes XML special characters and drops characters XML 1.0 forbids.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> RunReport {
        let mut r = RunReport {
            sequence: "Smoke <1>".into(),
            description: String::new(),
            project: "Demo".into(),
            endpoint: "sim".into(),
            tool_version: "0.1.0".into(),
            started: "2026-01-01T00:00:00Z".into(),
            duration_s: 1.5,
            passed: false,
            aborted: false,
            summary: Summary::default(),
            steps: vec![
                StepReport {
                    index: "1".into(),
                    action: "send".into(),
                    label: "send Ping".into(),
                    status: StepStatus::Pass,
                    duration_ms: 3.0,
                    message: None,
                    measured: BTreeMap::new(),
                    assertions: vec![],
                },
                StepReport {
                    index: "2".into(),
                    action: "capture".into(),
                    label: "capture & check".into(),
                    status: StepStatus::Fail,
                    duration_ms: 2000.0,
                    message: Some("1 assertion failed".into()),
                    measured: BTreeMap::new(),
                    assertions: vec![AssertionResult {
                        description: "x mean <= 1".into(),
                        measured: serde_json::json!(2.0),
                        passed: false,
                        message: Some("above maximum 1".into()),
                    }],
                },
                StepReport {
                    index: "3".into(),
                    action: "log".into(),
                    label: "log".into(),
                    status: StepStatus::Skipped,
                    duration_ms: 0.0,
                    message: None,
                    measured: BTreeMap::new(),
                    assertions: vec![],
                },
            ],
            vars: BTreeMap::new(),
            log: vec![],
        };
        r.finalize();
        r
    }

    #[test]
    fn summary_counts() {
        let r = report();
        assert!(!r.passed);
        assert_eq!(
            r.summary,
            Summary {
                total: 3,
                passed: 1,
                failed: 1,
                errors: 0,
                skipped: 1
            }
        );
        let j: serde_json::Value = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(j["steps"][1]["status"], "fail");
        assert_eq!(j["steps"][1]["assertions"][0]["passed"], false);
        assert!(r.summary_text().contains("FAILED: 1 passed, 1 failed"));
    }

    #[test]
    fn junit_structure() {
        let x = report().to_junit();
        assert!(x.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites "));
        assert!(x.contains("<testsuite name=\"Smoke &lt;1&gt;\" tests=\"3\" failures=\"1\" errors=\"0\" skipped=\"1\""));
        assert_eq!(x.matches("<testcase ").count(), 3);
        assert!(x.contains(
            "<testcase name=\"1 send Ping\" classname=\"protobench.Smoke__1_\" time=\"0.003\"/>"
        ));
        assert!(x.contains(
            "<failure message=\"1 assertion failed\" type=\"capture\">FAIL x mean &lt;= 1"
        ));
        assert!(x.contains("<skipped message=\"skipped\"/>"));
        assert!(x.trim_end().ends_with("</testsuites>"));
        // Balanced tags.
        for tag in ["testsuites", "testsuite", "properties"] {
            assert_eq!(
                x.matches(&format!("<{tag}")).count() - if tag == "testsuite" { 1 } else { 0 },
                x.matches(&format!("</{tag}>")).count(),
                "{tag}"
            );
        }
        assert_eq!(x.matches("</testcase>").count(), 2, "pass is self-closing");
        assert_eq!(esc("a\u{1}b&"), "ab&amp;");
    }
}
