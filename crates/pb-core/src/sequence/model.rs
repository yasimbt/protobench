//! Sequence file model (`sequences/<name>.toml`).

use std::path::Path;

use serde::Deserialize;

use crate::diag::{line_col, Diagnostic};
use crate::error::CoreError;

/// How `prompt_operator` behaves when nobody can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NonInteractive {
    /// The step fails.
    Fail,
    /// The prompt is logged and the step passes.
    Continue,
}

/// A sequence file.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SequenceFile {
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Description.
    #[serde(default)]
    pub description: String,
    /// Initial variables.
    #[serde(default)]
    pub vars: toml::Table,
    /// Default for headless `prompt_operator` (the CLI flag overrides it).
    #[serde(default)]
    pub non_interactive: Option<NonInteractive>,
    /// Keep running after a failed step (default: skip the rest).
    #[serde(default)]
    pub continue_on_failure: bool,
    /// Steps.
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// One assertion.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Assertion {
    /// Field path in the response (send/expect).
    pub path: Option<String>,
    /// Channel name (capture, or the latest value elsewhere).
    pub channel: Option<String>,
    /// Statistic for capture: mean, min, max, std, rate_hz, count, last.
    pub stat: Option<String>,
    /// Must equal (numbers, bools, strings, enum names).
    pub eq: Option<toml::Value>,
    /// Must not equal.
    pub ne: Option<toml::Value>,
    /// Lower bound (inclusive).
    pub min: Option<f64>,
    /// Upper bound (inclusive).
    pub max: Option<f64>,
    /// With `eq`: allowed absolute tolerance.
    pub within: Option<f64>,
}

/// A step.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// Send a command (by name) or an inline message.
    Send {
        /// Command name from project.toml.
        command: Option<String>,
        /// Inline message (oneof member or type).
        message: Option<String>,
        /// Inline payload, or overrides for a named command.
        payload: Option<toml::Table>,
        /// Expected reply (overrides the command's).
        expect: Option<String>,
        /// Timeout override.
        timeout_ms: Option<u64>,
        /// Retry override.
        retries: Option<u32>,
        /// Assertions on the reply.
        #[serde(default)]
        assert: Vec<Assertion>,
        /// Store reply fields in variables: `{ fw = "pong.firmware" }`.
        #[serde(default)]
        save: toml::Table,
    },
    /// Pause.
    Wait {
        /// Milliseconds.
        ms: u64,
    },
    /// Wait for a received message.
    Expect {
        /// Message (oneof member or type).
        message: String,
        /// How long to wait.
        #[serde(default = "default_expect_timeout")]
        timeout_ms: u64,
        /// Assertions on the message.
        #[serde(default)]
        assert: Vec<Assertion>,
    },
    /// Collect channel samples, then check statistics.
    Capture {
        /// Channels to capture.
        channels: Vec<String>,
        /// Capture for this long.
        duration_ms: Option<u64>,
        /// Or until the first channel has this many new samples.
        samples: Option<usize>,
        /// Upper bound when capturing by sample count.
        #[serde(default = "default_capture_timeout")]
        timeout_ms: u64,
        /// Assertions (`channel` + `stat`).
        #[serde(default)]
        assert: Vec<Assertion>,
    },
    /// Set variables: `var` + `value`, or a `vars` table.
    Set {
        /// Variable name.
        var: Option<String>,
        /// Value.
        value: Option<toml::Value>,
        /// Several at once.
        #[serde(default)]
        vars: toml::Table,
    },
    /// Repeat steps.
    Loop {
        /// Iterations.
        count: u32,
        /// Steps.
        steps: Vec<Step>,
    },
    /// Write to the run log.
    Log {
        /// Text (`${var}` is substituted).
        text: String,
    },
    /// Ask the operator to do something and confirm.
    PromptOperator {
        /// Instruction.
        message: String,
    },
}

fn default_expect_timeout() -> u64 {
    1000
}

fn default_capture_timeout() -> u64 {
    10_000
}

