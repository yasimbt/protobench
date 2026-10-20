//! Turns decoded messages into channel samples.

use std::collections::HashMap;

use prost_reflect::{Kind, Value};

use super::store::{ChannelKind, TelemetryStore};
use super::Expr;
use crate::codec::{Codec, DecodedMessage, MessageTarget};
use crate::config::{ProjectConfig, TelemetryMode};
use crate::error::CoreError;
use crate::schema::{extract, value_to_f64, FieldDisplay, FieldPath};

struct Subscription {
    name: String,
    mode: TelemetryMode,
    target: MessageTarget,
    paths: Vec<FieldPath>,
    trigger: Option<Expr>,
    samples: usize,
    /// Messages still to capture in triggered mode.
    remaining: usize,
}

struct Derived {
    name: String,
    expr: Expr,
    vars: Vec<String>,
    paths: Vec<Option<FieldPath>>,
    display: FieldDisplay,
}

/// Extracts channels from messages according to `[[telemetry]]` and
/// `[[derived]]`.
pub struct Extractor {
    subs: Vec<Subscription>,
    derived: Vec<Derived>,
    fields: Vec<crate::config::FieldMeta>,
}

/// State of a triggered capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerState {
    /// Telemetry entry name.
    pub name: String,
    /// Messages left to capture (0 = idle).
    pub remaining: usize,
    /// Messages per capture.
    pub samples: usize,
}

impl Extractor {
    /// Builds the extractor; config errors (bad paths, unknown messages) are
    /// returned.
    pub fn new(config: &ProjectConfig, codec: &Codec) -> Result<Extractor, CoreError> {
        let mut subs = Vec::new();
        for t in &config.telemetry {
            let msg = match t.mode {
                TelemetryMode::Poll => t.response.as_deref(),
                _ => t.message.as_deref(),
            }
            .ok_or_else(|| CoreError::Config(format!("telemetry `{}` has no message", t.name)))?;
            let target = codec.resolve_rx(msg)?;
            let paths = t
                .channels
                .iter()
                .map(|c| FieldPath::parse(c).map_err(CoreError::Config))
                .collect::<Result<Vec<_>, _>>()?;
            let trigger = t.trigger.as_deref().map(Expr::compile).transpose()?;
            subs.push(Subscription {
                name: t.name.clone(),
                mode: t.mode,
                target,
                paths,
                trigger,
                samples: t.samples.max(1),
                remaining: 0,
            });
        }
        let mut derived = Vec::new();
        for d in &config.derived {
            let expr = Expr::compile(&d.expr)?;
            let paths = expr
                .variables()
                .iter()
                .map(|v| FieldPath::parse(v).ok())
                .collect();
            let mut display = FieldDisplay::lookup(&config.fields, &d.name);
            if display.unit.is_empty() {
                display.unit = d.unit.clone().unwrap_or_default();
            }
            if d.format.is_some() {
                display.format = d.format.clone();
            }
            derived.push(Derived {
                name: d.name.clone(),
                vars: expr.variables().to_vec(),
                expr,
                paths,
                display,
            });
        }
        Ok(Extractor {
            subs,
            derived,
            fields: config.fields.clone(),
        })
    }

    /// Arms a triggered capture by telemetry name. Returns false if unknown.
    pub fn arm(&mut self, name: &str) -> bool {
        match self
            .subs
            .iter_mut()
            .find(|s| s.name == name && s.mode == TelemetryMode::Triggered)
        {
            Some(s) => {
                s.remaining = s.samples;
                true
            }
            None => false,
        }
    }

    /// States of the triggered captures.
    pub fn triggers(&self) -> Vec<TriggerState> {
        self.subs
            .iter()
            .filter(|s| s.mode == TelemetryMode::Triggered)
            .map(|s| TriggerState {
                name: s.name.clone(),
                remaining: s.remaining,
                samples: s.samples,
            })
            .collect()
    }

    /// Processes one received message. `t` is the sample time in seconds.
    /// Returns the number of samples written.
    pub fn process(
        &mut self,
        codec: &Codec,
        msg: &DecodedMessage,
        t: f64,
        store: &TelemetryStore,
    ) -> usize {
        let mut raw: HashMap<String, f64> = HashMap::new();
        let mut written = 0;
        for sub in &mut self.subs {
            if !codec.matches(msg, &sub.target) {
                continue;
            }
            if sub.mode == TelemetryMode::Triggered {
                if sub.remaining == 0 {
                    if let Some(tr) = sub.trigger.as_mut() {
                        let fired = tr
                            .eval(|v| lookup_in_message(msg, v))
                            .ok()
                            .flatten()
                            .is_some_and(|x| x != 0.0);
                        if fired {
                            sub.remaining = sub.samples;
                        }
                    }
                }
                if sub.remaining == 0 {
                    continue;
                }
                sub.remaining -= 1;
            }
            for path in &sub.paths {
                for e in extract(&msg.msg, path) {
                    let Some(v) = value_to_f64(&e.value) else {
                        continue;
                    };
                    let fields = &self.fields;
                    let ch = store.get_or_create(&e.name, || {
                        let kind = match (e.field.kind(), &e.value) {
                            (Kind::Enum(d), _) => ChannelKind::Enum(d),
                            (_, Value::Bool(_)) => ChannelKind::Bool,
                            _ => ChannelKind::Number,
                        };
                        (FieldDisplay::lookup(fields, &e.name), kind)
                    });
                    let value = match ch.kind {
                        ChannelKind::Number => ch.display.apply(v),
                        _ => v,
                    };
                    ch.push(t, value);
                    raw.insert(e.name, v);
                    written += 1;
                }
            }
        }
        for d in &mut self.derived {
            let mut any_here = false;
            let (vars, paths) = (&d.vars, &d.paths);
            let r = d.expr.eval(|var| {
                if let Some(v) = raw.get(var) {
                    any_here = true;
                    return Some(*v);
                }
                let idx = vars.iter().position(|v| v == var);
                if let Some(Some(p)) = idx.and_then(|i| paths.get(i)) {
                    if let Some(v) = extract(&msg.msg, p)
                        .first()
                        .and_then(|e| value_to_f64(&e.value))
                    {
                        any_here = true;
                        return Some(v);
                    }
                }
                store.get(var).and_then(|c| c.last()).map(|(_, v)| v)
            });
            if let Ok(Some(v)) = r {
                if any_here && v.is_finite() {
                    let display = d.display.clone();
                    let ch = store.get_or_create(&d.name, || (display, ChannelKind::Derived));
                    ch.push(t, v);
                    written += 1;
                }
            }
        }
        written
    }
}

