//! Derived-channel expressions, evaluated with `evalexpr`.
//!
//! Variables are field paths or channel names such as `telemetry.vout_raw`
//! or `telemetry.adc[2]`. Common math functions can be written without the
//! `math::` prefix: `sqrt(a*a + b*b)`.

use evalexpr::{
    build_operator_tree, ContextWithMutableVariables, DefaultNumericTypes, HashMapContext, Node,
    Value,
};

use crate::error::CoreError;

const MATH_FNS: &[&str] = &[
    "ln", "log", "log2", "log10", "exp", "exp2", "pow", "cos", "acos", "cosh", "acosh", "sin",
    "asin", "sinh", "asinh", "tan", "atan", "tanh", "atanh", "atan2", "sqrt", "cbrt", "hypot",
    "abs",
];

/// A compiled expression.
#[derive(Debug, Clone)]
pub struct Expr {
    source: String,
    node: Node<DefaultNumericTypes>,
    vars: Vec<String>,
    ctx: HashMapContext<DefaultNumericTypes>,
}

impl Expr {
    /// Compiles an expression.
    pub fn compile(src: &str) -> Result<Expr, CoreError> {
        let rewritten = add_math_prefix(src);
        let node = build_operator_tree::<DefaultNumericTypes>(&rewritten)
            .map_err(|e| CoreError::Expr(format!("`{src}`: {e}")))?;
        let mut vars: Vec<String> = node
            .iter_variable_identifiers()
            .map(str::to_string)
            .collect();
        vars.sort();
        vars.dedup();
        Ok(Expr {
            source: src.to_string(),
            node,
            vars,
            ctx: HashMapContext::new(),
        })
    }

    /// The source text.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Variable names the expression reads.
    pub fn variables(&self) -> &[String] {
        &self.vars
    }

    /// Evaluates with variables supplied by `lookup`. Returns `Ok(None)` when
    /// a variable is not available yet.
    pub fn eval(
        &mut self,
        mut lookup: impl FnMut(&str) -> Option<f64>,
    ) -> Result<Option<f64>, CoreError> {
        for v in &self.vars {
            let Some(x) = lookup(v) else {
                return Ok(None);
            };
            self.ctx
                .set_value(v.clone(), Value::Float(x))
                .map_err(|e| CoreError::Expr(e.to_string()))?;
        }
        let r = self
            .node
            .eval_with_context(&self.ctx)
            .map_err(|e| CoreError::Expr(format!("`{}`: {e}", self.source)))?;
        match r {
            Value::Float(f) => Ok(Some(f)),
            Value::Int(i) => Ok(Some(i as f64)),
            Value::Boolean(b) => Ok(Some(if b { 1.0 } else { 0.0 })),
            other => Err(CoreError::Expr(format!(
                "`{}` evaluated to a non-number: {other}",
                self.source
            ))),
        }
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '[' | ']')
}

fn add_math_prefix(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 16);
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if is_ident_char(chars[i]) && (i == 0 || !is_ident_char(chars[i - 1])) {
            let start = i;
            while i < chars.len() && is_ident_char(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if chars.get(j) == Some(&'(') && MATH_FNS.contains(&word.as_str()) {
                out.push_str("math::");
            }
            out.push_str(&word);
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_with_dotted_names() {
        let mut e =
            Expr::compile("telemetry.vout_raw * 0.0503 * telemetry.current_na / 1000").unwrap();
        assert_eq!(e.variables().len(), 2);
        let v = e
            .eval(|n| match n {
                "telemetry.vout_raw" => Some(1000.0),
                "telemetry.current_na" => Some(250.0),
                _ => None,
            })
            .unwrap()
            .unwrap();
        assert!((v - 12.575).abs() < 1e-9, "{v}");
    }

    #[test]
    fn math_functions_and_indexes() {
        let mut e = Expr::compile("sqrt(a[0]*a[0] + b*b)").unwrap();
        let v = e
            .eval(|n| match n {
                "a[0]" => Some(3.0),
                "b" => Some(4.0),
                _ => None,
            })
            .unwrap();
        assert_eq!(v, Some(5.0));
        let mut e = Expr::compile("abs(x) + max(x, 2)").unwrap();
        assert_eq!(e.eval(|_| Some(-1.0)).unwrap(), Some(3.0));
    }

    #[test]
    fn missing_variable_is_none_and_bad_syntax_errors() {
        let mut e = Expr::compile("x + 1").unwrap();
        assert_eq!(e.eval(|_| None).unwrap(), None);
        assert!(Expr::compile("x +* (").is_err());
        let mut e = Expr::compile("x > 1").unwrap();
        assert_eq!(e.eval(|_| Some(2.0)).unwrap(), Some(1.0));
    }
}
