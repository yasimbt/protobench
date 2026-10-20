//! The connection runtime.
//!
//! A [`Session`] owns one I/O thread per connection. The thread reads bytes,
//! runs them through the framer, CRC check and codec, extracts telemetry
//! straight into the shared [`TelemetryStore`], resolves outstanding
//! requests, and fans out [`SessionEvent`]s to subscribers. Sends go through
//! a command queue to the same thread, so there is never blocking I/O on a
//! caller's thread.

mod correlation;
mod stats;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use parking_lot::Mutex;
use prost_reflect::ReflectMessage;

pub use correlation::{Correlator, Pending, RequestOutcome, Resolved, TickAction, MAX_SEQ};
pub use stats::{
    BadFrame, BadFrames, Latency, LatencyStats, LinkCounters, LinkSnapshot, RateMeter,
};

use crate::clock::SharedClock;
use crate::codec::{get_int_at_path, set_int_at_path, Codec, DecodeError, DecodedMessage};
use crate::commands::{build_inline, table_to_json, SendRequest};
use crate::config::{ConnectionProfile, ProjectConfig, ResetLine, TelemetryMode, TimestampSource};
use crate::error::CoreError;
use crate::framing::{build_framer, FrameErrorKind, FrameEvent, Framer};
use crate::project::Project;
use crate::schema::FieldPath;
use crate::telemetry::{DeviceTimeBase, Extractor, TelemetryStore, TriggerState};
use crate::transport::Connector;

/// Direction of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Device → host.
    Rx,
    /// Host → device.
    Tx,
}

/// Connection state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    /// Not started or stopped.
    Disconnected,
    /// First connection attempt in progress.
    Connecting,
    /// Link is up.
    Connected,
    /// Waiting to retry after an error.
    Reconnecting {
        /// Attempts so far.
        attempt: u32,
        /// Last error.
        error: String,
    },
    /// Connecting failed and auto-reconnect is off.
    Failed(String),
    /// A replay reached its end.
    Finished,
}

impl ConnState {
    /// Short label for status bars.
    pub fn label(&self) -> String {
        match self {
            ConnState::Disconnected => "Disconnected".into(),
            ConnState::Connecting => "Connecting…".into(),
            ConnState::Connected => "Connected".into(),
            ConnState::Reconnecting { attempt, error } => {
                format!("Reconnecting (attempt {attempt}): {error}")
            }
            ConnState::Failed(e) => format!("Failed: {e}"),
            ConnState::Finished => "Replay finished".into(),
        }
    }

    /// True when connected.
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnState::Connected)
    }
}

/// Something that happened on the link.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Connection state changed.
    State(ConnState),
    /// A message was decoded (rx) or sent (tx).
    Message {
        /// Direction.
        dir: Direction,
        /// The message.
        msg: Arc<DecodedMessage>,
    },
    /// A text line from text passthrough.
    Text {
        /// Host time in ns.
        host_ts_ns: u64,
        /// The line.
        line: String,
    },
    /// Raw bytes read or written.
    Raw {
        /// Host time in ns.
        host_ts_ns: u64,
        /// Direction.
        dir: Direction,
        /// Bytes.
        bytes: Arc<[u8]>,
    },
    /// A request finished.
    Request {
        /// Request id.
        id: u64,
        /// Command name.
        name: String,
        /// Outcome.
        outcome: RequestOutcome,
    },
    /// A bad frame.
    BadFrame(BadFrame),
    /// A marker was added.
    Marker {
        /// Host time in ns.
        host_ts_ns: u64,
        /// Label.
        text: String,
    },
}

/// Record kinds written to a session file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    /// Received frame (codec bytes, CRC and framing removed).
    RxFrame = 1,
    /// Sent frame (codec bytes).
    TxFrame = 2,
    /// Text line.
    TextLine = 3,
    /// User marker.
    Marker = 4,
}

impl RecordKind {
    /// Parses the on-disk code.
    pub fn from_u8(b: u8) -> Option<RecordKind> {
        Some(match b {
            1 => RecordKind::RxFrame,
            2 => RecordKind::TxFrame,
            3 => RecordKind::TextLine,
            4 => RecordKind::Marker,
            _ => return None,
        })
    }

