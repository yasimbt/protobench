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
        validate_profile(p, i, &c.framing, &mut f);
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

    // devices
    let mut names = HashSet::new();
    for (i, d) in c.devices.iter().enumerate() {
        if !is_device_name(&d.name) {
            f.err(
                key_path!["devices", i, "name"],
                format!(
                    "device name `{}` must start with a letter or `_` and contain only letters, digits and `_`",
                    d.name
                ),
            );
        } else if d.name == MAIN_DEVICE {
            f.err(
                key_path!["devices", i, "name"],
                "`main` is this project's own device; pick another name".into(),
            );
        } else if !names.insert(d.name.as_str()) {
            f.err(
                key_path!["devices", i, "name"],
                format!("duplicate device name `{}`", d.name),
            );
        }
        if d.project.trim().is_empty() {
            f.err(
                key_path!["devices", i],
                format!("device `{}` needs `project` (a project folder)", d.name),
            );
        }
    }
    if c.devices.len() >= MAX_DEVICES {
        f.err(
            key_path!["devices", MAX_DEVICES - 1],
            format!(
                "at most {MAX_DEVICES} devices including this project's own; {} are listed",
                c.devices.len() + 1
            ),
        );
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

/// True for `host:port` with a numeric port.
fn is_host_port(s: &str) -> bool {
    s.rsplit_once(':')
        .is_some_and(|(h, p)| !h.trim().is_empty() && p.trim().parse::<u16>().is_ok())
}

/// Required fields of each transport kind, and a framing that suits it.
fn validate_profile(p: &ConnectionProfile, i: usize, framing: &FramingConfig, f: &mut Findings) {
    let at = |k: &str| key_path!["connection", "profiles", i, k];
    let kind = p.transport.as_str();
    match p.transport {
        TransportKind::Serial => {
            if p.port.trim().is_empty() {
                f.err(
                    at("port"),
                    "serial needs `port` (a name or \"auto\")".into(),
                );
            }
            if p.baud == 0 {
                f.err(at("baud"), "baud must be > 0".into());
            }
            if p.port == "auto"
                && p.usb_vid.is_none()
                && p.usb_pid.is_none()
                && p.usb_serial.is_none()
            {
                f.warn(
                    at("port"),
                    "port = \"auto\" without usb_vid, usb_pid or usb_serial only works with exactly one USB serial port attached".into(),
                );
            }
            if p.port != "auto" && p.usb_serial.is_some() {
                f.warn(
                    at("usb_serial"),
                    format!(
                        "usb_serial is only used with port = \"auto\"; this profile opens `{}`",
                        p.port
                    ),
                );
            }
        }
        TransportKind::TcpClient => {
            if p.host.trim().is_empty() {
                f.err(at("host"), "tcp_client needs `host`".into());
            }
            if p.tcp_port == 0 {
                f.err(at("tcp_port"), "tcp_client needs `tcp_port`".into());
            }
        }
        TransportKind::TcpServer => {}
        TransportKind::Udp => {
            if let Some(b) = p.bind.as_deref().filter(|b| !is_host_port(b)) {
                f.err(at("bind"), format!("udp bind `{b}` is not host:port"));
            }
            if let Some(t) = p.target.as_deref().filter(|t| !is_host_port(t)) {
                f.err(at("target"), format!("udp target `{t}` is not host:port"));
            }
        }
        TransportKind::Sim => {}
        TransportKind::Mqtt => {
            if p.broker.trim().is_empty() {
                f.err(
                    key_path!["connection", "profiles", i],
                    "mqtt needs `broker` (host:port)".into(),
                );
            }
            if p.subscribe.is_empty() && p.publish.is_none() {
                f.err(
                    key_path!["connection", "profiles", i],
                    "mqtt needs `subscribe` (topics to receive) and/or `publish` (topic to send to)"
                        .into(),
                );
            }
            for (j, t) in p.subscribe.iter().enumerate() {
                if let Some(e) = crate::transport::mqtt::filter_error(t) {
                    f.err(key_path!["connection", "profiles", i, "subscribe", j], e);
                }
            }
            if let Some(e) = p
                .publish
                .as_deref()
                .and_then(crate::transport::mqtt::topic_error)
            {
                f.err(at("publish"), e);
            }
            if p.qos > 1 {
                f.err(
                    at("qos"),
                    format!("qos must be 0 or 1 (QoS 2 is not supported), got {}", p.qos),
                );
            }
            if p.tls_cert.is_some() != p.tls_key.is_some() {
                f.err(
                    key_path!["connection", "profiles", i],
                    "mutual TLS needs both `tls_cert` and `tls_key`".into(),
                );
            }
            if (p.tls_ca.is_some() || p.tls_cert.is_some()) && !p.tls {
                f.warn(
                    at("tls"),
                    "tls_ca/tls_cert are set but tls = false; they are ignored".into(),
                );
            }
            if p.password.is_some() && p.password_env.is_some() {
                f.warn(
                    at("password"),
                    "both password and password_env are set; password_env wins".into(),
                );
            }
        }
        TransportKind::Can => {
            let max_id = if p.extended_id { 0x1FFF_FFFF } else { 0x7FF };
            for (k, v) in [("tx_id", p.tx_id), ("rx_id", p.rx_id)] {
                match v {
                    None => f.err(
                        key_path!["connection", "profiles", i],
                        format!("can needs `{k}` (the CAN identifier, e.g. 0x7E0)"),
                    ),
                    Some(id) if id > max_id => f.err(
                        at(k),
                        format!(
                            "{k} {id:#x} does not fit {} identifiers{}",
                            if p.extended_id { "29-bit" } else { "11-bit" },
                            if p.extended_id {
                                ""
                            } else {
                                " (set extended_id = true)"
                            }
                        ),
                    ),
                    _ => {}
                }
            }
            if p.tx_id.is_some() && p.tx_id == p.rx_id {
                f.err(at("rx_id"), "tx_id and rx_id must differ".into());
            }
            match p.can_backend {
                CanBackend::Slcan => {
                    if p.port.trim().is_empty() {
                        f.err(
                            at("port"),
                            "slcan needs `port` (the adapter's serial port)".into(),
                        );
                    }
                    if crate::transport::can::slcan::bitrate_code(p.bitrate).is_none() {
                        f.err(
                            at("bitrate"),
                            format!(
                                "slcan adapters take 10k, 20k, 50k, 100k, 125k, 250k, 500k, 800k or 1M bit/s, not {}",
                                p.bitrate
                            ),
                        );
                    }
                    if let Some(d) = p.data_bitrate {
                        if crate::transport::can::slcan::data_bitrate_code(d).is_none() {
                            f.err(
                                at("data_bitrate"),
                                format!("slcan data bit rates are 1M, 2M, 4M, 5M or 8M, not {d}"),
                            );
                        }
                    }
                }
                CanBackend::Socketcan => {
                    if p.interface.trim().is_empty() {
                        f.err(
                            at("interface"),
                            "socketcan needs `interface` (e.g. can0)".into(),
                        );
                    }
                    if !cfg!(target_os = "linux") {
                        f.warn(at("can_backend"), "SocketCAN is Linux only".into());
                    }
                }
            }
            if p.data_bitrate.is_some() && !p.fd {
                f.warn(
                    at("data_bitrate"),
                    "data_bitrate is only used with fd = true".into(),
                );
            }
            if p.isotp_st_min_us > 127_000 {
                f.err(
                    at("isotp_st_min_us"),
                    "isotp_st_min_us must be at most 127000".into(),
                );
            }
        }
        TransportKind::Replay => {
            if p.file.as_deref().is_none_or(|x| x.trim().is_empty()) {
                f.err(
                    key_path!["connection", "profiles", i],
                    "replay needs `file`".into(),
                );
            }
            if p.speed < 0.0 {
                f.err(at("speed"), "speed must be >= 0".into());
            }
        }
    }
    let none = framing.kind == FramingType::None;
    match p.transport.link_kind() {
        LinkKind::Stream if none => f.warn(
            at("transport"),
            format!(
                "`{kind}` is a byte stream, but framing type = \"none\" treats every read as one frame; use cobs, slip or a length prefix"
            ),
        ),
        LinkKind::Message if !none => f.warn(
            at("transport"),
            format!(
                "`{kind}` delivers whole messages; framing type = \"none\" is the usual choice (each message is one frame)"
            ),
        ),
        _ => {}
    }
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

    fn base() -> ProjectConfig {
        let mut c = ProjectConfig::default();
        c.schema.proto_files.push("a.proto".into());
        c.codec.tx_root = Some("a.T".into());
        c.codec.rx_root = Some("a.R".into());
        c
    }

    fn profile_findings(c: &ProjectConfig) -> Vec<(String, bool, String)> {
        validate(c)
            .into_iter()
            .map(|(p, e, m)| (crate::config::path_to_string(&p), e, m))
            .collect()
    }

    #[test]
    fn each_transport_kind_reports_missing_fields() {
        let mut c = base();
        let bad = |t: TransportKind, edit: &dyn Fn(&mut ConnectionProfile)| {
            let mut p = ConnectionProfile {
                name: t.as_str().into(),
                transport: t,
                ..Default::default()
            };
            edit(&mut p);
            p
        };
        c.connection.profiles = vec![
            bad(TransportKind::Serial, &|p| p.port = " ".into()),
            bad(TransportKind::TcpClient, &|p| {
                p.host = String::new();
                p.tcp_port = 0;
            }),
            bad(TransportKind::Udp, &|p| p.target = Some("nohost".into())),
            bad(TransportKind::Replay, &|p| p.speed = -1.0),
            bad(TransportKind::Mqtt, &|p| {
                p.subscribe = vec!["a/#/b".into()];
                p.publish = Some("a/+".into());
                p.qos = 2;
            }),
            bad(TransportKind::Mqtt, &|_| {}),
            bad(TransportKind::Can, &|p| {
                p.tx_id = Some(0x900);
                p.bitrate = 333_000;
            }),
            bad(TransportKind::Can, &|p| {
                p.tx_id = Some(0x10);
                p.rx_id = Some(0x10);
                p.can_backend = CanBackend::Socketcan;
                p.interface = String::new();
            }),
        ];
        let f = profile_findings(&c);
        let has =
            |key: &str, text: &str| f.iter().any(|(k, e, m)| *e && k == key && m.contains(text));
        assert!(
            has("connection.profiles[0].port", "serial needs `port`"),
            "{f:#?}"
        );
        assert!(has("connection.profiles[1].host", "needs `host`"), "{f:#?}");
        assert!(
            has("connection.profiles[1].tcp_port", "needs `tcp_port`"),
            "{f:#?}"
        );
        assert!(
            has("connection.profiles[2].target", "not host:port"),
            "{f:#?}"
        );
        assert!(
            has("connection.profiles[3]", "replay needs `file`"),
            "{f:#?}"
        );
        assert!(has("connection.profiles[3].speed", ">= 0"), "{f:#?}");
        assert!(
            has("connection.profiles[4]", "mqtt needs `broker`"),
            "{f:#?}"
        );
        assert!(
            has(
                "connection.profiles[4].subscribe[0]",
                "`#` must be a whole, last level"
            ),
            "{f:#?}"
        );
        assert!(
            has("connection.profiles[4].publish", "cannot contain"),
            "{f:#?}"
        );
        assert!(
            has("connection.profiles[4].qos", "QoS 2 is not supported"),
            "{f:#?}"
        );
        assert!(has("connection.profiles[5]", "needs `subscribe`"), "{f:#?}");
        assert!(has("connection.profiles[6]", "can needs `rx_id`"), "{f:#?}");
        assert!(
            has(
                "connection.profiles[6].tx_id",
                "does not fit 11-bit identifiers"
            ),
            "{f:#?}"
        );
        assert!(
            has("connection.profiles[6].bitrate", "slcan adapters take"),
            "{f:#?}"
        );
        assert!(has("connection.profiles[7].rx_id", "must differ"), "{f:#?}");
        assert!(
            has(
                "connection.profiles[7].interface",
                "socketcan needs `interface`"
            ),
            "{f:#?}"
        );
        // Sim and tcp_server need nothing beyond their defaults.
        c.connection.profiles = vec![
            bad(TransportKind::Sim, &|_| {}),
            bad(TransportKind::TcpServer, &|_| {}),
        ];
        assert!(profile_findings(&c).iter().all(|(_, e, _)| !e));
    }

    #[test]
    fn framing_must_suit_the_link() {
        let mut c = base();
        c.connection.profiles = vec![
            ConnectionProfile {
                name: "usb".into(),
                port: "COM3".into(),
                ..Default::default()
            },
            ConnectionProfile {
                name: "udp".into(),
                transport: TransportKind::Udp,
                ..Default::default()
            },
        ];
        c.framing.kind = FramingType::None;
        let f = profile_findings(&c);
        let warned = |i: usize| {
            f.iter().any(|(k, e, m)| {
                !e && k == &format!("connection.profiles[{i}].transport")
                    && m.contains("byte stream")
            })
        };
        assert!(warned(0), "{f:#?}");
        assert!(!warned(1), "UDP datagrams suit any framing: {f:#?}");
        c.framing.kind = FramingType::Cobs;
        assert!(profile_findings(&c)
            .iter()
            .all(|(k, _, _)| !k.ends_with(".transport")));
        // A message link with a framer gets the opposite hint.
        c.connection.profiles.push(ConnectionProfile {
            name: "broker".into(),
            transport: TransportKind::Mqtt,
            broker: "localhost".into(),
            subscribe: vec!["up".into()],
            ..Default::default()
        });
        let f = profile_findings(&c);
        assert!(
            f.iter().any(|(k, e, m)| !e
                && k == "connection.profiles[2].transport"
                && m.contains("delivers whole messages")),
            "{f:#?}"
        );
    }
}