impl Step {
    /// Action name.
    pub fn action(&self) -> &'static str {
        match self {
            Step::Send { .. } => "send",
            Step::Wait { .. } => "wait",
            Step::Expect { .. } => "expect",
            Step::Capture { .. } => "capture",
            Step::Set { .. } => "set",
            Step::Loop { .. } => "loop",
            Step::Log { .. } => "log",
            Step::PromptOperator { .. } => "prompt_operator",
        }
    }

    /// Short human-readable label.
    pub fn label(&self) -> String {
        match self {
            Step::Send {
                command, message, ..
            } => format!(
                "send {}",
                command.as_deref().or(message.as_deref()).unwrap_or("?")
            ),
            Step::Wait { ms } => format!("wait {ms} ms"),
            Step::Expect { message, .. } => format!("expect {message}"),
            Step::Capture {
                channels,
                duration_ms,
                samples,
                ..
            } => match (duration_ms, samples) {
                (Some(d), _) => format!("capture {} ch for {d} ms", channels.len()),
                (None, Some(n)) => format!("capture {n} samples"),
                _ => "capture".into(),
            },
            Step::Set { var, vars, .. } => match var {
                Some(v) => format!("set {v}"),
                None => format!(
                    "set {}",
                    vars.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            },
            Step::Loop { count, steps } => format!("loop {count}× ({} steps)", steps.len()),
            Step::Log { text } => format!("log {text}"),
            Step::PromptOperator { message } => format!("prompt: {message}"),
        }
    }
}

impl SequenceFile {
    /// Parses sequence TOML. Errors carry the file and line.
    pub fn parse(src: &str, file: &Path) -> Result<SequenceFile, Diagnostic> {
        let mut seq: SequenceFile = toml::from_str(src).map_err(|e| {
            let line = e.span().map(|s| line_col(src, s.start).0);
            Diagnostic::error(file, line, e.message().trim().to_string())
        })?;
        if seq.name.is_empty() {
            seq.name = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        validate_steps(&seq.steps).map_err(|m| Diagnostic::error(file, None, m))?;
        Ok(seq)
    }

    /// Reads and parses a sequence file.
    pub fn load(path: &Path) -> Result<SequenceFile, CoreError> {
        let src = std::fs::read_to_string(path).map_err(|e| CoreError::io(path, e))?;
        Self::parse(&src, path).map_err(|d| CoreError::Sequence(d.to_string()))
    }
}

fn validate_steps(steps: &[Step]) -> Result<(), String> {
    for (i, s) in steps.iter().enumerate() {
        match s {
            Step::Send {
                command, message, ..
            } => {
                if command.is_some() == message.is_some() {
                    return Err(format!(
                        "step {}: send needs exactly one of `command` or `message`",
                        i + 1
                    ));
                }
            }
            Step::Capture {
                channels,
                duration_ms,
                samples,
                ..
            } => {
                if channels.is_empty() {
                    return Err(format!("step {}: capture needs `channels`", i + 1));
                }
                if duration_ms.is_none() && samples.is_none() {
                    return Err(format!(
                        "step {}: capture needs `duration_ms` or `samples`",
                        i + 1
                    ));
                }
            }
            Step::Set { var, value, vars } => {
                if var.is_some() != value.is_some() || (var.is_none() && vars.is_empty()) {
                    return Err(format!(
                        "step {}: set needs `var` and `value`, or `vars`",
                        i + 1
                    ));
                }
            }
            Step::Loop { steps, .. } => {
                validate_steps(steps).map_err(|e| format!("in loop at step {}: {e}", i + 1))?
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_example_parses() {
        let path = crate::schema::tests::demo_dir().join("sequences/smoke.toml");
        let s = SequenceFile::load(&path).unwrap();
        assert_eq!(s.name, "Bring-up smoke test");
        assert_eq!(s.steps.len(), 8);
        assert!(matches!(&s.steps[6], Step::Loop { count: 3, steps } if steps.len() == 1));
        assert_eq!(s.non_interactive, Some(NonInteractive::Continue));
        assert_eq!(s.steps[3].label(), "capture 2 ch for 2000 ms");
    }

    #[test]
    fn errors_have_lines() {
        let src = "name = \"x\"\n[[steps]]\naction = \"send\"\ncomand = \"Ping\"\n";
        let e = SequenceFile::parse(src, Path::new("s.toml")).unwrap_err();
        assert!(e.message.contains("comand"), "{e}");
        assert!(e.line.is_some(), "{e}");
        let src = "[[steps]]\naction = \"teleport\"\n";
        assert!(SequenceFile::parse(src, Path::new("s.toml")).is_err());
        let src = "[[steps]]\naction = \"send\"\n";
        let e = SequenceFile::parse(src, Path::new("s.toml")).unwrap_err();
        assert!(e.message.contains("exactly one"), "{e}");
    }
}