    /// Name used in exports.
    pub fn as_str(self) -> &'static str {
        match self {
            RecordKind::RxFrame => "rx_frame",
            RecordKind::TxFrame => "tx_frame",
            RecordKind::TextLine => "text_line",
            RecordKind::Marker => "marker",
        }
    }
}

/// Receives every frame, text line and marker while recording.
pub trait RecordSink: Send {
    /// Writes one record.
    fn record(&mut self, kind: RecordKind, host_ts_ns: u64, bytes: &[u8]);
    /// Flushes buffered data. Errors are reported here.
    fn finish(&mut self) -> Result<(), CoreError>;
    /// Last write error, if any.
    fn error(&self) -> Option<String> {
        None
    }
}

/// Everything a session needs besides the transport.
#[derive(Clone)]
pub struct SessionParams {
    /// Project config.
    pub config: ProjectConfig,
    /// Host codec.
    pub codec: Arc<Codec>,
    /// Ring-buffer capacity per channel.
    pub capacity: usize,
    /// Reconnect after errors.
    pub auto_reconnect: bool,
    /// Pulse-reset duration.
    pub reset_pulse_ms: u64,
    /// Line used by pulse reset.
    pub reset_line: ResetLine,
    /// Idle DTR level.
    pub dtr_idle: bool,
    /// Idle RTS level.
    pub rts_idle: bool,
    /// Endpoint description shown until connected.
    pub endpoint: String,
}

impl SessionParams {
    /// Builds parameters for a project and profile.
    pub fn new(project: &Project, profile: &ConnectionProfile) -> Result<SessionParams, CoreError> {
        let schema = project.require_schema()?;
        let codec = Arc::new(Codec::new(
            &project.config,
            &schema,
            crate::codec::Role::Host,
        )?);
        Ok(SessionParams {
            config: project.config.clone(),
            codec,
            capacity: crate::telemetry::DEFAULT_CAPACITY,
            auto_reconnect: profile.auto_reconnect,
            reset_pulse_ms: profile.reset_pulse_ms,
            reset_line: profile.reset_line,
            dtr_idle: profile.dtr_on_open,
            rts_idle: profile.rts_on_open,
            endpoint: profile.endpoint(),
        })
    }
}

/// A handle to wait for one request.
#[derive(Debug)]
pub struct RequestHandle {
    /// Request id.
    pub id: u64,
    rx: Receiver<RequestOutcome>,
}

impl RequestHandle {
    /// Waits for the outcome. Returns `Error("no result")` if the session
    /// stopped or `timeout` passed.
    pub fn wait(&self, timeout: Duration) -> RequestOutcome {
        self.rx.recv_timeout(timeout).unwrap_or_else(|_| {
            RequestOutcome::Error("no result (session stopped or wait timed out)".into())
        })
    }

    /// Non-blocking check.
    pub fn try_get(&self) -> Option<RequestOutcome> {
        self.rx.try_recv().ok()
    }
}

enum Cmd {
    Send {
        id: u64,
        req: SendRequest,
        reply: Option<Sender<RequestOutcome>>,
    },
    SendRaw(Vec<u8>),
    SetDtr(bool),
    SetRts(bool),
    PulseReset,
    Periodic {
        key: String,
        spec: Option<(SendRequest, u64)>,
    },
    Arm(String),
    Stop,
}

/// State shared between the I/O thread and handles.
pub struct Shared {
    /// Counters.
    pub counters: LinkCounters,
    /// Last bad frames.
    pub bad_frames: BadFrames,
    /// Per-command latency.
    pub latency: Latency,
    /// Telemetry channels.
    pub telemetry: Arc<TelemetryStore>,
    state: Mutex<ConnState>,
    rates: Mutex<LinkSnapshot>,
    subscribers: Mutex<Vec<Sender<SessionEvent>>>,
    recorder: Mutex<Option<Box<dyn RecordSink>>>,
    clock: SharedClock,
    endpoint: Mutex<String>,
    triggers: Mutex<Vec<TriggerState>>,
    periodic: Mutex<Vec<String>>,
    next_id: AtomicU64,
    record_start_ns: AtomicU64,
}

