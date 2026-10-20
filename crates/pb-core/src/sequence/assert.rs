//! Assertion evaluation.

use prost_reflect::{DynamicMessage, Value};
use serde_json::Value as Json;

use super::model::Assertion;
use crate::commands::toml_to_json;
use crate::schema::{extract, FieldPath};
use crate::telemetry::StatsSnapshot;

/// Result of one assertion.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AssertionResult {
    /// What was checked, e.g. `telemetry.current_na mean in [200, 300]`.
    pub description: String,
    /// The measured value.
    pub measured: Json,
    /// Pass or fail.
    pub passed: bool,
    /// Why it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A measured value to compare.
#[derive(Debug, Clone, PartialEq)]
pub enum Measured {
    /// A number.
    Num(f64),
    /// A bool.
    Bool(bool),
    /// A string.
    Str(String),
    /// An enum: number and name.
    Enum(i32, String),
}

impl Measured {
    fn to_json(&self) -> Json {
        match self {
            Measured::Num(v) => serde_json::Number::from_f64(*v)
                .map(Json::Number)
                .unwrap_or(Json::Null),
            Measured::Bool(b) => Json::Bool(*b),
            Measured::Str(s) => Json::String(s.clone()),
            Measured::Enum(_, n) => Json::String(n.clone()),
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Measured::Num(v) => Some(*v),
            Measured::Bool(b) => Some(f64::from(u8::from(*b))),
            Measured::Enum(n, _) => Some(f64::from(*n)),
            Measured::Str(s) => s.parse().ok(),
        }
    }

    fn equals(&self, expected: &Json, tol: Option<f64>) -> bool {
        match (self, expected) {
            (Measured::Enum(n, name), Json::String(s)) => name == s || n.to_string() == *s,
            (Measured::Str(a), Json::String(b)) => a == b,
            (Measured::Bool(a), Json::Bool(b)) => a == b,
            (m, Json::Number(n)) => match (m.as_f64(), n.as_f64()) {
                (Some(a), Some(b)) => (a - b).abs() <= tol.unwrap_or(1e-9 * b.abs().max(1.0)),
                _ => false,
            },
            (m, Json::Bool(b)) => m.as_f64() == Some(f64::from(u8::from(*b))),
            _ => false,
        }
    }
}

/// Reads a field from a message as a [`Measured`] value.
pub fn measure_field(msg: &DynamicMessage, path: &str) -> Result<Measured, String> {
    let p = FieldPath::parse(path)?;
    let e = extract(msg, &p)
        .into_iter()
        .next()
        .ok_or_else(|| format!("`{path}` is not present in the message"))?;
    Ok(match &e.value {
        Value::Bool(b) => Measured::Bool(*b),
        Value::String(s) => Measured::Str(s.clone()),
        Value::EnumNumber(n) => {
            let name = match e.field.kind() {
                prost_reflect::Kind::Enum(d) => d
                    .get_value(*n)
                    .map(|v| v.name().to_string())
                    .unwrap_or_else(|| n.to_string()),
                _ => n.to_string(),
            };
            Measured::Enum(*n, name)
        }
        v => match crate::schema::value_to_f64(v) {
            Some(x) => Measured::Num(x),
            None => return Err(format!("`{path}` is not a scalar")),
        },
    })
}

/// Checks an assertion against a measured value. `what` names the value.
pub fn check(a: &Assertion, what: &str, m: Result<Measured, String>) -> AssertionResult {
    let mut desc = what.to_string();
    let eq = a.eq.as_ref().map(toml_to_json);
    let ne = a.ne.as_ref().map(toml_to_json);
    if let Some(e) = &eq {
        match a.within {
            Some(t) => desc.push_str(&format!(" == {e} ± {t}")),
            None => desc.push_str(&format!(" == {e}")),
        }
    }
    if let Some(e) = &ne {
        desc.push_str(&format!(" != {e}"));
    }
    match (a.min, a.max) {
        (Some(lo), Some(hi)) => desc.push_str(&format!(" in [{lo}, {hi}]")),
        (Some(lo), None) => desc.push_str(&format!(" >= {lo}")),
        (None, Some(hi)) => desc.push_str(&format!(" <= {hi}")),
        (None, None) => {}
    }
    let m = match m {
        Ok(m) => m,
        Err(msg) => {
            return AssertionResult {
                description: desc,
                measured: Json::Null,
                passed: false,
                message: Some(msg),
            }
        }
    };
    let mut fails = Vec::new();
    if let Some(e) = &eq {
        if !m.equals(e, a.within) {
            fails.push(format!("expected {e}"));
        }
    }
    if let Some(e) = &ne {
        if m.equals(e, a.within) {
            fails.push(format!("expected not {e}"));
        }
    }
    if a.min.is_some() || a.max.is_some() {
        match m.as_f64() {
            Some(v) => {
                if a.min.is_some_and(|lo| v < lo) {
                    fails.push(format!("below minimum {}", a.min.unwrap_or_default()));
                }
                if a.max.is_some_and(|hi| v > hi) {
                    fails.push(format!("above maximum {}", a.max.unwrap_or_default()));
                }
            }
            None => fails.push("not a number".into()),
        }
    }
    if eq.is_none() && ne.is_none() && a.min.is_none() && a.max.is_none() {
        fails.push("assertion has no condition (eq, ne, min, max)".into());
    }
    let measured = m.to_json();
    AssertionResult {
        description: desc,
        passed: fails.is_empty(),
        message: (!fails.is_empty()).then(|| format!("measured {measured}: {}", fails.join(", "))),
        measured,
    }
}

