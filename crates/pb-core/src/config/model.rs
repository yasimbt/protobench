//! The `project.toml` data model.
//!
//! Every section has defaults so that a minimal project only needs a schema,
//! a framing section and a codec section. See `docs/CONFIG_REFERENCE.md`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Current `config_version`. Older files are migrated on load.
pub const CONFIG_VERSION: u32 = 1;

/// Root of `project.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProjectConfig {
    /// Schema version of this file.
    pub config_version: u32,
    /// Project name, used in titles and recording file names.
    pub name: String,
    /// Free-form description.
    pub description: String,
    /// Where the Protobuf schema comes from.
    pub schema: SchemaConfig,
    /// Connection profiles.
    pub connection: ConnectionConfig,
    /// Byte-level framing.
    pub framing: FramingConfig,
    /// Message dispatch.
    pub codec: CodecConfig,
    /// Request/response correlation.
    pub correlation: CorrelationConfig,
    /// Sample timestamps.
    pub timestamp: TimestampConfig,
    /// Field metadata overlay.
    pub fields: Vec<FieldMeta>,
    /// Command buttons.
    pub commands: Vec<CommandConfig>,
    /// Telemetry subscriptions.
    pub telemetry: Vec<TelemetryConfig>,
    /// Derived (computed) channels.
    pub derived: Vec<DerivedConfig>,
    /// Built-in device simulator.
    pub simulator: SimulatorConfig,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            name: "Untitled".into(),
            description: String::new(),
            schema: SchemaConfig::default(),
            connection: ConnectionConfig::default(),
            framing: FramingConfig::default(),
            codec: CodecConfig::default(),
            correlation: CorrelationConfig::default(),
            timestamp: TimestampConfig::default(),
            fields: Vec::new(),
            commands: Vec::new(),
            telemetry: Vec::new(),
            derived: Vec::new(),
            simulator: SimulatorConfig::default(),
        }
    }
}

impl ProjectConfig {
    /// Looks up a connection profile by name.
    pub fn profile(&self, name: &str) -> Option<&ConnectionProfile> {
        self.connection.profiles.iter().find(|p| p.name == name)
    }

    /// The default profile: `connection.profile`, else the first one.
    pub fn default_profile(&self) -> Option<&ConnectionProfile> {
        match &self.connection.profile {
            Some(name) => self.profile(name),
            None => self.connection.profiles.first(),
        }
    }

    /// Looks up a command by name.
    pub fn command(&self, name: &str) -> Option<&CommandConfig> {
        self.commands.iter().find(|c| c.name == name)
    }

    /// Looks up field metadata by path.
    pub fn field_meta(&self, path: &str) -> Option<&FieldMeta> {
        self.fields.iter().find(|f| f.path == path)
    }
}

/// `[schema]`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SchemaConfig {
    /// `.proto` files, relative to the project folder.
    pub proto_files: Vec<String>,
    /// Include paths, relative to the project folder.
    pub include_paths: Vec<String>,
    /// A compiled `FileDescriptorSet`, as an alternative to `proto_files`.
    pub descriptor_set: Option<String>,
}

/// `[connection]`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ConnectionConfig {
    /// Name of the default profile.
    pub profile: Option<String>,
    /// All profiles.
    pub profiles: Vec<ConnectionProfile>,
}

/// Transport kinds.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// Serial port / USB CDC.
    #[default]
    Serial,
    /// TCP client.
    TcpClient,
    /// TCP server (accepts one client at a time).
    TcpServer,
    /// UDP socket.
    Udp,
    /// In-process loopback to the built-in simulator.
    Sim,
    /// Plays back a `.pbsession` recording.
    Replay,
}

impl TransportKind {
    /// The name used in `project.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            TransportKind::Serial => "serial",
            TransportKind::TcpClient => "tcp_client",
            TransportKind::TcpServer => "tcp_server",
            TransportKind::Udp => "udp",
            TransportKind::Sim => "sim",
            TransportKind::Replay => "replay",
        }
    }
}

/// Serial parity.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Parity {
    /// No parity bit.
    #[default]
    None,
    /// Odd parity.
    Odd,
    /// Even parity.
    Even,
}

/// Serial flow control.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowControl {
    /// None.
    #[default]
    None,
    /// XON/XOFF.
    Software,
    /// RTS/CTS.
    Hardware,
}