impl Shared {
    fn emit(&self, ev: SessionEvent) {
        let mut subs = self.subscribers.lock();
        if subs.is_empty() {
            return;
        }
        subs.retain(|s| match s.try_send(ev.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                LinkCounters::add(&self.counters.dropped_events, 1);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    fn has_subscribers(&self) -> bool {
        !self.subscribers.lock().is_empty()
    }

    fn set_state(&self, s: ConnState) {
        let changed = {
            let mut g = self.state.lock();
            let changed = *g != s;
            *g = s.clone();
            changed
        };
        if changed {
            self.emit(SessionEvent::State(s));
        }
    }

    fn record(&self, kind: RecordKind, ts: u64, bytes: &[u8]) {
        if let Some(r) = self.recorder.lock().as_mut() {
            r.record(kind, ts, bytes);
        }
    }
}

/// Capacity of each subscriber queue.
pub const SUBSCRIBER_QUEUE: usize = 200_000;

/// A running connection.
pub struct Session {
    shared: Arc<Shared>,
    cmd_tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
    codec: Arc<Codec>,
}

impl Session {
    /// Starts the I/O thread. It connects (and reconnects) using `connector`.
    pub fn start(
        params: SessionParams,
        connector: Connector,
        clock: SharedClock,
    ) -> Result<Session, CoreError> {
        let telemetry = Arc::new(TelemetryStore::new(params.capacity));
        let extractor = Extractor::new(&params.config, &params.codec)?;
        let shared = Arc::new(Shared {
            counters: LinkCounters::default(),
            bad_frames: BadFrames::new(20),
            latency: Latency::default(),
            telemetry,
            state: Mutex::new(ConnState::Connecting),
            rates: Mutex::new(LinkSnapshot::default()),
            subscribers: Mutex::new(Vec::new()),
            recorder: Mutex::new(None),
            clock: clock.clone(),
            endpoint: Mutex::new(params.endpoint.clone()),
            triggers: Mutex::new(extractor.triggers()),
            periodic: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            record_start_ns: AtomicU64::new(0),
        });
        let mut polls = Vec::new();
        for t in &params.config.telemetry {
            if t.mode != TelemetryMode::Poll {
                continue;
            }
            let (Some(req), Some(resp)) = (&t.request, &t.response) else {
                continue;
            };
            let r = build_inline(
                &params.config,
                &params.codec,
                req,
                table_to_json(&t.request_payload),
                Some(resp),
                None,
                Some(0),
            )?;
            let mut r = r;
            r.name = format!("poll: {}", t.name);
            polls.push(Periodic {
                key: format!("poll:{}", t.name),
                req: r,
                period_ns: t.interval_ms.max(1) * 1_000_000,
                next_ns: 0,
            });
        }
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let codec = params.codec.clone();
        let worker = Worker::new(params, extractor, shared.clone(), polls);
        let thread = std::thread::Builder::new()
            .name("protobench-io".into())
            .spawn(move || worker.run(connector, cmd_rx))
            .map_err(CoreError::IoBare)?;
        Ok(Session {
            shared,
            cmd_tx,
            thread: Some(thread),
            codec,
        })
    }

    /// The codec used by this session.
    pub fn codec(&self) -> &Arc<Codec> {
        &self.codec
    }

    /// Shared state (counters, telemetry, latency).
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Telemetry store.
    pub fn telemetry(&self) -> &Arc<TelemetryStore> {
        &self.shared.telemetry
    }

    /// Current connection state.
    pub fn state(&self) -> ConnState {
        self.shared.state.lock().clone()
    }

    /// Current endpoint description.
    pub fn endpoint(&self) -> String {
        self.shared.endpoint.lock().clone()
    }

    /// Counters with rates.
    pub fn stats(&self) -> LinkSnapshot {
        *self.shared.rates.lock()
    }

    /// Subscribes to events. Each subscriber has its own bounded queue;
    /// events are dropped (and counted) if it falls behind.
    pub fn subscribe(&self) -> Receiver<SessionEvent> {
        let (tx, rx) = crossbeam_channel::bounded(SUBSCRIBER_QUEUE);
        self.shared.subscribers.lock().push(tx);
        rx
    }

    /// Queues a request. The handle yields the outcome.
    pub fn send(&self, req: SendRequest) -> RequestHandle {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = crossbeam_channel::bounded(1);
        if self
            .cmd_tx
            .send(Cmd::Send {
                id,
                req,
                reply: Some(tx.clone()),
            })
            .is_err()
        {
            let _ = tx.send(RequestOutcome::Error("session stopped".into()));
        }
        RequestHandle { id, rx }
    }

    /// Sends and waits for the outcome.
    pub fn send_and_wait(&self, req: SendRequest) -> RequestOutcome {
        let total = req.timeout_ms.saturating_mul(u64::from(req.retries) + 1) + 2000;
        self.send(req).wait(Duration::from_millis(total))
    }

    /// Writes raw bytes (no framing).
    pub fn send_raw(&self, bytes: Vec<u8>) {
        let _ = self.cmd_tx.send(Cmd::SendRaw(bytes));
    }

    /// Sets DTR.
    pub fn set_dtr(&self, on: bool) {
        let _ = self.cmd_tx.send(Cmd::SetDtr(on));
    }

    /// Sets RTS.
    pub fn set_rts(&self, on: bool) {
        let _ = self.cmd_tx.send(Cmd::SetRts(on));
    }

    /// Pulses the reset line.
    pub fn pulse_reset(&self) {
        let _ = self.cmd_tx.send(Cmd::PulseReset);
    }

    /// Starts (`Some`) or stops (`None`) sending a request every `period_ms`.
    pub fn set_periodic(&self, key: &str, spec: Option<(SendRequest, u64)>) {
        {
            let mut p = self.shared.periodic.lock();
            p.retain(|k| k != key);
            if spec.is_some() {
                p.push(key.to_string());
            }
        }
        let _ = self.cmd_tx.send(Cmd::Periodic {
            key: key.to_string(),
            spec,
        });
    }

    /// Keys of the active periodic sends.
    pub fn periodic_keys(&self) -> Vec<String> {
        self.shared.periodic.lock().clone()
    }

    /// Arms a triggered capture.
    pub fn arm_trigger(&self, name: &str) {
        let _ = self.cmd_tx.send(Cmd::Arm(name.to_string()));
    }

    /// States of triggered captures.
    pub fn triggers(&self) -> Vec<TriggerState> {
        self.shared.triggers.lock().clone()
    }

    /// Starts recording into `sink` (replacing any current recording).
    pub fn start_recording(&self, sink: Box<dyn RecordSink>) -> Option<Box<dyn RecordSink>> {
        self.shared
            .record_start_ns
            .store(self.shared.clock.now_ns(), Ordering::Relaxed);
        self.shared.recorder.lock().replace(sink)
    }

    /// Stops recording and returns the sink (call `finish` on it).
    pub fn stop_recording(&self) -> Option<Box<dyn RecordSink>> {
        self.shared.recorder.lock().take()
    }

    /// True while recording.
    pub fn is_recording(&self) -> bool {
        self.shared.recorder.lock().is_some()
    }

    /// Seconds since recording started.
    pub fn recording_elapsed_s(&self) -> Option<f64> {
        if !self.is_recording() {
            return None;
        }
        let start = self.shared.record_start_ns.load(Ordering::Relaxed);
        Some(self.shared.clock.now_ns().saturating_sub(start) as f64 / 1e9)
    }

    /// Adds a labelled marker to the recording and the event stream.
    pub fn add_marker(&self, text: &str) {
        let ts = self.shared.clock.now_ns();
        self.shared.record(RecordKind::Marker, ts, text.as_bytes());
        self.shared.emit(SessionEvent::Marker {
            host_ts_ns: ts,
            text: text.to_string(),
        });
    }

    /// Host time now, in ns on the session clock.
    pub fn now_ns(&self) -> u64 {
        self.shared.clock.now_ns()
    }

    /// Waits until connected or `timeout`. Returns the final state.
    pub fn wait_connected(&self, timeout: Duration) -> ConnState {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let s = self.state();
            if matches!(
                s,
                ConnState::Connected | ConnState::Failed(_) | ConnState::Finished
            ) || std::time::Instant::now() >= deadline
            {
                return s;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Stops the I/O thread and waits for it.
    pub fn stop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(mut r) = self.shared.recorder.lock().take() {
            let _ = r.finish();
        }
        self.shared.set_state(ConnState::Disconnected);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Periodic {
    key: String,
    req: SendRequest,
    period_ns: u64,
    next_ns: u64,
}

type Reply = Option<Sender<RequestOutcome>>;

struct Worker {
    params: SessionParams,
    extractor: Extractor,
    shared: Arc<Shared>,
    framer: Box<dyn Framer>,
    correlator: Correlator<Reply>,
    request_field: Option<FieldPath>,
    response_field: Option<FieldPath>,
    ts_field: Option<FieldPath>,
    timebase: DeviceTimeBase,
    periodic: Vec<Periodic>,
    rate: RateMeter,
    events: Vec<FrameEvent>,
}

impl Worker {
    fn new(
        params: SessionParams,
        extractor: Extractor,
        shared: Arc<Shared>,
        polls: Vec<Periodic>,
    ) -> Self {
        let cfg = &params.config;
        let request_field = cfg
            .correlation
            .request_field
            .as_deref()
            .and_then(|p| FieldPath::parse(p).ok());
        let response_field = cfg
            .correlation
            .response_field
            .as_deref()
            .and_then(|p| FieldPath::parse(p).ok());
        let ts_field = match cfg.timestamp.source {
            TimestampSource::Device => cfg
                .timestamp
                .field
                .as_deref()
                .and_then(|p| FieldPath::parse(p).ok()),
            TimestampSource::Host => None,
        };
        Worker {
            framer: build_framer(&cfg.framing),
            correlator: Correlator::new(response_field.is_some()),
            timebase: DeviceTimeBase::new(cfg.timestamp.unit, cfg.timestamp.wrap_bits),
            request_field,
            response_field,
            ts_field,
            extractor,
            shared,
            periodic: polls,
            rate: RateMeter::default(),
            events: Vec::new(),
            params,
        }
    }

    fn now(&self) -> u64 {
        self.shared.clock.now_ns()
    }

    fn run(mut self, mut connector: Connector, cmd_rx: Receiver<Cmd>) {
        let mut transport: Option<Box<dyn crate::transport::Transport>> = None;
        let mut backoff_ms: u64 = 100;
        let mut next_attempt_ns: u64 = 0;
        let mut attempts: u32 = 0;
        let mut gave_up = false;
        let mut finished = false;
        let mut buf = vec![0u8; 64 * 1024];
        let mut last_tick_ns = 0u64;
        loop {
            // Commands.
            loop {
                let cmd = if transport.is_none() {
                    match cmd_rx.recv_timeout(Duration::from_millis(5)) {
                        Ok(c) => c,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                    }
                } else {
                    match cmd_rx.try_recv() {
                        Ok(c) => c,
                        Err(crossbeam_channel::TryRecvError::Empty) => break,
                        Err(crossbeam_channel::TryRecvError::Disconnected) => return,
                    }
                };
                if matches!(cmd, Cmd::Stop) {
                    self.fail_pending("session stopped");
                    return;
                }
                self.handle_cmd(cmd, &mut transport);
            }

            // Connection management.
            if transport.is_none() {
                if gave_up || finished {
                    continue;
                }
                let now = self.now();
                if now < next_attempt_ns {
                    continue;
                }
                match connector() {
                    Ok(t) => {
                        *self.shared.endpoint.lock() = t.describe();
                        self.framer.reset();
                        transport = Some(t);
                        attempts = 0;
                        backoff_ms = 100;
                        self.shared.set_state(ConnState::Connected);
                    }
                    Err(e) => {
                        attempts += 1;
                        if !self.params.auto_reconnect {
                            gave_up = true;
                            self.shared.set_state(ConnState::Failed(e.to_string()));
                        } else {
                            LinkCounters::add(&self.shared.counters.reconnects, 1);
                            self.shared.set_state(ConnState::Reconnecting {
                                attempt: attempts,
                                error: e.to_string(),
                            });
                            next_attempt_ns = now + backoff_ms * 1_000_000;
                            backoff_ms = (backoff_ms * 2).min(2000);
                        }
                    }
                }
                continue;
            }

            // Read.
            let mut lost: Option<String> = None;
            if let Some(t) = transport.as_mut() {
                match t.read(&mut buf) {
                    Ok(0) => {}
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        self.handle_rx(&data, t.as_mut());
                    }
                    Err(e) => lost = Some(e.to_string()),
                }
                if t.finished() {
                    finished = true;
                }
            }
            if finished {
                transport = None;
                self.fail_pending("replay finished");
                self.shared.set_state(ConnState::Finished);
                continue;
            }
            if let Some(err) = lost {
                transport = None;
                self.fail_pending("disconnected");
                if self.params.auto_reconnect {
                    self.shared.set_state(ConnState::Reconnecting {
                        attempt: 0,
                        error: err,
                    });
                    next_attempt_ns = self.now() + backoff_ms * 1_000_000;
                } else {
                    gave_up = true;
                    self.shared.set_state(ConnState::Failed(err));
                }
                continue;
            }

            // Timers (at most every millisecond).
            let now = self.now();
            if now.saturating_sub(last_tick_ns) >= 1_000_000 {
                last_tick_ns = now;
                if let Some(t) = transport.as_mut() {
                    self.tick(now, t.as_mut());
                }
            }
        }
    }

    fn fail_pending(&mut self, reason: &str) {
        for r in self.correlator.fail_all(reason) {
            self.deliver(r);
        }
    }

    fn deliver(&self, r: Resolved<Reply>) {
        match &r.outcome {
            RequestOutcome::Ok { latency_ns, .. } => {
                self.shared.latency.record(&r.name, *latency_ns)
            }
            RequestOutcome::Timeout { .. } => {
                LinkCounters::add(&self.shared.counters.timeouts, 1);
                self.shared.latency.record_timeout(&r.name);
            }
            _ => {}
        }
        if let Some(tx) = &r.token {
            let _ = tx.try_send(r.outcome.clone());
        }
        self.shared.emit(SessionEvent::Request {
            id: r.id,
            name: r.name,
            outcome: r.outcome,
        });
    }

    fn handle_cmd(
        &mut self,
        cmd: Cmd,
        transport: &mut Option<Box<dyn crate::transport::Transport>>,
    ) {
        match cmd {
            Cmd::Send { id, req, reply } => match transport.as_mut() {
                Some(t) => self.send(id, req, reply, t.as_mut()),
                None => self.deliver(Resolved {
                    id,
                    name: req.name,
                    outcome: RequestOutcome::Error("not connected".into()),
                    token: reply,
                }),
            },
            Cmd::SendRaw(bytes) => {
                if let Some(t) = transport.as_mut() {
                    self.write(t.as_mut(), &bytes);
                }
            }
            Cmd::SetDtr(on) => {
                if let Some(t) = transport.as_mut() {
                    let _ = t.set_dtr(on);
                }
            }
            Cmd::SetRts(on) => {
                if let Some(t) = transport.as_mut() {
                    let _ = t.set_rts(on);
                }
            }
            Cmd::PulseReset => {
                if let Some(t) = transport.as_mut() {
                    let d = crate::transport::pulse(self.params.reset_pulse_ms);
                    let _ = match self.params.reset_line {
                        ResetLine::Dtr => t.set_dtr(!self.params.dtr_idle),
                        ResetLine::Rts => t.set_rts(!self.params.rts_idle),
                    };
                    std::thread::sleep(d);
                    let _ = match self.params.reset_line {
                        ResetLine::Dtr => t.set_dtr(self.params.dtr_idle),
                        ResetLine::Rts => t.set_rts(self.params.rts_idle),
                    };
                }
            }
            Cmd::Periodic { key, spec } => {
                self.periodic.retain(|p| p.key != key);
                if let Some((req, ms)) = spec {
                    self.periodic.push(Periodic {
                        key,
                        req,
                        period_ns: ms.max(1) * 1_000_000,
                        next_ns: 0,
                    });
                }
            }
            Cmd::Arm(name) => {
                self.extractor.arm(&name);
                *self.shared.triggers.lock() = self.extractor.triggers();
            }
            Cmd::Stop => {}
        }
    }

    fn write(&self, t: &mut dyn crate::transport::Transport, bytes: &[u8]) -> bool {
        let ts = self.now();
        match t.write_all(bytes) {
            Ok(()) => {
                LinkCounters::add(&self.shared.counters.tx_bytes, bytes.len() as u64);
                if self.shared.has_subscribers() {
                    self.shared.emit(SessionEvent::Raw {
                        host_ts_ns: ts,
                        dir: Direction::Tx,
                        bytes: Arc::from(bytes),
                    });
                }
                true
            }
            Err(e) => {
                tracing::warn!("write failed: {e}");
                false
            }
        }
    }

    fn send(
        &mut self,
        id: u64,
        mut req: SendRequest,
        reply: Reply,
        t: &mut dyn crate::transport::Transport,
    ) {
        let mut seq = 0;
        if let Some(rf) = &self.request_field {
            let s = self.correlator.next_seq();
            if set_int_at_path(&mut req.msg, rf, u64::from(s)) {
                seq = s;
            }
        }
        let codec_bytes = match self.params.codec.encode(&req.msg) {
            Ok(b) => b,
            Err(e) => {
                self.deliver(Resolved {
                    id,
                    name: req.name,
                    outcome: RequestOutcome::Error(e.to_string()),
                    token: reply,
                });
                return;
            }
        };
        let wire = self.framer.encode(&codec_bytes);
        let ts = self.now();
        if !self.write(t, &wire) {
            self.deliver(Resolved {
                id,
                name: req.name,
                outcome: RequestOutcome::Error("write failed".into()),
                token: reply,
            });
            return;
        }
        LinkCounters::add(&self.shared.counters.tx_frames, 1);
        self.shared.record(RecordKind::TxFrame, ts, &codec_bytes);
        if self.shared.has_subscribers() {
            let desc = req.msg.descriptor();
            let member = crate::codec::set_oneof_member(&req.msg)
                .filter(|_| self.params.codec.mode() == crate::config::CodecMode::Envelope);
            let offset = codec_bytes
                .len()
                .saturating_sub(prost::Message::encoded_len(&req.msg));
            self.shared.emit(SessionEvent::Message {
                dir: Direction::Tx,
                msg: Arc::new(DecodedMessage {
                    host_ts_ns: ts,
                    device_ts: None,
                    type_name: desc.full_name().to_string(),
                    member,
                    msg: req.msg.clone(),
                    raw: codec_bytes.clone(),
                    payload_offset: offset,
                }),
            });
        }
        match req.expect {
            Some(expect) => self.correlator.register(Pending {
                id,
                seq,
                name: req.name,
                expect,
                wire,
                timeout_ns: req.timeout_ms.max(1) * 1_000_000,
                retries_left: req.retries,
                attempts: 1,
                sent_ns: ts,
                token: reply,
            }),
            None => self.deliver(Resolved {
                id,
                name: req.name,
                outcome: RequestOutcome::Sent,
                token: reply,
            }),
        }
    }

    fn handle_rx(&mut self, data: &[u8], _t: &mut dyn crate::transport::Transport) {
        let ts = self.now();
        let c = &self.shared.counters;
        LinkCounters::add(&c.rx_bytes, data.len() as u64);
        if self.shared.has_subscribers() {
            self.shared.emit(SessionEvent::Raw {
                host_ts_ns: ts,
                dir: Direction::Rx,
                bytes: Arc::from(data),
            });
        }
        let mut events = std::mem::take(&mut self.events);
        events.clear();
        self.framer.push(data, &mut events);
        for ev in events.drain(..) {
            match ev {
                FrameEvent::Frame(payload) => self.handle_frame(payload, ts),
                FrameEvent::Text(line) => {
                    LinkCounters::add(&self.shared.counters.text_lines, 1);
                    self.shared
                        .record(RecordKind::TextLine, ts, line.as_bytes());
                    self.shared.emit(SessionEvent::Text {
                        host_ts_ns: ts,
                        line,
                    });
                }
                FrameEvent::Error {
                    kind,
                    reason,
                    bytes,
                } => {
                    let c = &self.shared.counters;
                    let k = match kind {
                        FrameErrorKind::Framing => {
                            LinkCounters::add(&c.framing_errors, 1);
                            "framing"
                        }
                        FrameErrorKind::Crc => {
                            LinkCounters::add(&c.crc_errors, 1);
                            "crc"
                        }
                    };
                    self.bad_frame(k, reason, bytes, ts);
                }
            }
        }
        self.events = events;
    }

    fn bad_frame(&self, kind: &'static str, reason: String, bytes: Vec<u8>, ts: u64) {
        let f = BadFrame {
            host_ts_ns: ts,
            kind,
            reason,
            bytes,
        };
        self.shared.bad_frames.push(f.clone());
        self.shared.emit(SessionEvent::BadFrame(f));
    }

    fn handle_frame(&mut self, payload: Vec<u8>, ts: u64) {
        let c = &self.shared.counters;
        LinkCounters::add(&c.rx_frames, 1);
        self.shared.record(RecordKind::RxFrame, ts, &payload);
        let mut msg = match self.params.codec.decode(&payload, ts) {
            Ok(m) => m,
            Err(e) => {
                let kind = match e {
                    DecodeError::UnknownId(_) => {
                        LinkCounters::add(&c.unknown_ids, 1);
                        "unknown_id"
                    }
                    _ => {
                        LinkCounters::add(&c.decode_errors, 1);
                        "decode"
                    }
                };
                let keep = payload.len().min(crate::framing::BAD_FRAME_KEEP);
                self.bad_frame(kind, e.to_string(), payload[..keep].to_vec(), ts);
                return;
            }
        };
        LinkCounters::add(&c.rx_msgs, 1);
        let host_s = ts as f64 / 1e9;
        if let Some(p) = &self.ts_field {
            if let Some(raw) = get_int_at_path(&msg.msg, p) {
                msg.device_ts = Some(self.timebase.map(raw, host_s));
            }
        }
        let msg = Arc::new(msg);
        let t = msg.device_ts.unwrap_or(host_s);
        let n = self
            .extractor
            .process(&self.params.codec, &msg, t, &self.shared.telemetry);
        if n > 0 {
            LinkCounters::add(&c.samples, n as u64);
        }
        let seq = self
            .response_field
            .as_ref()
            .and_then(|p| get_int_at_path(&msg.msg, p));
        if let Some(r) = self
            .correlator
            .on_message(&self.params.codec, &msg, seq, ts)
        {
            self.deliver(r);
        }
        if self.shared.has_subscribers() {
            self.shared.emit(SessionEvent::Message {
                dir: Direction::Rx,
                msg,
            });
        }
        *self.shared.triggers.lock() = self.extractor.triggers();
    }

    fn tick(&mut self, now: u64, t: &mut dyn crate::transport::Transport) {
        let (resend, done) = self.correlator.tick(now);
        for a in resend {
            let TickAction::Resend { wire, .. } = a;
            LinkCounters::add(&self.shared.counters.retries, 1);
            if self.write(t, &wire) {
                LinkCounters::add(&self.shared.counters.tx_frames, 1);
            }
        }
        for r in done {
            self.deliver(r);
        }
        let mut due = Vec::new();
        for p in &mut self.periodic {
            if now >= p.next_ns {
                p.next_ns = if p.next_ns == 0 { now } else { p.next_ns } + p.period_ns;
                if p.next_ns < now {
                    p.next_ns = now + p.period_ns;
                }
                due.push(p.req.clone());
            }
        }
        for req in due {
            let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
            self.send(id, req, None, t);
        }
        let snap = self.rate.update(now, self.shared.counters.snapshot());
        *self.shared.rates.lock() = snap;
    }
}

#[cfg(test)]
mod tests;