/// Checks a capture assertion against per-channel statistics.
pub fn check_stat(a: &Assertion, stats: &[(String, Option<StatsSnapshot>)]) -> AssertionResult {
    let Some(ch) = &a.channel else {
        return check(a, "?", Err("capture assertion needs `channel`".into()));
    };
    let stat = a.stat.as_deref().unwrap_or("mean");
    let what = format!("{ch} {stat}");
    let m = match stats.iter().find(|(n, _)| n == ch) {
        None => Err(format!("channel `{ch}` was not captured")),
        Some((_, None)) => Err(format!("no samples on `{ch}`")),
        Some((_, Some(s))) => s.get(stat).map(Measured::Num).ok_or_else(|| {
            format!("unknown stat `{stat}` (mean, min, max, std, rate_hz, count, last)")
        }),
    };
    check(a, &what, m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(f: impl FnOnce(&mut Assertion)) -> Assertion {
        let mut x = Assertion::default();
        f(&mut x);
        x
    }

    #[test]
    fn numeric_and_tolerance() {
        let r = check(
            &a(|x| {
                x.min = Some(200.0);
                x.max = Some(300.0)
            }),
            "v",
            Ok(Measured::Num(250.0)),
        );
        assert!(r.passed);
        assert_eq!(r.description, "v in [200, 300]");
        let r = check(&a(|x| x.max = Some(10.0)), "v", Ok(Measured::Num(11.0)));
        assert!(!r.passed);
        assert!(r.message.unwrap().contains("above maximum 10"));
        let r = check(
            &a(|x| {
                x.eq = Some(toml::Value::Float(3.3));
                x.within = Some(0.1)
            }),
            "v",
            Ok(Measured::Num(3.25)),
        );
        assert!(r.passed, "{r:?}");
        let r = check(
            &a(|x| {
                x.eq = Some(toml::Value::Integer(5));
            }),
            "v",
            Ok(Measured::Num(5.0)),
        );
        assert!(r.passed);
        let r = check(
            &a(|x| {
                x.ne = Some(toml::Value::Integer(0));
            }),
            "v",
            Ok(Measured::Num(0.0)),
        );
        assert!(!r.passed);
    }

    #[test]
    fn bools_enums_strings_and_errors() {
        assert!(
            check(
                &a(|x| x.eq = Some(toml::Value::Boolean(true))),
                "ok",
                Ok(Measured::Bool(true))
            )
            .passed
        );
        assert!(
            !check(
                &a(|x| x.eq = Some(toml::Value::Boolean(true))),
                "ok",
                Ok(Measured::Bool(false))
            )
            .passed
        );
        let e = Measured::Enum(2, "MEASURING".into());
        assert!(
            check(
                &a(|x| x.eq = Some(toml::Value::String("MEASURING".into()))),
                "s",
                Ok(e.clone())
            )
            .passed
        );
        assert!(check(&a(|x| x.eq = Some(toml::Value::Integer(2))), "s", Ok(e)).passed);
        assert!(
            check(
                &a(|x| x.eq = Some(toml::Value::String("1.2".into()))),
                "fw",
                Ok(Measured::Str("1.2".into()))
            )
            .passed
        );
        let r = check(&a(|x| x.min = Some(1.0)), "x", Err("missing".into()));
        assert!(!r.passed);
        assert_eq!(r.message.as_deref(), Some("missing"));
        assert!(!check(&Assertion::default(), "x", Ok(Measured::Num(1.0))).passed);
    }

    #[test]
    fn capture_stats() {
        let snap = StatsSnapshot {
            count: 2000,
            mean: 250.0,
            std: 5.0,
            rate_hz: 1000.0,
            ..Default::default()
        };
        let stats = vec![("c".to_string(), Some(snap)), ("empty".to_string(), None)];
        let r = check_stat(
            &a(|x| {
                x.channel = Some("c".into());
                x.stat = Some("rate_hz".into());
                x.min = Some(900.0)
            }),
            &stats,
        );
        assert!(r.passed);
        assert_eq!(r.description, "c rate_hz >= 900");
        assert!(
            !check_stat(
                &a(|x| {
                    x.channel = Some("empty".into());
                    x.min = Some(1.0)
                }),
                &stats
            )
            .passed
        );
        assert!(
            !check_stat(
                &a(|x| {
                    x.channel = Some("zz".into());
                    x.min = Some(1.0)
                }),
                &stats
            )
            .passed
        );
        assert!(
            !check_stat(
                &a(|x| {
                    x.channel = Some("c".into());
                    x.stat = Some("median".into());
                    x.min = Some(1.0)
                }),
                &stats
            )
            .passed
        );
    }
}