/// `[[connection.profiles]]`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ConnectionProfile {
    /// Profile name.
    pub name: String,
    /// Transport kind.
    pub transport: TransportKind,
    /// Serial port name, or `"auto"` to pick by USB VID/PID.
    pub port: String,
    /// USB vendor ID for `port = "auto"`.
    pub usb_vid: Option<u16>,
    /// USB product ID for `port = "auto"`.
    pub usb_pid: Option<u16>,
    /// Baud rate.
    pub baud: u32,
    /// 5, 6, 7 or 8.
    pub data_bits: u8,
    /// Parity.
    pub parity: Parity,
    /// 1 or 2.
    pub stop_bits: u8,
    /// Flow control.
    pub flow_control: FlowControl,
    /// DTR level after opening.
    pub dtr_on_open: bool,
    /// RTS level after opening.
    pub rts_on_open: bool,
    /// Reopen the link after errors or unplugging, with backoff.
    pub auto_reconnect: bool,
    /// Duration of the "pulse reset" action.
    pub reset_pulse_ms: u64,
    /// Remote host for `tcp_client`.
    pub host: String,
    /// TCP port for `tcp_client` and `tcp_server`.
    pub tcp_port: u16,
    /// Local bind address: `tcp_server` (host part) and `udp` (`host:port`).
    pub bind: Option<String>,
    /// UDP destination `host:port`. Defaults to the sender of the last datagram.
    pub target: Option<String>,
    /// Recording to play back, for `replay`.
    pub file: Option<String>,
    /// Replay speed: 1.0 = real time, 0 = as fast as possible.
    pub speed: f64,
}

impl Default for ConnectionProfile {
    fn default() -> Self {
        Self {
            name: "default".into(),
            transport: TransportKind::Serial,
            port: "auto".into(),
            usb_vid: None,
            usb_pid: None,
            baud: 115_200,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: 1,
            flow_control: FlowControl::None,
            dtr_on_open: true,
            rts_on_open: false,
            auto_reconnect: true,
            reset_pulse_ms: 100,
            host: "127.0.0.1".into(),
            tcp_port: 9000,
            bind: None,
            target: None,
            file: None,
            speed: 1.0,
        }
    }
}

impl ConnectionProfile {
    /// Short human-readable endpoint, e.g. `/dev/ttyACM0 @ 921600`.
    pub fn endpoint(&self) -> String {
        match self.transport {
            TransportKind::Serial => format!("{} @ {}", self.port, self.baud),
            TransportKind::TcpClient => format!("tcp://{}:{}", self.host, self.tcp_port),
            TransportKind::TcpServer => format!(
                "listen tcp://{}:{}",
                self.bind.as_deref().unwrap_or("0.0.0.0"),
                self.tcp_port
            ),
            TransportKind::Udp => format!(
                "udp {} -> {}",
                self.bind.as_deref().unwrap_or("0.0.0.0:0"),
                self.target.as_deref().unwrap_or("(last sender)")
            ),
            TransportKind::Sim => "simulator (loopback)".into(),
            TransportKind::Replay => {
                format!("replay {}", self.file.as_deref().unwrap_or("(no file)"))
            }
        }
    }
}

/// Framing types.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FramingType {
    /// COBS with a 0x00 delimiter.
    #[default]
    Cobs,
    /// SLIP (RFC 1055).
    Slip,
    /// Varint length prefix.
    VarintLen,
    /// Fixed-width length prefix.
    FixedLen,
    /// No framing: every transport read (e.g. a UDP datagram) is one frame.
    None,
}

/// Byte order.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Endian {
    /// Least-significant byte first.
    #[default]
    Little,
    /// Most-significant byte first.
    Big,
}

/// CRC variants.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CrcKind {
    /// No CRC.
    #[default]
    None,
    /// CRC-8 (SMBus: poly 0x07, init 0).
    Crc8,
    /// CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF).
    Crc16CcittFalse,
    /// CRC-16/MODBUS (poly 0x8005 reflected, init 0xFFFF).
    Crc16Modbus,
    /// CRC-32 (ISO-HDLC, as in zlib).
    Crc32,
}

/// `[framing]`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FramingConfig {
    /// Framing type.
    #[serde(rename = "type")]
    pub kind: FramingType,
    /// Length-prefix width for `fixed_len`: 1, 2 or 4.
    pub len_bytes: u8,
    /// Length-prefix byte order for `fixed_len`.
    pub endian: Endian,
    /// Optional sync word before the length (length-prefixed framings only).
    pub sync_word: Vec<u8>,
    /// CRC appended to the payload.
    pub crc: CrcKind,
    /// CRC byte order.
    pub crc_endian: Endian,
    /// Largest accepted frame (payload + CRC) in bytes.
    pub max_frame_len: usize,
    /// Printable lines outside frames go to the text console.
    pub text_passthrough: bool,
}

