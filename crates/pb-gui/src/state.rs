//! Application state and actions, separate from rendering so they can be
//! unit-tested. Every button in the UI calls one of these methods.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use pb_core::bench::Bench;
use pb_core::codec::DecodedMessage;
use pb_core::commands::{build_command, CommandHistory, HistoryEntry, SendRequest};
use pb_core::config::{CommandConfig, ConnectionProfile, ProjectConfig, MAIN_DEVICE};
use pb_core::diag::Diagnostic;
use pb_core::project::Project;
use pb_core::recorder::{export_csv, recording_path, CsvMode};
use pb_core::sequence::{OperatorInput, OperatorPrompt};
use pb_core::session::{BadFrame, ConnState, Direction, RequestOutcome, Session, SessionEvent};
use pb_core::telemetry::TelemetryStore;

/// Settings (saved in the app's storage, not per project).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Dark theme.
    pub dark: bool,
    /// Ring-buffer capacity per channel.
    pub capacity: usize,
    /// Repaint rate cap while connected.
    pub fps_cap: u32,
    /// Recordings folder; default `<project>/recordings`.
    pub recordings_dir: Option<PathBuf>,
    /// Foxglove WebSocket server port (8765 is Foxglove's default).
    pub foxglove_port: u16,
    /// Address the Foxglove server binds; `0.0.0.0` serves other machines.
    pub foxglove_bind: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dark: true,
            capacity: pb_core::telemetry::DEFAULT_CAPACITY,
            fps_cap: 60,
            recordings_dir: None,
            foxglove_port: 8765,
            foxglove_bind: "127.0.0.1".into(),
        }
    }
}

/// Result badge on a command button.
#[derive(Debug, Clone, PartialEq)]
pub enum Badge {
    /// Waiting for a reply.
    Pending,
    /// Fire-and-forget written.
    Sent,
    /// Reply received.
    Ok {
        /// Latency in ms.
        latency_ms: f64,
    },
    /// No reply.
    Timeout,
    /// Error with reason.
    Error(String),
}

/// A dialog waiting for the user.
#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    /// Edit prompt fields before sending.
    Prompt {
        /// Device the command belongs to.
        device: String,
        /// Command name.
        command: String,
        /// (field path, editable text) pairs.
        fields: Vec<(String, String)>,
    },
    /// Confirm a dangerous command.
    Confirm {
        /// Device the command belongs to.
        device: String,
        /// Command name.
        command: String,
        /// Overrides collected by a prompt.
        overrides: Vec<(String, Json)>,
    },
    /// `prompt_operator` from a running sequence.
    Operator {
        /// Instruction.
        message: String,
        /// Values to enter.
        fields: Vec<OperatorField>,
    },
    /// The project changed on disk.
    Reload {
        /// What changed.
        what: String,
    },
}

/// Answer to an operator prompt: the entered values, or None for Fail.
pub type OperatorReply = Sender<Option<Vec<(String, Json)>>>;

/// One input field of an operator dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorField {
    /// Spec from the sequence.
    pub input: OperatorInput,
    /// Text being edited.
    pub text: String,
}

impl OperatorField {
    /// The parsed value, or why it is invalid.
    pub fn value(&self) -> Result<Json, String> {
        self.input.parse_text(&self.text)
    }
}

/// What happened when a command button was clicked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickOutcome {
    /// The prompt dialog opened.
    Prompt,
    /// The confirmation dialog opened.
    Confirm,
    /// The command was queued.
    Sent(u64),
    /// It could not be sent.
    Failed(String),
}

/// A message in the log.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Running number.
    pub n: u64,
    /// Device it came from or went to.
    pub device: Arc<str>,
    /// Direction.
    pub dir: Direction,
    /// Message.
    pub msg: Arc<DecodedMessage>,
}

/// A text-console line.
#[derive(Debug, Clone, PartialEq)]
pub struct TextEntry {
    /// Device it came from.
    pub device: Arc<str>,
    /// Host seconds.
    pub t: f64,
    /// The line.
    pub line: String,
    /// Detected level.
    pub level: Level,
}

/// Log levels detected from prefixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// `E:` / `[ERROR]`
    Error,
    /// `W:` / `[WARN]`
    Warn,
    /// `I:` / `[INFO]`
    Info,
    /// `D:` / `[DEBUG]`
    Debug,
    /// Anything else.
    Plain,
}

/// Detects the level of a firmware log line.
pub fn detect_level(line: &str) -> Level {
    let l = line.trim_start();
    let upper: String = l.chars().take(10).collect::<String>().to_ascii_uppercase();
    let starts = |p: &str| upper.starts_with(p);
    if starts("E:") || starts("[E]") || starts("[ERR") || starts("ERROR") {
        Level::Error
    } else if starts("W:") || starts("[W]") || starts("[WARN") || starts("WARN") {
        Level::Warn
    } else if starts("I:") || starts("[I]") || starts("[INFO") || starts("INFO") {
        Level::Info
    } else if starts("D:")
        || starts("[D]")
        || starts("[DEBUG")
        || starts("DEBUG")
        || starts("[TRACE")
    {
        Level::Debug
    } else {
        Level::Plain
    }
}

/// Raw console chunk.
#[derive(Debug, Clone)]
pub struct RawEntry {
    /// Device it came from or went to.
    pub device: Arc<str>,
    /// Host seconds.
    pub t: f64,
    /// Direction.
    pub dir: Direction,
    /// Bytes.
    pub bytes: Arc<[u8]>,
}

