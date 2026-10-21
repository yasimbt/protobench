//! Schema-independent validation of a parsed [`ProjectConfig`].
//!
//! Checks that need the descriptor pool live in [`crate::project`].

use std::collections::HashSet;

use super::load::Seg;
use super::model::*;
use crate::key_path;

/// A validation finding: key path, `true` for an error (else a warning), message.
pub type Finding = (Vec<Seg>, bool, String);

/// Parses a `[codec.ids]` key (`"0x01"` or `"1"`).
pub fn parse_id_key(k: &str) -> Option<u32> {
    let k = k.trim();
    if let Some(h) = k.strip_prefix("0x").or_else(|| k.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).ok()
    } else {
        k.parse().ok()
    }
}

#[derive(Default)]
struct Findings(Vec<Finding>);

impl Findings {
    fn err(&mut self, p: Vec<Seg>, m: String) {
        self.0.push((p, true, m));
    }
    fn warn(&mut self, p: Vec<Seg>, m: String) {
        self.0.push((p, false, m));
    }
}

/// Validates a config and returns findings.
pub fn validate(c: &ProjectConfig) -> Vec<Finding> {
    let mut f = Findings::default();

    // schema
    if c.schema.proto_files.is_empty() && c.schema.descriptor_set.is_none() {
        f.err(
            key_path!["schema"],
            "schema needs `proto_files` or `descriptor_set`".into(),
        );
    }

    // connection
    let mut names = HashSet::new();
    for (i, p) in c.connection.profiles.iter().enumerate() {
        if !names.insert(p.name.as_str()) {
            f.err(
                key_path!["connection", "profiles", i, "name"],
                format!("duplicate profile name `{}`", p.name),
            );
        }
        if !(5..=8).contains(&p.data_bits) {
            f.err(
                key_path!["connection", "profiles", i, "data_bits"],
                format!("data_bits must be 5..8, got {}", p.data_bits),
            );
        }
        if !(1..=2).contains(&p.stop_bits) {
            f.err(
                key_path!["connection", "profiles", i, "stop_bits"],
                format!("stop_bits must be 1 or 2, got {}", p.stop_bits),
            );
        }
        match p.transport {
            TransportKind::TcpClient if p.host.trim().is_empty() => f.err(
                key_path!["connection", "profiles", i, "host"],
                "tcp_client needs `host`".into(),
            ),
            TransportKind::Replay if p.file.is_none() => f.err(
                key_path!["connection", "profiles", i],
                "replay needs `file`".into(),
            ),
            TransportKind::Serial
                if p.port == "auto"
                    && p.usb_vid.is_none()
                    && p.usb_pid.is_none()
                    && p.usb_serial.is_none() =>
            {
                f.warn(
                    key_path!["connection", "profiles", i, "port"],
                    "port = \"auto\" without usb_vid, usb_pid or usb_serial only works with exactly one USB serial port attached".into(),
                )
            }
            _ => {}
        }
        if p.transport == TransportKind::Serial && p.port != "auto" && p.usb_serial.is_some() {
            f.warn(
                key_path!["connection", "profiles", i, "usb_serial"],
                format!(
                    "usb_serial is only used with port = \"auto\"; this profile opens `{}`",
                    p.port
                ),
            );
        }
        if p.speed < 0.0 {
            f.err(
                key_path!["connection", "profiles", i, "speed"],
                "speed must be >= 0".into(),
            );
        }
    }
    if let Some(name) = &c.connection.profile {
        if c.profile(name).is_none() {
            f.err(
                key_path!["connection", "profile"],
                format!("default profile `{name}` is not defined in [[connection.profiles]]"),
            );
        }
    }

    // framing
    let fr = &c.framing;
    if fr.kind == FramingType::FixedLen && ![1, 2, 4].contains(&fr.len_bytes) {
        f.err(
            key_path!["framing", "len_bytes"],
            format!("len_bytes must be 1, 2 or 4, got {}", fr.len_bytes),
        );
    }
    if fr.max_frame_len == 0 || fr.max_frame_len > 16 * 1024 * 1024 {
        f.err(
            key_path!["framing", "max_frame_len"],
            format!(
                "max_frame_len must be 1..16777216, got {}",
                fr.max_frame_len
            ),
        );
    }
    if !fr.sync_word.is_empty()
        && !matches!(fr.kind, FramingType::FixedLen | FramingType::VarintLen)
    {
        f.warn(
            key_path!["framing", "sync_word"],
            "sync_word is only used by fixed_len and varint_len framing".into(),
        );
    }

    // codec
    match c.codec.mode {
        CodecMode::Envelope | CodecMode::Fixed => {
            if c.codec.tx_root.is_none() {
                f.err(key_path!["codec"], "codec needs `tx_root`".into());
            }
            if c.codec.rx_root.is_none() {
                f.err(key_path!["codec"], "codec needs `rx_root`".into());
            }
        }
        CodecMode::IdPrefix => {
            if c.codec.ids.is_empty() {
                f.err(
                    key_path!["codec"],
                    "id_prefix mode needs a [codec.ids] table".into(),
                );
            }
            let mut seen = HashSet::new();
            for (k, v) in &c.codec.ids {
                match parse_id_key(k) {
                    None => f.err(
                        key_path!["codec", "ids", k.as_str()],
                        format!("invalid message id `{k}` (use decimal or 0x hex)"),
                    ),
                    Some(id) => {
                        let max = match c.codec.id_width {
                            IdWidth::U8 => 0xFF,
                            IdWidth::U16 => 0xFFFF,
                            IdWidth::Varint => u32::MAX,
                        };
                        if id > max {
                            f.err(
                                key_path!["codec", "ids", k.as_str()],
                                format!("message id {id} does not fit id_width"),
                            );
                        }
                        if !seen.insert(id) {
                            f.err(
                                key_path!["codec", "ids", k.as_str()],
                                format!("message id {id} listed twice"),
                            );
                        }
                    }
                }
                if v.trim().is_empty() {
                    f.err(
                        key_path!["codec", "ids", k.as_str()],
                        "empty type name".into(),
                    );
                }
            }
        }
    }

    // timestamp
    if c.timestamp.source == TimestampSource::Device && c.timestamp.field.is_none() {
        f.err(
            key_path!["timestamp", "source"],
            "timestamp.source = \"device\" needs `field`".into(),
        );
    }
    if c.timestamp.wrap_bits > 64 {
        f.err(
            key_path!["timestamp", "wrap_bits"],
            "wrap_bits must be 0..64".into(),
        );
    }

    // commands
    let mut names = HashSet::new();
    for (i, cmd) in c.commands.iter().enumerate() {
        if cmd.name.trim().is_empty() {
            f.err(key_path!["commands", i], "command needs `name`".into());
        } else if !names.insert(cmd.name.as_str()) {
            f.err(
                key_path!["commands", i, "name"],
                format!("duplicate command name `{}`", cmd.name),
            );
        }
        if cmd.message.trim().is_empty() {
            f.err(
                key_path!["commands", i],
                format!("command `{}` needs `message`", cmd.name),
            );
        }
        if cmd.periodic_ms == Some(0) {
            f.err(
                key_path!["commands", i, "periodic_ms"],
                "periodic_ms must be > 0".into(),
            );
        }
        if let Some(col) = &cmd.colour {
            if parse_colour(col).is_none() {
                f.err(
                    key_path!["commands", i, "colour"],
                    format!("colour `{col}` is not #rrggbb"),
                );
            }
        }
    }

    // telemetry
    for (i, t) in c.telemetry.iter().enumerate() {
        match t.mode {
            TelemetryMode::Stream | TelemetryMode::Triggered if t.message.is_none() => f.err(
                key_path!["telemetry", i],
                format!("telemetry `{}` needs `message`", t.name),
            ),
            TelemetryMode::Poll if t.request.is_none() || t.response.is_none() => f.err(
                key_path!["telemetry", i],
                format!("poll telemetry `{}` needs `request` and `response`", t.name),
            ),
            TelemetryMode::Poll if t.interval_ms == 0 => f.err(
                key_path!["telemetry", i, "interval_ms"],
                "interval_ms must be > 0".into(),
            ),
            _ => {}
        }
        for (j, ch) in t.channels.iter().enumerate() {
            if let Err(e) = crate::schema::FieldPath::parse(ch) {
                f.err(key_path!["telemetry", i, "channels", j], e);
            }
        }
    }

    // derived
    for (i, d) in c.derived.iter().enumerate() {
        if d.name.trim().is_empty() {
            f.err(
                key_path!["derived", i],
                "derived channel needs `name`".into(),
            );
        }
        if let Err(e) = crate::telemetry::Expr::compile(&d.expr) {
            f.err(key_path!["derived", i, "expr"], e.to_string());
        }
    }

    // simulator
    let sim = &c.simulator;
    for (k, v) in [
        ("drop_rate", sim.drop_rate),
        ("corrupt_rate", sim.corrupt_rate),
        ("garbage_rate", sim.garbage_rate),
    ] {
        if !(0.0..=1.0).contains(&v) {
            f.err(key_path!["simulator", k], format!("{k} must be in 0..1"));
        }
    }
    for (i, st) in sim.streams.iter().enumerate() {
        if st.rate_hz <= 0.0 || st.rate_hz > 1_000_000.0 {
            f.err(
                key_path!["simulator", "streams", i, "rate_hz"],
                "rate_hz must be in (0, 1e6]".into(),
            );
        }
    }
    for (i, r) in sim.responses.iter().enumerate() {
        if let Some(a) = &r.action {
            if parse_sim_action(a).is_none() {
                f.err(
                    key_path!["simulator", "responses", i, "action"],
                    format!("unknown action `{a}` (toggle_stream:<msg>, enable_stream:<msg>, disable_stream:<msg>)"),
                );
            }
        }
    }
    f.0
}