impl Default for FramingConfig {
    fn default() -> Self {
        Self {
            kind: FramingType::Cobs,
            len_bytes: 2,
            endian: Endian::Little,
            sync_word: Vec::new(),
            crc: CrcKind::None,
            crc_endian: Endian::Little,
            max_frame_len: 1024,
            text_passthrough: false,
        }
    }
}

/// Dispatch modes.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodecMode {
    /// One root message per direction with a `oneof`.
    #[default]
    Envelope,
    /// A message-ID prefix selects the type.
    IdPrefix,
    /// One fixed type per direction.
    Fixed,
}

/// Width of the message ID in `id_prefix` mode.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "toml::Value", into = "toml::Value")]
pub enum IdWidth {
    /// One byte.
    #[default]
    U8,
    /// Two bytes, little-endian.
    U16,
    /// Protobuf varint.
    Varint,
}

impl TryFrom<toml::Value> for IdWidth {
    type Error = String;
    fn try_from(v: toml::Value) -> Result<Self, Self::Error> {
        match v {
            toml::Value::Integer(1) => Ok(IdWidth::U8),
            toml::Value::Integer(2) => Ok(IdWidth::U16),
            toml::Value::String(s) if s == "varint" => Ok(IdWidth::Varint),
            other => Err(format!("id_width must be 1, 2 or \"varint\", got {other}")),
        }
    }
}

impl From<IdWidth> for toml::Value {
    fn from(w: IdWidth) -> Self {
        match w {
            IdWidth::U8 => toml::Value::Integer(1),
            IdWidth::U16 => toml::Value::Integer(2),
            IdWidth::Varint => toml::Value::String("varint".into()),
        }
    }
}

/// `[codec]`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CodecConfig {
    /// Dispatch mode.
    pub mode: CodecMode,
    /// Type sent to the device (envelope and fixed).
    pub tx_root: Option<String>,
    /// Type received from the device (envelope and fixed).
    pub rx_root: Option<String>,
    /// ID width (id_prefix).
    pub id_width: IdWidth,
    /// ID → full type name (id_prefix). Keys are decimal or `0x` hex.
    pub ids: BTreeMap<String, String>,
}

/// `[correlation]`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CorrelationConfig {
    /// Field path in the tx message that carries the sequence ID.
    pub request_field: Option<String>,
    /// Field path in the rx message that echoes it.
    pub response_field: Option<String>,
    /// Default response timeout.
    pub default_timeout_ms: u64,
    /// Default number of retries after a timeout.
    pub default_retries: u32,
}

impl Default for CorrelationConfig {
    fn default() -> Self {
        Self {
            request_field: None,
            response_field: None,
            default_timeout_ms: 500,
            default_retries: 0,
        }
    }
}

/// Timestamp sources.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimestampSource {
    /// Host receive time.
    #[default]
    Host,
    /// A field in the message.
    Device,
}

/// Time units.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimeUnit {
    /// Microseconds.
    #[default]
    Us,
    /// Milliseconds.
    Ms,
    /// Seconds.
    S,
}

impl TimeUnit {
    /// Seconds per unit.
    pub fn seconds(self) -> f64 {
        match self {
            TimeUnit::Us => 1e-6,
            TimeUnit::Ms => 1e-3,
            TimeUnit::S => 1.0,
        }
    }
}

/// `[timestamp]`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TimestampConfig {
    /// Where sample times come from.
    pub source: TimestampSource,
    /// Field path holding the device timestamp.
    pub field: Option<String>,
    /// Unit of the device timestamp.
    pub unit: TimeUnit,
    /// Counter width for wrap handling; 0 = no wrap handling.
    pub wrap_bits: u32,
}

/// `[[fields]]` metadata overlay.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FieldMeta {
    /// Field path, e.g. `telemetry.vout_raw` or `telemetry.adc[2]`.
    pub path: String,
    /// Display name.
    pub display: Option<String>,
    /// Engineering unit.
    pub unit: Option<String>,
    /// Multiplier applied to the raw value.
    pub scale: Option<f64>,
    /// Added after scaling.
    pub offset: Option<f64>,
    /// Format string such as `{:.2}`.
    pub format: Option<String>,
    /// Below this the value shows as a warning.
    pub warn_min: Option<f64>,
    /// Above this the value shows as a warning.
    pub warn_max: Option<f64>,
    /// Below this the value shows as an alarm.
    pub min: Option<f64>,
    /// Above this the value shows as an alarm.
    pub max: Option<f64>,
}

