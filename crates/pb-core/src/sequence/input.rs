//! Operator inputs of `prompt_operator` steps: spec, parsing and validation.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::commands::toml_to_json;

/// Value type of an operator input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputType {
    /// A number (stored as a JSON float).
    #[default]
    Float,
    /// An integer.
    Int,
    /// Text.
    String,
    /// true / false.
    Bool,
}

/// One value `prompt_operator` asks for.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OperatorInput {
    /// Variable the value is stored in.
    pub var: String,
    /// Field label (default: `var`).
    pub label: Option<String>,
    /// Shown after the field.
    #[serde(default)]
    pub unit: String,
    /// Value type.
    #[serde(rename = "type", default)]
    pub ty: InputType,
    /// Pre-filled value (default: the variable's current value).
    pub default: Option<toml::Value>,
    /// Inclusive lower bound (float, int).
    pub min: Option<f64>,
    /// Inclusive upper bound (float, int).
    pub max: Option<f64>,
    /// Allowed values (string: shown as a dropdown).
    pub choices: Option<Vec<String>>,
}

impl OperatorInput {
    /// Field label.
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.var)
    }

    /// `default` as JSON.
    pub fn default_json(&self) -> Option<Json> {
        self.default.as_ref().map(toml_to_json)
    }

    /// Mistakes in the spec itself (for `protobench check`).
    pub fn spec_errors(&self) -> Vec<String> {
        let mut out = Vec::new();
        let numeric = matches!(self.ty, InputType::Float | InputType::Int);
        if let (Some(lo), Some(hi)) = (self.min, self.max) {
            if lo > hi {
                out.push(format!("input `{}`: min {lo} is above max {hi}", self.var));
            }
        }
        if !numeric && (self.min.is_some() || self.max.is_some()) {
            out.push(format!(
                "input `{}`: min/max only apply to float and int inputs",
                self.var
            ));
        }
        if self.choices.is_some() && self.ty != InputType::String {
            out.push(format!(
                "input `{}`: choices only apply to string inputs",
                self.var
            ));
        }
        if let Some(d) = self.default_json() {
            if let Err(e) = self.validate(&d) {
                out.push(format!("input `{}`: default: {e}", self.var));
            }
        }
        out
    }

    /// Checks a value from `--var`, `[vars]` or `default`, converting it to
    /// the input's type (an int for a float input becomes a float, a number
    /// for a string input becomes its text).
    pub fn validate(&self, v: &Json) -> Result<Json, String> {
        let v = match (self.ty, v) {
            (InputType::Float, Json::Number(n)) => match n.as_f64() {
                Some(f) => Json::from(f),
                None => return Err(format!("{n} is not a number")),
            },
            (InputType::Int, Json::Number(n)) => match n.as_i64().or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15)
                    .map(|f| f as i64)
            }) {
                Some(i) => Json::from(i),
                None => return Err(format!("{n} is not an integer")),
            },
            (InputType::String, Json::String(_)) => v.clone(),
            (InputType::String, Json::Number(_) | Json::Bool(_)) => Json::String(v.to_string()),
            (InputType::Bool, Json::Bool(_)) => v.clone(),
            (ty, other) => return Err(format!("{other} is not {}", type_name(ty))),
        };
        self.check_value(&v)?;
        Ok(v)
    }

    /// Parses what the operator typed.
    pub fn parse_text(&self, text: &str) -> Result<Json, String> {
        let t = text.trim();
        if t.is_empty() && self.ty != InputType::String {
            return Err("required".into());
        }
        let v = match self.ty {
            InputType::Float => match t.parse::<f64>() {
                Ok(f) if f.is_finite() => Json::from(f),
                _ => return Err("not a number".into()),
            },
            InputType::Int => match t.parse::<i64>() {
                Ok(i) => Json::from(i),
                Err(_) => return Err("not an integer".into()),
            },
            InputType::Bool => match t {
                "true" => Json::Bool(true),
                "false" => Json::Bool(false),
                _ => return Err("not true or false".into()),
            },
            InputType::String => Json::String(text.to_string()),
        };
        self.check_value(&v)?;
        Ok(v)
    }

    /// Text form of a value, for pre-filling a field.
    pub fn to_text(v: &Json) -> String {
        match v {
            Json::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    fn check_value(&self, v: &Json) -> Result<(), String> {
        if let Some(x) = v.as_f64() {
            if !x.is_finite() {
                return Err("not a finite number".into());
            }
            if let Some(lo) = self.min.filter(|lo| x < *lo) {
                return Err(format!("{x} is below min {lo}"));
            }
            if let Some(hi) = self.max.filter(|hi| x > *hi) {
                return Err(format!("{x} is above max {hi}"));
            }
        }
        if let (Some(c), Json::String(s)) = (&self.choices, v) {
            if !c.contains(s) {
                return Err(format!("`{s}` is not one of {}", c.join(", ")));
            }
        }
        Ok(())
    }
}

fn type_name(ty: InputType) -> &'static str {
    match ty {
        InputType::Float => "a number",
        InputType::Int => "an integer",
        InputType::String => "a string",
        InputType::Bool => "true or false",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(ty: InputType) -> OperatorInput {
        OperatorInput {
            var: "x".into(),
            label: None,
            unit: String::new(),
            ty,
            default: None,
            min: Some(1.0),
            max: Some(10.0),
            choices: None,
        }
    }

    #[test]
    fn validate_converts_and_checks_bounds() {
        let f = input(InputType::Float);
        assert_eq!(f.validate(&json!(5)).unwrap(), json!(5.0));
        assert!(f.validate(&json!(0.5)).unwrap_err().contains("below min 1"));
        assert!(f.validate(&json!(11)).unwrap_err().contains("above max 10"));
        assert!(f
            .validate(&json!("5"))
            .unwrap_err()
            .contains("not a number"));
        let i = input(InputType::Int);
        assert_eq!(i.validate(&json!(3.0)).unwrap(), json!(3));
        assert!(i.validate(&json!(3.5)).is_err());
        let mut s = input(InputType::String);
        s.min = None;
        s.max = None;
        assert_eq!(s.validate(&json!(1234)).unwrap(), json!("1234"));
        s.choices = Some(vec!["a".into(), "b".into()]);
        assert!(s.validate(&json!("c")).unwrap_err().contains("one of a, b"));
    }

    #[test]
    fn parse_text_per_type() {
        let f = input(InputType::Float);
        assert_eq!(f.parse_text(" 2.5 ").unwrap(), json!(2.5));
        assert_eq!(f.parse_text("").unwrap_err(), "required");
        assert_eq!(f.parse_text("abc").unwrap_err(), "not a number");
        assert!(f.parse_text("NaN").is_err());
        let i = input(InputType::Int);
        assert_eq!(i.parse_text("7").unwrap(), json!(7));
        assert!(i.parse_text("7.5").is_err());
        let b = input(InputType::Bool);
        assert_eq!(b.parse_text("true").unwrap(), json!(true));
    }

    #[test]
    fn spec_errors() {
        let mut f = input(InputType::Float);
        f.min = Some(5.0);
        f.max = Some(1.0);
        f.default = Some(toml::Value::String("x".into()));
        let e = f.spec_errors();
        assert!(e[0].contains("min 5 is above max 1"), "{e:?}");
        assert!(e[1].contains("default"), "{e:?}");
        let mut b = input(InputType::Bool);
        b.choices = Some(vec![]);
        let e = b.spec_errors();
        assert!(e.iter().any(|e| e.contains("min/max only apply")), "{e:?}");
        assert!(e.iter().any(|e| e.contains("choices only apply")), "{e:?}");
    }
}