/// Parses `#rrggbb` (or `rrggbb`) into RGB.
pub fn parse_colour(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// Simulator response actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimAction {
    /// Toggle a stream (or set it from the request's `enable` field).
    Toggle(String),
    /// Enable a stream.
    Enable(String),
    /// Disable a stream.
    Disable(String),
}

/// Parses a simulator `action` string.
pub fn parse_sim_action(s: &str) -> Option<SimAction> {
    let (verb, msg) = s.split_once(':')?;
    let msg = msg.trim().to_string();
    if msg.is_empty() {
        return None;
    }
    match verb.trim() {
        "toggle_stream" => Some(SimAction::Toggle(msg)),
        "enable_stream" => Some(SimAction::Enable(msg)),
        "disable_stream" => Some(SimAction::Disable(msg)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_keys() {
        assert_eq!(parse_id_key("0x1F"), Some(31));
        assert_eq!(parse_id_key("7"), Some(7));
        assert_eq!(parse_id_key("x"), None);
    }

    #[test]
    fn colours() {
        assert_eq!(parse_colour("#c0392b"), Some([0xc0, 0x39, 0x2b]));
        assert_eq!(parse_colour("red"), None);
    }

    #[test]
    fn actions() {
        assert_eq!(
            parse_sim_action("toggle_stream:telemetry"),
            Some(SimAction::Toggle("telemetry".into()))
        );
        assert_eq!(parse_sim_action("explode:x"), None);
        assert_eq!(parse_sim_action("enable_stream:"), None);
    }

    #[test]
    fn duplicate_profiles_and_bad_default() {
        let mut c = ProjectConfig::default();
        c.schema.proto_files.push("a.proto".into());
        c.codec.tx_root = Some("a.T".into());
        c.codec.rx_root = Some("a.R".into());
        c.connection.profile = Some("nope".into());
        c.connection.profiles.push(ConnectionProfile {
            name: "a".into(),
            transport: TransportKind::Sim,
            ..Default::default()
        });
        c.connection.profiles.push(ConnectionProfile {
            name: "a".into(),
            transport: TransportKind::Sim,
            ..Default::default()
        });
        let f = validate(&c);
        assert!(f
            .iter()
            .any(|(_, e, m)| *e && m.contains("duplicate profile")));
        assert!(f
            .iter()
            .any(|(_, e, m)| *e && m.contains("default profile")));
    }
}