/// A marker.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    /// Host seconds.
    pub t: f64,
    /// Label.
    pub text: String,
}

/// Background job results delivered to the UI thread.
#[derive(Debug)]
pub enum JobResult {
    /// A notification line.
    Info(String),
    /// An error.
    Error(String),
}

/// Watches project files for changes.
#[derive(Debug, Default)]
struct Watcher {
    files: Vec<(PathBuf, Option<SystemTime>)>,
    last_check: Option<Instant>,
}

impl Watcher {
    /// Watches the project's config and schema files, and those of every
    /// `[[devices]]` project.
    fn new(project: &Project) -> Watcher {
        let mut files = Vec::new();
        for d in project.all_devices() {
            let p = d.project;
            files.push(p.config_path());
            for f in &p.config.schema.proto_files {
                files.push(p.dir.join(f));
            }
            if let Some(ds) = &p.config.schema.descriptor_set {
                files.push(p.dir.join(ds));
            }
        }
        files.sort();
        files.dedup();
        Watcher {
            files: files
                .into_iter()
                .map(|f| {
                    let m = mtime(&f);
                    (f, m)
                })
                .collect(),
            last_check: None,
        }
    }

    /// Returns changed files (at most once per second).
    fn poll(&mut self) -> Vec<PathBuf> {
        if self
            .last_check
            .is_some_and(|t| t.elapsed().as_secs_f64() < 1.0)
        {
            return Vec::new();
        }
        self.last_check = Some(Instant::now());
        let mut changed = Vec::new();
        for (f, m) in &mut self.files {
            let now = mtime(f);
            if now != *m {
                *m = now;
                changed.push(f.clone());
            }
        }
        changed
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Maximum entries kept in the UI logs.
pub const MSG_LOG_CAP: usize = 5000;
/// Maximum raw chunks kept.
pub const RAW_LOG_CAP: usize = 20_000;
/// Maximum text lines kept.
pub const TEXT_LOG_CAP: usize = 10_000;

/// Everything the UI shows and does.
pub struct AppState {
    /// Open project.
    pub project: Option<Project>,
    /// Errors and warnings (project plus runtime).
    pub diagnostics: Vec<Diagnostic>,
    /// The running devices: one session each, sharing a clock and a
    /// telemetry store.
    pub bench: Bench,
    /// Session of the project's own device (`main`), while it runs.
    pub session: Option<Arc<Session>>,
    events: Vec<(Arc<str>, Receiver<SessionEvent>)>,
    /// Selected connection profile of `main`.
    pub profile: Option<String>,
    /// Port override for `main`'s profile (serial port or host:port).
    pub port_override: String,
    /// Profiles chosen for other devices (default: their `[[devices]]`
    /// profile).
    pub device_profiles: HashMap<String, String>,
    /// Badges by command: the name for `main`, `device:name` for others.
    pub badges: HashMap<String, Badge>,
    requests: HashMap<u64, String>,
    /// Sent commands.
    pub history: CommandHistory,
    /// Decoded messages (rx and tx).
    pub msg_log: VecDeque<LogEntry>,
    msg_counter: u64,
    /// Raw bytes.
    pub raw_log: VecDeque<RawEntry>,
    /// Text lines.
    pub text_log: VecDeque<TextEntry>,
    /// Markers.
    pub markers: Vec<Marker>,
    /// Notes typed by the user (saved into recordings as markers).
    pub notes: String,
    /// Open dialog.
    pub dialog: Option<Dialog>,
    /// Status-line notifications, newest last.
    pub notifications: VecDeque<(Instant, String)>,
    /// Settings.
    pub settings: Settings,
    /// Recent projects, newest first.
    pub recent: Vec<PathBuf>,
    /// Current recording file.
    pub recording: Option<PathBuf>,
    /// Selected message for the inspector (log entry number).
    pub selected: Option<u64>,
    /// Field path selected in the inspector tree.
    pub selected_field: Option<Vec<String>>,
    /// Last connection state of `main`.
    pub conn_state: ConnState,
    /// Last connection state of every device that has run.
    pub device_states: std::collections::BTreeMap<String, ConnState>,
    /// Warning about changed field numbers after a proto reload.
    pub proto_diff: Option<String>,
    watcher: Option<Watcher>,
    jobs_tx: Sender<JobResult>,
    jobs_rx: Receiver<JobResult>,
    /// Answer channel for an operator prompt from a running sequence.
    pub operator_reply: Option<OperatorReply>,
    /// Text last entered per input variable, pre-filled next time.
    pub operator_memory: HashMap<String, String>,
    /// Foxglove server wanted (survives reconnects).
    pub foxglove_enabled: bool,
    foxglove: Option<pb_core::foxglove::FoxgloveServer>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(Settings::default())
    }
}

impl AppState {
    /// Creates an empty state.
    pub fn new(settings: Settings) -> Self {
        let (jobs_tx, jobs_rx) = crossbeam_channel::unbounded();
        Self {
            project: None,
            diagnostics: Vec::new(),
            bench: Bench::new(settings.capacity),
            session: None,
            events: Vec::new(),
            profile: None,
            port_override: String::new(),
            device_profiles: HashMap::new(),
            badges: HashMap::new(),
            requests: HashMap::new(),
            history: CommandHistory::default(),
            msg_log: VecDeque::new(),
            msg_counter: 0,
            raw_log: VecDeque::new(),
            text_log: VecDeque::new(),
            markers: Vec::new(),
            notes: String::new(),
            dialog: None,
            notifications: VecDeque::new(),
            settings,
            recent: Vec::new(),
            recording: None,
            selected: None,
            selected_field: None,
            conn_state: ConnState::Disconnected,
            device_states: Default::default(),
            proto_diff: None,
            watcher: None,
            jobs_tx,
            jobs_rx,
            operator_reply: None,
            operator_memory: HashMap::new(),
            foxglove_enabled: false,
            foxglove: None,
        }
    }