/// `[[commands]]`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CommandConfig {
    /// Button label; also how sequences refer to the command.
    pub name: String,
    /// Group heading in the command bar.
    pub group: String,
    /// Keyboard shortcut, e.g. `F1` or `Ctrl+S`.
    pub shortcut: Option<String>,
    /// Oneof member of `tx_root` or a full type name.
    pub message: String,
    /// Field values (Protobuf JSON mapping).
    pub payload: toml::Table,
    /// Payload fields the user edits before sending.
    pub prompt: Vec<String>,
    /// Expected reply (oneof member or type); omit for fire-and-forget.
    pub expect: Option<String>,
    /// Response timeout; defaults to `correlation.default_timeout_ms`.
    pub timeout_ms: Option<u64>,
    /// Retries after a timeout; defaults to `correlation.default_retries`.
    pub retries: Option<u32>,
    /// Ask for confirmation before sending.
    pub dangerous: bool,
    /// Button colour, `#rrggbb`.
    #[serde(alias = "color")]
    pub colour: Option<String>,
    /// Shows a periodic-send toggle that sends every N ms.
    pub periodic_ms: Option<u64>,
}

impl Default for CommandConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            group: "General".into(),
            shortcut: None,
            message: String::new(),
            payload: toml::Table::new(),
            prompt: Vec::new(),
            expect: None,
            timeout_ms: None,
            retries: None,
            dangerous: false,
            colour: None,
            periodic_ms: None,
        }
    }
}

/// Telemetry modes.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryMode {
    /// Extract channels from every matching message.
    #[default]
    Stream,
    /// Send a request on a timer and extract from the response.
    Poll,
    /// Capture N samples after a trigger.
    Triggered,
}

/// `[[telemetry]]`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TelemetryConfig {
    /// Name shown in the UI.
    pub name: String,
    /// Mode.
    pub mode: TelemetryMode,
    /// Message to subscribe to (stream and triggered).
    pub message: Option<String>,
    /// Channel field paths.
    pub channels: Vec<String>,
    /// Request command message (poll).
    pub request: Option<String>,
    /// Request payload (poll).
    pub request_payload: toml::Table,
    /// Expected response (poll).
    pub response: Option<String>,
    /// Poll interval.
    pub interval_ms: u64,
    /// Condition expression that arms a triggered capture, e.g.
    /// `telemetry.vout_raw > 2000`. Without it the capture is armed manually.
    pub trigger: Option<String>,
    /// Samples per triggered capture.
    pub samples: usize,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            mode: TelemetryMode::Stream,
            message: None,
            channels: Vec::new(),
            request: None,
            request_payload: toml::Table::new(),
            response: None,
            interval_ms: 1000,
            trigger: None,
            samples: 1000,
        }
    }
}

/// `[[derived]]`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DerivedConfig {
    /// Channel name.
    pub name: String,
    /// Expression over field paths and channel names.
    pub expr: String,
    /// Unit.
    pub unit: Option<String>,
    /// Format string.
    pub format: Option<String>,
}

/// `[simulator]`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SimulatorConfig {
    /// Delay before each response.
    pub latency_ms: u64,
    /// Probability of dropping a response or telemetry frame.
    pub drop_rate: f64,
    /// Probability of flipping a byte after the CRC is computed.
    pub corrupt_rate: f64,
    /// Probability of inserting random bytes between frames.
    pub garbage_rate: f64,
    /// Interval of the simulated firmware log lines; 0 disables them.
    pub log_interval_ms: u64,
    /// PRNG seed.
    pub seed: u64,
    /// Telemetry streams.
    pub streams: Vec<SimStream>,
    /// Command responses.
    pub responses: Vec<SimResponse>,
}

impl Default for SimulatorConfig {
    fn default() -> Self {
        Self {
            latency_ms: 0,
            drop_rate: 0.0,
            corrupt_rate: 0.0,
            garbage_rate: 0.0,
            log_interval_ms: 1000,
            seed: 1,
            streams: Vec::new(),
            responses: Vec::new(),
        }
    }
}

/// `[[simulator.streams]]`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SimStream {
    /// Oneof member of `rx_root` or a full type name.
    pub message: String,
    /// Messages per second.
    pub rate_hz: f64,
    /// Whether the stream runs from the start.
    pub start_enabled: bool,
    /// Field → generator expression or constant.
    pub fields: toml::Table,
}

impl Default for SimStream {
    fn default() -> Self {
        Self {
            message: String::new(),
            rate_hz: 10.0,
            start_enabled: true,
            fields: toml::Table::new(),
        }
    }
}

/// `[[simulator.responses]]`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SimResponse {
    /// Request oneof member or type that triggers this response.
    pub on: String,
    /// Reply oneof member or type.
    pub reply: String,
    /// Reply field values; strings may be generator expressions.
    pub payload: toml::Table,
    /// `toggle_stream:<msg>`, `enable_stream:<msg>` or `disable_stream:<msg>`.
    pub action: Option<String>,
}