fn lookup_in_message(msg: &DecodedMessage, var: &str) -> Option<f64> {
    let p = FieldPath::parse(var).ok()?;
    extract(&msg.msg, &p)
        .first()
        .and_then(|e| value_to_f64(&e.value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Role;
    use crate::project::Project;

    fn setup() -> (Project, Codec, Codec) {
        let p = Project::open(&crate::schema::tests::demo_dir());
        let s = p.schema.clone().unwrap();
        let host = Codec::new(&p.config, &s, Role::Host).unwrap();
        let dev = Codec::new(&p.config, &s, Role::Device).unwrap();
        (p, host, dev)
    }

    fn telemetry(dev: &Codec, host: &Codec, json: serde_json::Value) -> DecodedMessage {
        let t = dev.resolve_tx("telemetry").unwrap();
        let m = dev.build(&t, json).unwrap();
        host.decode(&dev.encode(&m).unwrap(), 0).unwrap()
    }

    #[test]
    fn stream_extraction_with_scale_enum_wildcard_and_derived() {
        let (p, host, dev) = setup();
        let mut ex = Extractor::new(&p.config, &host).unwrap();
        let store = TelemetryStore::new(100);
        let m = telemetry(
            &dev,
            &host,
            serde_json::json!({"vout_raw": 1000, "current_na": 250.0, "adc": [1, 2, 3, 4], "state": "MEASURING"}),
        );
        let n = ex.process(&host, &m, 1.0, &store);
        // vout, current, temperature, 4 adc, state + derived power
        assert_eq!(n, 9, "{:?}", store.names());
        let vout = store.get("telemetry.vout_raw").unwrap();
        assert!((vout.last().unwrap().1 - 50.3).abs() < 1e-9);
        assert_eq!(vout.display.display, "Vout");
        assert_eq!(vout.format_value(50.3), "50.30");
        let st = store.get("telemetry.state").unwrap();
        assert!(matches!(st.kind, ChannelKind::Enum(_)));
        assert_eq!(st.format_value(st.last().unwrap().1), "MEASURING");
        assert_eq!(
            store.get("telemetry.adc[3]").unwrap().last().unwrap().1,
            4.0
        );
        let pw = store.get("power_uw").unwrap();
        assert!((pw.last().unwrap().1 - 1000.0 * 0.0503 * 250.0 / 1000.0).abs() < 1e-6);
        assert_eq!(pw.display.unit, "uW");
        // Messages of other types do not touch telemetry channels.
        let t = dev.resolve_tx("pong").unwrap();
        let pong = host
            .decode(
                &dev.encode(&dev.build(&t, serde_json::json!({})).unwrap())
                    .unwrap(),
                0,
            )
            .unwrap();
        assert_eq!(ex.process(&host, &pong, 2.0, &store), 0);
    }

    #[test]
    fn triggered_capture_counts_down() {
        let (p, host, dev) = setup();
        let mut cfg = p.config.clone();
        cfg.derived.clear();
        cfg.telemetry = vec![crate::config::TelemetryConfig {
            name: "burst".into(),
            mode: TelemetryMode::Triggered,
            message: Some("telemetry".into()),
            channels: vec!["telemetry.vout_raw".into()],
            trigger: Some("telemetry.vout_raw > 2000".into()),
            samples: 3,
            ..Default::default()
        }];
        let mut ex = Extractor::new(&cfg, &host).unwrap();
        let store = TelemetryStore::new(100);
        for (i, v) in [100, 2500, 100, 100, 100, 100].iter().enumerate() {
            let m = telemetry(&dev, &host, serde_json::json!({"vout_raw": v}));
            ex.process(&host, &m, i as f64, &store);
        }
        assert_eq!(store.get("telemetry.vout_raw").unwrap().total(), 3);
        assert!(ex.arm("burst"));
        assert_eq!(ex.triggers()[0].remaining, 3);
        assert!(!ex.arm("nope"));
    }
}