    /// Adds a status notification.
    pub fn notify(&mut self, s: impl Into<String>) {
        let s = s.into();
        tracing::info!("{s}");
        self.notifications.push_back((Instant::now(), s));
        while self.notifications.len() > 50 {
            self.notifications.pop_front();
        }
    }

    /// Sender for background jobs.
    pub fn jobs(&self) -> Sender<JobResult> {
        self.jobs_tx.clone()
    }

    // ---------- project ----------

    /// Opens a project folder. Errors in it are shown, not fatal.
    pub fn open_project(&mut self, dir: &Path) {
        self.disconnect();
        let p = Project::open(dir);
        self.diagnostics = p.diagnostics.clone();
        self.profile = p.config.default_profile().map(|x| x.name.clone());
        self.device_profiles.clear();
        self.device_states.clear();
        self.watcher = Some(Watcher::new(&p));
        let canon = std::fs::canonicalize(&p.dir).unwrap_or_else(|_| p.dir.clone());
        self.recent.retain(|r| r != &canon);
        self.recent.insert(0, canon);
        self.recent.truncate(10);
        self.badges.clear();
        self.history.clear();
        self.clear_logs();
        let n_err = p.errors().count();
        self.notify(if n_err == 0 {
            format!("opened {}", p.config.name)
        } else {
            format!("opened {} with {n_err} error(s)", p.dir.display())
        });
        self.project = Some(p);
    }

    /// Reloads the project from disk, keeping the connection profile.
    /// Warns if field numbers of existing fields changed.
    pub fn reload_project(&mut self) {
        let Some(old) = self.project.as_ref() else {
            return;
        };
        let dir = old.dir.clone();
        let old_schema = old.schema.clone();
        let profile = self.profile.clone();
        let device_profiles = self.device_profiles.clone();
        let running: Vec<String> = self
            .bench
            .devices()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        self.open_project(&dir);
        self.device_profiles = device_profiles;
        if let (Some(a), Some(b)) = (
            old_schema,
            self.project.as_ref().and_then(|p| p.schema.clone()),
        ) {
            let d = schema_diff(&a, &b);
            self.proto_diff = (!d.is_empty()).then(|| d.join("\n"));
            if let Some(w) = &self.proto_diff {
                self.notify(format!("schema changed:\n{w}"));
            }
        }
        if profile.is_some() {
            self.profile = profile;
        }
        for d in running {
            if let Err(e) = self.connect_device(&d) {
                self.notify(format!("reconnect {d}: {e}"));
            }
        }
    }

    /// Checks watched files; opens the reload dialog when they changed.
    pub fn check_files(&mut self) {
        let Some(w) = self.watcher.as_mut() else {
            return;
        };
        let changed = w.poll();
        if !changed.is_empty() && self.dialog.is_none() {
            let what = changed
                .iter()
                .map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(", ");
            self.dialog = Some(Dialog::Reload { what });
        }
    }

    /// Clears message, raw and text logs.
    pub fn clear_logs(&mut self) {
        self.msg_log.clear();
        self.raw_log.clear();
        self.text_log.clear();
        self.markers.clear();
        self.selected = None;
    }

    /// Connection profiles of the project.
    pub fn profiles(&self) -> Vec<ConnectionProfile> {
        self.project
            .as_ref()
            .map(|p| p.config.connection.profiles.clone())
            .unwrap_or_default()
    }

    // ---------- connection ----------

    /// Connects `main` with a profile (other devices keep running).
    pub fn connect(&mut self, profile: &str) -> Result<(), String> {
        self.profile = Some(profile.to_string());
        self.connect_device(MAIN_DEVICE)
    }

