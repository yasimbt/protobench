//! Diagnostics: errors and warnings that point at a file and line.

use std::fmt;
use std::path::PathBuf;

/// How serious a diagnostic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Prevents the project from being used fully.
    Error,
    /// Worth showing, but the project still works.
    Warning,
}

/// A problem found while loading a project, schema or sequence.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Diagnostic {
    /// Error or warning.
    pub severity: Severity,
    /// File the problem is in.
    pub file: PathBuf,
    /// 1-based line, when known.
    pub line: Option<usize>,
    /// 1-based column, when known.
    pub column: Option<usize>,
    /// Human-readable message.
    pub message: String,
}

impl Diagnostic {
    /// Creates an error.
    pub fn error(
        file: impl Into<PathBuf>,
        line: Option<usize>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Error,
            file: file.into(),
            line,
            column: None,
            message: message.into(),
        }
    }

    /// Creates a warning.
    pub fn warning(
        file: impl Into<PathBuf>,
        line: Option<usize>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(file, line, message)
        }
    }

    /// True for errors.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.file.display())?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
            if let Some(col) = self.column {
                write!(f, ":{col}")?;
            }
        }
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, ": {sev}: {}", self.message)
    }
}

/// Converts a byte offset into a 1-based (line, column).
pub fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(src.len());
    let before = &src.as_bytes()[..offset];
    let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
    let col = before.iter().rev().take_while(|&&b| b != b'\n').count() + 1;
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_counts_from_one() {
        let s = "a\nbc\nd";
        assert_eq!(line_col(s, 0), (1, 1));
        assert_eq!(line_col(s, 3), (2, 2));
        assert_eq!(line_col(s, 5), (3, 1));
        assert_eq!(line_col(s, 999), (3, 2));
    }

    #[test]
    fn display_includes_location() {
        let d = Diagnostic::error("p.toml", Some(3), "bad");
        assert_eq!(d.to_string(), "p.toml:3: error: bad");
    }
}
