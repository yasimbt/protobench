//! Field metadata overlay: display name, unit, scale/offset, format and limits.

use crate::config::FieldMeta;

/// Where a value sits relative to its limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitState {
    /// Within limits, or no limits configured.
    Normal,
    /// Outside `warn_min`/`warn_max`.
    Warn,
    /// Outside `min`/`max`.
    Alarm,
}

/// Resolved display metadata for one channel.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDisplay {
    /// Display name (defaults to the channel name).
    pub display: String,
    /// Unit, possibly empty.
    pub unit: String,
    /// Multiplier.
    pub scale: f64,
    /// Offset added after scaling.
    pub offset: f64,
    /// Format string.
    pub format: Option<String>,
    /// Warning limits.
    pub warn: (Option<f64>, Option<f64>),
    /// Alarm limits.
    pub alarm: (Option<f64>, Option<f64>),
}

impl FieldDisplay {
    /// Plain metadata for a channel without an overlay entry.
    pub fn plain(name: &str) -> Self {
        Self {
            display: name.to_string(),
            unit: String::new(),
            scale: 1.0,
            offset: 0.0,
            format: None,
            warn: (None, None),
            alarm: (None, None),
        }
    }

    /// Finds the overlay for a concrete channel name. `a.b[2]` falls back to
    /// `a.b[*]` and then `a.b`; the display name of a fallback gets `[2]`
    /// appended.
    pub fn lookup(fields: &[FieldMeta], channel: &str) -> Self {
        let exact = fields.iter().find(|f| f.path == channel);
        let (meta, suffix) = match exact {
            Some(m) => (Some(m), String::new()),
            None => match channel.rfind('[') {
                Some(open) if channel.ends_with(']') => {
                    let base = &channel[..open];
                    let suffix = channel[open..].to_string();
                    let star = format!("{base}[*]");
                    let m = fields
                        .iter()
                        .find(|f| f.path == star)
                        .or_else(|| fields.iter().find(|f| f.path == base));
                    (m, suffix)
                }
                _ => (None, String::new()),
            },
        };
        match meta {
            None => Self::plain(channel),
            Some(m) => Self {
                display: match &m.display {
                    Some(d) => format!("{d}{suffix}"),
                    None => channel.to_string(),
                },
                unit: m.unit.clone().unwrap_or_default(),
                scale: m.scale.unwrap_or(1.0),
                offset: m.offset.unwrap_or(0.0),
                format: m.format.clone(),
                warn: (m.warn_min, m.warn_max),
                alarm: (m.min, m.max),
            },
        }
    }

    /// Raw → engineering units.
    pub fn apply(&self, raw: f64) -> f64 {
        raw * self.scale + self.offset
    }

    /// Classifies an engineering-unit value against the limits.
    pub fn limit_state(&self, v: f64) -> LimitState {
        let out = |(lo, hi): (Option<f64>, Option<f64>)| {
            lo.is_some_and(|l| v < l) || hi.is_some_and(|h| v > h)
        };
        if out(self.alarm) {
            LimitState::Alarm
        } else if out(self.warn) {
            LimitState::Warn
        } else {
            LimitState::Normal
        }
    }

    /// Formats an engineering-unit value with the configured format.
    pub fn format(&self, v: f64) -> String {
        format_value(v, self.format.as_deref())
    }
}

/// Formats a number with a Rust-like format string. Supported placeholders:
/// `{}`, `{:.N}`, `{:e}`, `{:.Ne}`, `{:x}`, `{:X}`, `{:b}`. Text around the
/// placeholder is kept. Without a format, integers print without decimals and
/// other values with up to 6 significant decimals.
pub fn format_value(v: f64, fmt: Option<&str>) -> String {
    let Some(fmt) = fmt else {
        return default_format(v);
    };
    let (Some(open), Some(close)) = (fmt.find('{'), fmt.find('}')) else {
        return default_format(v);
    };
    if close < open {
        return default_format(v);
    }
    let spec = &fmt[open + 1..close];
    let body = match spec.strip_prefix(':') {
        None if spec.is_empty() => default_format(v),
        None => return default_format(v),
        Some(s) => format_spec(v, s),
    };
    format!("{}{}{}", &fmt[..open], body, &fmt[close + 1..])
}

fn format_spec(v: f64, s: &str) -> String {
    let int = || v.round() as i64;
    match s {
        "x" => format!("{:x}", int()),
        "X" => format!("{:X}", int()),
        "b" => format!("{:b}", int()),
        "e" => format!("{v:e}"),
        "" => default_format(v),
        _ => {
            if let Some(p) = s.strip_prefix('.') {
                let (digits, exp) = match p.strip_suffix('e') {
                    Some(d) => (d, true),
                    None => (p, false),
                };
                match digits.parse::<usize>() {
                    Ok(n) if exp => format!("{v:.n$e}"),
                    Ok(n) => format!("{v:.n$}"),
                    Err(_) => default_format(v),
                }
            } else {
                default_format(v)
            }
        }
    }
}

fn default_format(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else if v.is_finite() {
        let s = format!("{v:.6}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        s.to_string()
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Vec<FieldMeta> {
        vec![
            FieldMeta {
                path: "telemetry.vout_raw".into(),
                display: Some("Vout".into()),
                unit: Some("mV".into()),
                scale: Some(0.0503),
                offset: Some(1.0),
                format: Some("{:.2}".into()),
                warn_min: Some(100.0),
                warn_max: Some(3200.0),
                min: Some(0.0),
                max: Some(3300.0),
            },
            FieldMeta {
                path: "telemetry.adc[*]".into(),
                display: Some("ADC".into()),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn scale_offset_and_limits() {
        let d = FieldDisplay::lookup(&meta(), "telemetry.vout_raw");
        assert_eq!(d.display, "Vout");
        assert!((d.apply(1000.0) - 51.3).abs() < 1e-9);
        assert_eq!(d.format(51.3), "51.30");
        assert_eq!(d.limit_state(50.0), LimitState::Warn);
        assert_eq!(d.limit_state(500.0), LimitState::Normal);
        assert_eq!(d.limit_state(4000.0), LimitState::Alarm);
        assert_eq!(d.limit_state(-1.0), LimitState::Alarm);
    }

    #[test]
    fn wildcard_fallback() {
        let d = FieldDisplay::lookup(&meta(), "telemetry.adc[2]");
        assert_eq!(d.display, "ADC[2]");
        let p = FieldDisplay::lookup(&meta(), "other");
        assert_eq!(p, FieldDisplay::plain("other"));
    }

    #[test]
    fn formats() {
        assert_eq!(format_value(3.0, None), "3");
        assert_eq!(format_value(3.25, None), "3.25");
        assert_eq!(format_value(255.0, Some("0x{:X}")), "0xFF");
        assert_eq!(format_value(1.5, Some("{:.3} V")), "1.500 V");
        assert_eq!(format_value(1500.0, Some("{:.1e}")), "1.5e3");
        assert_eq!(format_value(2.0, Some("{}")), "2");
        assert_eq!(format_value(2.0, Some("bad")), "2");
        assert_eq!(format_value(f64::NAN, None), "NaN");
    }
}