    /// Connects every device of the project: `main` with the selected
    /// profile, the others with theirs.
    pub fn connect_all(&mut self) -> Result<(), String> {
        let names: Vec<String> = self
            .project
            .as_ref()
            .ok_or("no project open")?
            .all_devices()
            .iter()
            .map(|d| d.name.to_string())
            .collect();
        let mut errors = Vec::new();
        for n in names {
            if let Err(e) = self.connect_device(&n) {
                errors.push(format!("{n}: {e}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// The profile a device connects with.
    pub fn device_profile_name(&self, device: &str) -> Option<String> {
        if device == MAIN_DEVICE {
            return self.profile.clone();
        }
        let p = self.project.as_ref()?;
        self.device_profiles.get(device).cloned().or_else(|| {
            p.device_profile(device, None)
                .ok()
                .map(|prof| prof.name.clone())
        })
    }

    /// Connects (or reconnects) one device. The I/O happens on its session
    /// thread.
    pub fn connect_device(&mut self, device: &str) -> Result<(), String> {
        let p = self.project.as_ref().ok_or("no project open")?;
        if p.has_errors() {
            return Err("fix the project errors before connecting".into());
        }
        let profile = self.device_profile_name(device);
        let port = (device == MAIN_DEVICE && !self.port_override.trim().is_empty())
            .then(|| self.port_override.trim().to_string());
        if self.bench.is_empty() {
            // Nothing runs: start a fresh clock and store.
            self.bench = Bench::new(self.settings.capacity);
        }
        self.bench.telemetry().set_capacity(self.settings.capacity);
        self.drop_device(device);
        let p = self.project.as_ref().ok_or("no project open")?;
        let d = self
            .bench
            .start(
                p,
                device,
                profile.as_deref(),
                port,
                &|dp: &Project| pb_sim::sim_factory(dp),
                |_| {},
            )
            .map_err(|e| e.to_string())?;
        let session = d.session.clone();
        let endpoint = d.profile.endpoint();
        let name: Arc<str> = Arc::from(device);
        self.events.push((name, session.subscribe()));
        self.device_states
            .insert(device.to_string(), ConnState::Connecting);
        if device == MAIN_DEVICE {
            self.session = Some(session.clone());
            self.conn_state = ConnState::Connecting;
            if let Some(pr) = profile {
                self.profile = Some(pr);
            }
        }
        self.notify(if device == MAIN_DEVICE {
            format!("connecting: {endpoint}")
        } else {
            format!("connecting {device}: {endpoint}")
        });
        if self.foxglove_enabled {
            if self.foxglove.is_none() {
                if let Err(e) = self.start_foxglove() {
                    self.notify(format!("Foxglove server: {e}"));
                }
            } else if let Some(f) = &self.foxglove {
                f.attach_device(session.subscribe(), session.wall_offset_ns(), device);
            }
        }
        Ok(())
    }

    /// Stops one device's session and forgets its events and badges.
    fn drop_device(&mut self, device: &str) {
        let was = self.bench.get(device).is_some();
        self.bench.stop(device);
        self.events.retain(|(n, _)| &**n != device);
        let prefix = format!("{device}:");
        if device == MAIN_DEVICE {
            let others: Vec<String> = self
                .project
                .as_ref()
                .map(|p| p.devices.iter().map(|d| format!("{}:", d.name)).collect())
                .unwrap_or_default();
            self.badges
                .retain(|k, _| others.iter().any(|o| k.starts_with(o.as_str())));
            self.session = None;
            self.conn_state = ConnState::Disconnected;
        } else {
            self.badges.retain(|k, _| !k.starts_with(&prefix));
        }
        if was {
            self.device_states
                .insert(device.to_string(), ConnState::Disconnected);
        }
        if self.bench.is_empty() {
            self.foxglove = None;
            self.recording = None;
            self.requests.clear();
        }
    }

    /// Disconnects one device (the others keep running).
    pub fn disconnect_device(&mut self, device: &str) {
        if self.bench.get(device).is_some() {
            self.drop_device(device);
            self.notify(if device == MAIN_DEVICE {
                "disconnected".to_string()
            } else {
                format!("disconnected {device}")
            });
        }
    }

    /// The session of a running device.
    pub fn device_session(&self, device: &str) -> Option<Arc<Session>> {
        self.bench.session(device).cloned()
    }

    /// The config of a device's project.
    pub fn device_config(&self, device: &str) -> Option<&ProjectConfig> {
        self.project
            .as_ref()?
            .device(device)
            .map(|d| &d.project.config)
    }

    /// Names of the project's devices, `main` first.
    pub fn device_names(&self) -> Vec<String> {
        self.project
            .as_ref()
            .map(|p| p.all_devices().iter().map(|d| d.name.to_string()).collect())
            .unwrap_or_else(|| vec![MAIN_DEVICE.to_string()])
    }

    /// True if the project lists other devices.
    pub fn has_devices(&self) -> bool {
        self.project.as_ref().is_some_and(|p| !p.devices.is_empty())
    }

    /// Last known state of a device.
    pub fn device_state(&self, device: &str) -> ConnState {
        self.device_states
            .get(device)
            .cloned()
            .unwrap_or(ConnState::Disconnected)
    }

    /// True if a device is connected.
    pub fn device_connected(&self, device: &str) -> bool {
        self.bench
            .session(device)
            .is_some_and(|s| s.state().is_connected())
    }

    /// The telemetry store shared by every running device.
    pub fn store(&self) -> Option<Arc<TelemetryStore>> {
        (!self.bench.is_empty()).then(|| self.bench.telemetry().clone())
    }

    /// True while any device runs.
    pub fn running(&self) -> bool {
        !self.bench.is_empty()
    }

    // ---------- foxglove ----------

    /// Starts the Foxglove server with every device of the project (each
    /// with its own schema) and attaches the running ones.
    fn start_foxglove(&mut self) -> Result<(), String> {
        self.foxglove = None;
        if self.bench.is_empty() {
            return Err("not connected".into());
        }
        let p = self.project.as_ref().ok_or("no project open")?;
        let mut devices = Vec::new();
        for d in p.all_devices() {
            let schema = d.project.require_schema().map_err(|e| e.to_string())?;
            let codec =
                pb_core::codec::Codec::new(&d.project.config, &schema, pb_core::codec::Role::Host)
                    .map_err(|e| e.to_string())?;
            devices.push(pb_core::foxglove::FoxgloveDevice {
                name: d.name.to_string(),
                fds: schema.fds_bytes().to_vec(),
                topics: pb_core::topics::known_topics_on(d.name, &codec),
            });
        }
        let server = pb_core::foxglove::FoxgloveServer::start_devices(
            &self.settings.foxglove_bind,
            self.settings.foxglove_port,
            &format!("ProtoBench: {}", p.config.name),
            &devices,
        )
        .map_err(|e| e.to_string())?;
        for d in self.bench.devices() {
            server.attach_device(d.session.subscribe(), d.session.wall_offset_ns(), &d.name);
        }
        let addr = server.local_addr();
        self.foxglove = Some(server);
        self.notify(format!("Foxglove server on ws://{addr}"));
        Ok(())
    }

    /// Turns the Foxglove server on or off. While on, it follows the
    /// connection: it starts with the first device and stops when the last
    /// one disconnects.
    pub fn set_foxglove(&mut self, on: bool) -> Result<(), String> {
        self.foxglove_enabled = on;
        if !on {
            if self.foxglove.take().is_some() {
                self.notify("Foxglove server stopped");
            }
            return Ok(());
        }
        if self.running() {
            self.start_foxglove()
        } else {
            Ok(())
        }
    }

    /// Address and client count of the running Foxglove server.
    pub fn foxglove_status(&self) -> Option<(std::net::SocketAddr, usize)> {
        self.foxglove
            .as_ref()
            .map(|f| (f.local_addr(), f.client_count()))
    }

    /// Disconnects every device (stops any recording).
    pub fn disconnect(&mut self) {
        self.stop_recording();
        self.foxglove = None;
        let names: Vec<String> = self
            .bench
            .devices()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        let any = !names.is_empty();
        for n in names {
            self.drop_device(&n);
        }
        if any {
            self.notify("disconnected");
        }
        self.conn_state = ConnState::Disconnected;
        self.requests.clear();
    }

    /// True if `main` is connected.
    pub fn connected(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.state().is_connected())
    }

    // ---------- commands ----------

    fn command(&self, device: &str, name: &str) -> Option<CommandConfig> {
        self.device_config(device)?.command(name).cloned()
    }

    /// Badge key of a command: its name for `main`, `device:name` for
    /// another device.
    pub fn badge_key(device: &str, name: &str) -> String {
        if device == MAIN_DEVICE {
            name.to_string()
        } else {
            format!("{device}:{name}")
        }
    }

    /// Handles a click on a command button of `main`.
    pub fn click_command(&mut self, name: &str) -> ClickOutcome {
        self.click_command_on(MAIN_DEVICE, name)
    }

    /// Handles a click on a command button of `device`.
    pub fn click_command_on(&mut self, device: &str, name: &str) -> ClickOutcome {
        let Some(cmd) = self.command(device, name) else {
            return ClickOutcome::Failed(format!("no command `{name}`"));
        };
        if !cmd.prompt.is_empty() {
            let payload = pb_core::commands::table_to_json(&cmd.payload);
            let fields = cmd
                .prompt
                .iter()
                .map(|f| {
                    let v = pb_core::commands::get_json_path(&payload, f)
                        .map(|v| match v {
                            Json::String(s) => format!("{s:?}"),
                            other => other.to_string(),
                        })
                        .unwrap_or_default();
                    (f.clone(), v)
                })
                .collect();
            self.dialog = Some(Dialog::Prompt {
                device: device.to_string(),
                command: name.to_string(),
                fields,
            });
            return ClickOutcome::Prompt;
        }
        if cmd.dangerous {
            self.dialog = Some(Dialog::Confirm {
                device: device.to_string(),
                command: name.to_string(),
                overrides: Vec::new(),
            });
            return ClickOutcome::Confirm;
        }
        self.send_command_on(device, name, Vec::new())
    }

    /// OK in the prompt dialog.
    pub fn submit_prompt(&mut self) -> ClickOutcome {
        let Some(Dialog::Prompt {
            device,
            command,
            fields,
        }) = self.dialog.take()
        else {
            return ClickOutcome::Failed("no prompt open".into());
        };
        let overrides: Vec<(String, Json)> = fields
            .iter()
            .map(|(k, v)| (k.clone(), pb_core::commands::parse_value(v.trim())))
            .collect();
        if self.command(&device, &command).is_some_and(|c| c.dangerous) {
            self.dialog = Some(Dialog::Confirm {
                device,
                command,
                overrides,
            });
            return ClickOutcome::Confirm;
        }
        self.send_command_on(&device, &command, overrides)
    }

    /// Yes in the confirmation dialog.
    pub fn confirm(&mut self) -> ClickOutcome {
        match self.dialog.take() {
            Some(Dialog::Confirm {
                device,
                command,
                overrides,
            }) => self.send_command_on(&device, &command, overrides),
            other => {
                self.dialog = other;
                ClickOutcome::Failed("no confirmation pending".into())
            }
        }
    }

    /// Cancel in any dialog.
    pub fn cancel_dialog(&mut self) {
        if let Some(Dialog::Operator { .. }) = &self.dialog {
            if let Some(tx) = self.operator_reply.take() {
                let _ = tx.send(None);
            }
        }
        self.dialog = None;
    }

    /// Opens the dialog for a `prompt_operator` step. Fields are pre-filled
    /// with what the operator entered last, else the runner's value.
    pub fn open_operator(&mut self, prompt: OperatorPrompt, reply: OperatorReply) {
        let fields = prompt
            .inputs
            .into_iter()
            .map(|(input, v)| {
                let text = self
                    .operator_memory
                    .get(&input.var)
                    .cloned()
                    .or_else(|| v.as_ref().map(OperatorInput::to_text))
                    .unwrap_or_default();
                OperatorField { input, text }
            })
            .collect();
        self.operator_reply = Some(reply);
        self.dialog = Some(Dialog::Operator {
            message: prompt.message,
            fields,
        });
    }

    /// Answers a `prompt_operator` dialog from a running sequence. OK does
    /// nothing while a field is invalid.
    pub fn answer_operator(&mut self, ok: bool) {
        let answer = match (&self.dialog, ok) {
            (Some(Dialog::Operator { fields, .. }), true) => {
                let Ok(values) = fields
                    .iter()
                    .map(|f| f.value().map(|v| (f.input.var.clone(), v)))
                    .collect::<Result<Vec<_>, _>>()
                else {
                    return;
                };
                for f in fields {
                    self.operator_memory
                        .insert(f.input.var.clone(), f.text.clone());
                }
                Some(values)
            }
            _ => None,
        };
        if let Some(tx) = self.operator_reply.take() {
            let _ = tx.send(answer);
        }
        self.dialog = None;
    }

    /// Builds the request for a command of `device`.
    pub fn build_request_on(
        &self,
        device: &str,
        name: &str,
        overrides: &[(String, Json)],
    ) -> Result<SendRequest, String> {
        let cfg = self.device_config(device).ok_or("no project")?;
        let s = self.device_session(device).ok_or("not connected")?;
        let cmd = cfg
            .command(name)
            .ok_or_else(|| format!("no command `{name}`"))?;
        build_command(cfg, s.codec(), cmd, overrides, &Default::default())
            .map_err(|e| e.to_string())
    }

    /// Sends a command of `device` (no dialogs).
    pub fn send_command_on(
        &mut self,
        device: &str,
        name: &str,
        overrides: Vec<(String, Json)>,
    ) -> ClickOutcome {
        let req = match self.build_request_on(device, name, &overrides) {
            Ok(r) => r,
            Err(e) => {
                self.badges
                    .insert(Self::badge_key(device, name), Badge::Error(e.clone()));
                return ClickOutcome::Failed(e);
            }
        };
        let id = self.send_request_on(device, req);
        ClickOutcome::Sent(id)
    }

    /// Sends a prepared request to `device`, tracking its badge under
    /// [`AppState::badge_key`] of `req.name`.
    pub fn send_request_on(&mut self, device: &str, req: SendRequest) -> u64 {
        let key = Self::badge_key(device, &req.name);
        let Some(s) = self.device_session(device) else {
            self.badges
                .insert(key, Badge::Error("not connected".into()));
            return 0;
        };
        let inner = inner_json(&req.msg);
        self.history.push(HistoryEntry {
            host_ts_ns: s.now_ns(),
            device: device.to_string(),
            name: req.name.clone(),
            message: pb_core::codec::set_oneof_member(&req.msg).unwrap_or_else(|| {
                prost_reflect::ReflectMessage::descriptor(&req.msg)
                    .full_name()
                    .to_string()
            }),
            payload: inner,
            expect: req.expect.as_ref().map(|e| e.name()),
        });
        let h = s.send(req);
        self.requests.insert(h.id, key.clone());
        self.badges.insert(key, Badge::Pending);
        h.id
    }

    /// Resends a history entry to the device it went to.
    pub fn resend(&mut self, index: usize) -> Result<u64, String> {
        let e = self
            .history
            .iter()
            .nth(index)
            .cloned()
            .ok_or("no such entry")?;
        let cfg = self.device_config(&e.device).ok_or("no project")?;
        let s = self.device_session(&e.device).ok_or("not connected")?;
        let req = pb_core::commands::build_inline(
            cfg,
            s.codec(),
            &e.message,
            e.payload.clone(),
            e.expect.as_deref(),
            None,
            None,
        )
        .map_err(|e| e.to_string())?;
        let mut req = req;
        req.name = e.name.clone();
        Ok(self.send_request_on(&e.device, req))
    }

    /// Starts or stops periodic sending of a command of `device`.
    pub fn set_periodic_on(&mut self, device: &str, name: &str, on: bool) -> Result<(), String> {
        let s = self.device_session(device).ok_or("not connected")?;
        if !on {
            s.set_periodic(name, None);
            return Ok(());
        }
        let cmd = self.command(device, name).ok_or("no such command")?;
        let period = cmd.periodic_ms.ok_or("command has no periodic_ms")?;
        let req = self.build_request_on(device, name, &[])?;
        s.set_periodic(name, Some((req, period)));
        Ok(())
    }

    /// True if a command of `device` is sending periodically.
    pub fn is_periodic_on(&self, device: &str, name: &str) -> bool {
        self.device_session(device)
            .is_some_and(|s| s.periodic_keys().iter().any(|k| k == name))
    }

    // ---------- events ----------

    /// Drains session and job events. Call once per frame.
    pub fn poll(&mut self) {
        while let Ok(j) = self.jobs_rx.try_recv() {
            match j {
                JobResult::Info(s) => self.notify(s),
                JobResult::Error(e) => {
                    self.notify(format!("error: {e}"));
                    self.diagnostics.push(Diagnostic::error("", None, e));
                }
            }
        }
        let receivers = self.events.clone();
        // Bound the work per frame; the rest is picked up next frame.
        for (device, rx) in receivers {
            for _ in 0..20_000 {
                let Ok(ev) = rx.try_recv() else { break };
                self.handle_event(&device, ev);
            }
        }
    }

    fn handle_event(&mut self, device: &Arc<str>, ev: SessionEvent) {
        match ev {
            SessionEvent::State(s) => {
                self.notify(if &**device == MAIN_DEVICE {
                    s.label()
                } else {
                    format!("{device}: {}", s.label())
                });
                if &**device == MAIN_DEVICE {
                    self.conn_state = s.clone();
                }
                self.device_states.insert(device.to_string(), s);
            }
            SessionEvent::Message { dir, msg } => {
                self.msg_counter += 1;
                self.msg_log.push_back(LogEntry {
                    n: self.msg_counter,
                    device: device.clone(),
                    dir,
                    msg,
                });
                while self.msg_log.len() > MSG_LOG_CAP {
                    self.msg_log.pop_front();
                }
            }
            SessionEvent::Text { host_ts_ns, line } => {
                self.text_log.push_back(TextEntry {
                    device: device.clone(),
                    t: host_ts_ns as f64 / 1e9,
                    level: detect_level(&line),
                    line,
                });
                while self.text_log.len() > TEXT_LOG_CAP {
                    self.text_log.pop_front();
                }
            }
            SessionEvent::Raw {
                host_ts_ns,
                dir,
                bytes,
            } => {
                self.raw_log.push_back(RawEntry {
                    device: device.clone(),
                    t: host_ts_ns as f64 / 1e9,
                    dir,
                    bytes,
                });
                while self.raw_log.len() > RAW_LOG_CAP {
                    self.raw_log.pop_front();
                }
            }
            SessionEvent::Request { id, name, outcome } => {
                let key = self
                    .requests
                    .remove(&id)
                    .unwrap_or_else(|| Self::badge_key(device, &name));
                let badge = match outcome {
                    RequestOutcome::Sent => Badge::Sent,
                    RequestOutcome::Ok { latency_ns, .. } => Badge::Ok {
                        latency_ms: latency_ns as f64 / 1e6,
                    },
                    RequestOutcome::Timeout { .. } => Badge::Timeout,
                    RequestOutcome::Error(e) => Badge::Error(e),
                };
                self.badges.insert(key, badge);
            }
            SessionEvent::BadFrame(BadFrame { .. }) => {}
            SessionEvent::Marker { host_ts_ns, text } => self.markers.push(Marker {
                t: host_ts_ns as f64 / 1e9,
                text,
            }),
        }
    }

    /// Looks up a logged message by number.
    pub fn logged(&self, n: u64) -> Option<&LogEntry> {
        self.msg_log.iter().rev().find(|e| e.n == n)
    }

    // ---------- recording ----------

    /// Starts recording every running device (and devices connected
    /// later) to a new file.
    pub fn start_recording(&mut self) -> Result<PathBuf, String> {
        let p = self.project.as_ref().ok_or("no project")?;
        if !self.running() {
            return Err("connect first".into());
        }
        let dir = self
            .settings
            .recordings_dir
            .clone()
            .unwrap_or_else(|| p.recordings_dir());
        let path = recording_path(&dir, &p.config.name);
        let p = p.clone();
        self.bench
            .start_recording(&path, &p)
            .map_err(|e| e.to_string())?;
        self.recording = Some(path.clone());
        self.notify(format!("recording to {}", path.display()));
        Ok(path)
    }

    /// Stops recording.
    pub fn stop_recording(&mut self) {
        if self.bench.recording().is_none() {
            self.recording = None;
            return;
        }
        match self.bench.stop_recording() {
            Ok(()) => {
                if let Some(p) = self.recording.take() {
                    self.notify(format!("saved {}", p.display()));
                }
            }
            Err(e) => self.notify(format!("recording error: {e}")),
        }
        self.recording = None;
    }

    /// Seconds since recording started.
    pub fn recording_elapsed_s(&self) -> Option<f64> {
        self.bench
            .recording()
            .and_then(|_| self.bench.devices().first())
            .and_then(|d| d.session.recording_elapsed_s())
    }

    /// Adds a marker (also written to the recording).
    pub fn add_marker(&mut self, text: &str) {
        let text = if text.trim().is_empty() {
            format!("marker {}", self.markers.len() + 1)
        } else {
            text.trim().to_string()
        };
        if self.running() {
            self.bench.add_marker(&text);
        } else {
            self.markers.push(Marker { t: 0.0, text });
        }
    }

    /// Exports channels to CSV on a background thread.
    pub fn export_csv(
        &mut self,
        path: PathBuf,
        channels: Vec<String>,
        range: Option<(f64, f64)>,
        mode: CsvMode,
    ) {
        let Some(store) = self.store() else {
            self.notify("nothing to export: not connected");
            return;
        };
        let tx = self.jobs();
        std::thread::spawn(move || {
            let r = export_csv(&store, &channels, range, mode, &path);
            let _ = tx.send(match r {
                Ok(files) => JobResult::Info(format!(
                    "exported {}",
                    files
                        .iter()
                        .map(|f| f.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                Err(e) => JobResult::Error(e.to_string()),
            });
        });
    }

    /// Exports channels to Parquet on a background thread.
    pub fn export_parquet(&mut self, path: PathBuf) {
        let Some(store) = self.store() else {
            self.notify("nothing to export: not connected");
            return;
        };
        let tx = self.jobs();
        std::thread::spawn(move || {
            let r = pb_core::recorder::export_parquet(&store, &[], None, &path);
            let _ = tx.send(match r {
                Ok(n) => JobResult::Info(format!("exported {n} rows to {}", path.display())),
                Err(e) => JobResult::Error(e.to_string()),
            });
        });
    }

    /// Exports a recording to MCAP on a background thread.
    pub fn export_mcap(&mut self, recording: PathBuf, out: PathBuf) {
        let tx = self.jobs();
        std::thread::spawn(move || {
            let r = pb_core::recorder::OfflineSession::open(&recording)
                .and_then(|s| pb_core::recorder::mcap_export::export_mcap(&s, &out));
            let _ = tx.send(match r {
                Ok(n) => JobResult::Info(format!("exported {n} messages to {}", out.display())),
                Err(e) => JobResult::Error(e.to_string()),
            });
        });
    }

    // ---------- composer ----------

    /// Appends a `[[commands]]` entry to the project.toml of `device`
    /// (formatting preserved) and reloads the project.
    pub fn save_command_on(
        &mut self,
        device: &str,
        name: &str,
        group: &str,
        message: &str,
        payload: &Json,
        expect: Option<&str>,
    ) -> Result<(), String> {
        let p = self.project.as_ref().ok_or("no project")?;
        let d = p.device(device).ok_or("no such device")?;
        if d.project.config.command(name).is_some() {
            return Err(format!("a command named `{name}` already exists"));
        }
        let path = d.project.config_path();
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let new = append_command(&text, name, group, message, payload, expect)?;
        std::fs::write(&path, new).map_err(|e| e.to_string())?;
        self.reload_project();
        self.notify(format!("added command `{name}` to project.toml"));
        Ok(())
    }
}

/// JSON of the payload part of a top-level message: the set oneof member's
/// message, else the whole message. Proto field names are used.
pub fn inner_json(msg: &prost_reflect::DynamicMessage) -> Json {
    let opts = prost_reflect::SerializeOptions::new()
        .use_proto_field_name(true)
        .skip_default_fields(true);
    let inner = pb_core::codec::set_oneof_member(msg)
        .and_then(|m| msg.get_field_by_name(&m))
        .and_then(|v| v.as_message().cloned());
    let m = inner.as_ref().unwrap_or(msg);
    m.serialize_with_options(serde_json::value::Serializer, &opts)
        .unwrap_or(Json::Null)
}

fn json_to_toml_value(v: &Json) -> Option<toml_edit::Value> {
    Some(match v {
        Json::Null => return None,
        Json::Bool(b) => toml_edit::Value::from(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => toml_edit::Value::from(i),
            None => toml_edit::Value::from(n.as_f64().unwrap_or(0.0)),
        },
        Json::String(s) => toml_edit::Value::from(s.as_str()),
        Json::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for x in a {
                if let Some(v) = json_to_toml_value(x) {
                    arr.push(v);
                }
            }
            toml_edit::Value::Array(arr)
        }
        Json::Object(o) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, x) in o {
                if let Some(v) = json_to_toml_value(x) {
                    t.insert(k, v);
                }
            }
            toml_edit::Value::InlineTable(t)
        }
    })
}

/// Appends a command to project.toml text with `toml_edit`, keeping the
/// rest of the file (comments, formatting) unchanged.
pub fn append_command(
    text: &str,
    name: &str,
    group: &str,
    message: &str,
    payload: &Json,
    expect: Option<&str>,
) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("project.toml: {e}"))?;
    let mut t = toml_edit::Table::new();
    t.insert("name", toml_edit::value(name));
    t.insert("group", toml_edit::value(group));
    t.insert("message", toml_edit::value(message));
    let payload = json_to_toml_value(payload)
        .unwrap_or_else(|| toml_edit::Value::InlineTable(Default::default()));
    let payload = match payload {
        toml_edit::Value::InlineTable(t) => toml_edit::Value::InlineTable(t),
        _ => toml_edit::Value::InlineTable(Default::default()),
    };
    t.insert("payload", toml_edit::Item::Value(payload));
    if let Some(e) = expect {
        t.insert("expect", toml_edit::value(e));
    }
    match doc.get_mut("commands") {
        Some(toml_edit::Item::ArrayOfTables(a)) => a.push(t),
        Some(_) => return Err("`commands` in project.toml is not an array of tables".into()),
        None => {
            let mut a = toml_edit::ArrayOfTables::new();
            a.push(t);
            doc.insert("commands", toml_edit::Item::ArrayOfTables(a));
        }
    }
    Ok(doc.to_string())
}

/// Lists fields whose number changed or that disappeared between schemas.
pub fn schema_diff(old: &pb_core::schema::Schema, new: &pb_core::schema::Schema) -> Vec<String> {
    let mut out = Vec::new();
    for m in old.user_messages() {
        let Some(nm) = new.message(m.full_name()) else {
            out.push(format!("message {} was removed", m.full_name()));
            continue;
        };
        for f in m.fields() {
            match nm.get_field_by_name(f.name()) {
                None => out.push(format!("{}.{} was removed", m.full_name(), f.name())),
                Some(nf) if nf.number() != f.number() => out.push(format!(
                    "{}.{}: field number changed {} → {} (old recordings and firmware will not match)",
                    m.full_name(),
                    f.name(),
                    f.number(),
                    nf.number()
                )),
                Some(_) => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
