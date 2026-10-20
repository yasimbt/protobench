//! ProtoBench device simulator.
//!
//! Implements the device side of a project's framing, codec and correlation
//! using the project's own schema: telemetry streams built from field
//! generators, configured command responses that echo the sequence id,
//! stream actions, and fault injection (latency, drops, corruption, garbage).
//! It runs over the in-process loopback ([`sim_factory`]) or as a TCP server
//! ([`run_tcp_server`]).

pub mod generators;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use prost_reflect::{DynamicMessage, Kind, MessageDescriptor, ReflectMessage};
use serde_json::Value as Json;

use pb_core::clock::{SharedClock, SystemClock};
use pb_core::codec::{get_int_at_path, set_int_at_path, Codec, MessageTarget, Role};
use pb_core::config::{parse_sim_action, ProjectConfig, SimAction, SimulatorConfig};
use pb_core::error::CoreError;
use pb_core::framing::{build_framer, FrameEvent, Framer};
use pb_core::project::Project;
use pb_core::schema::{FieldPath, Schema};
use pb_core::transport::{loopback_pair, SimFactory, TcpServer, Transport};
use pb_core::util::Rng;

pub use generators::{Gen, GenValue};

/// How a field gets its value.
#[derive(Debug, Clone)]
enum FieldSource {
    Gen(Gen),
    Const(Json),
}

#[derive(Debug, Clone)]
struct FieldSpec {
    name: String,
    source: FieldSource,
    kind: Kind,
    is_list: bool,
}

fn compile_fields(
    desc: &MessageDescriptor,
    table: &toml::Table,
    ctx: &str,
) -> Result<Vec<FieldSpec>, CoreError> {
    let mut out = Vec::new();
    for (k, v) in table {
        let field = desc
            .get_field_by_name(k)
            .or_else(|| desc.get_field_by_json_name(k))
            .ok_or_else(|| {
                CoreError::Config(format!("{ctx}: {} has no field `{k}`", desc.full_name()))
            })?;
        let source = match v {
            toml::Value::String(s)
                if matches!(field.kind(), Kind::String) && Gen::parse(s).is_err() =>
            {
                FieldSource::Const(Json::String(s.clone()))
            }
            toml::Value::String(s) => match Gen::parse(s) {
                Ok(g) => FieldSource::Gen(g),
                // Enum names and other literal strings.
                Err(e) => {
                    if matches!(field.kind(), Kind::Enum(_) | Kind::Bytes) {
                        FieldSource::Const(Json::String(s.clone()))
                    } else {
                        return Err(CoreError::Config(format!("{ctx}: field `{k}`: {e}")));
                    }
                }
            },
            other => FieldSource::Const(pb_core::commands::toml_to_json(other)),
        };
        out.push(FieldSpec {
            name: field.name().to_string(),
            source,
            kind: field.kind(),
            is_list: field.is_list(),
        });
    }
    Ok(out)
}

fn num_to_json(kind: &Kind, v: f64, enum_cycle: bool) -> Json {
    match kind {
        Kind::Bool => Json::Bool(v != 0.0),
        Kind::Float | Kind::Double => serde_json::Number::from_f64(v)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Kind::Enum(e) => {
            let values: Vec<i32> = e.values().map(|x| x.number()).collect();
            let n = if enum_cycle && !values.is_empty() {
                values[(v.max(0.0) as usize) % values.len()]
            } else {
                v.round() as i32
            };
            Json::from(n)
        }
        Kind::String => Json::String(format!("{v}")),
        Kind::Uint32 | Kind::Fixed32 => Json::from(v.round().clamp(0.0, u32::MAX as f64) as u32),
        Kind::Uint64 | Kind::Fixed64 => Json::from(v.round().max(0.0) as u64),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => {
            Json::from(v.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32)
        }
        _ => Json::from(v.round() as i64),
    }
}

fn eval_fields(fields: &mut [FieldSpec], t: f64, rng: &mut Rng) -> Json {
    let mut obj = serde_json::Map::new();
    for f in fields.iter_mut() {
        let v = match &mut f.source {
            FieldSource::Const(j) => j.clone(),
            FieldSource::Gen(g) => {
                let cyc = g.is_enum_cycle();
                match g.eval(t, rng) {
                    GenValue::Num(v) if f.is_list => {
                        Json::Array(vec![num_to_json(&f.kind, v, cyc)])
                    }
                    GenValue::Num(v) => num_to_json(&f.kind, v, cyc),
                    GenValue::Array(a) if f.is_list => {
                        Json::Array(a.iter().map(|v| num_to_json(&f.kind, *v, cyc)).collect())
                    }
                    GenValue::Array(a) => {
                        num_to_json(&f.kind, a.first().copied().unwrap_or(0.0), cyc)
                    }
                }
            }
        };
        obj.insert(f.name.clone(), v);
    }
    Json::Object(obj)
}

struct Stream {
    name: String,
    target: MessageTarget,
    fields: Vec<FieldSpec>,
    interval_ns: u64,
    enabled: bool,
    next_ns: u64,
}

struct Response {
    on: MessageTarget,
    reply: MessageTarget,
    fields: Vec<FieldSpec>,
    action: Option<SimAction>,
}

/// Counters the simulator keeps about itself.
#[derive(Debug, Default)]
pub struct SimStats {
    /// Telemetry messages generated (including dropped ones).
    pub stream_msgs: AtomicU64,
    /// Responses sent.
    pub responses: AtomicU64,
    /// Requests received.
    pub requests: AtomicU64,
    /// Frames dropped on purpose.
    pub dropped: AtomicU64,
    /// Frames corrupted on purpose.
    pub corrupted: AtomicU64,
    /// Garbage bursts inserted.
    pub garbage: AtomicU64,
}

/// The simulated device.
pub struct Simulator {
    codec: Codec,
    framer: Box<dyn Framer>,
    cfg: SimulatorConfig,
    text: bool,
    streams: Vec<Stream>,
    responses: Vec<Response>,
    rng: Rng,
    clock: SharedClock,
    start_ns: u64,
    req_seq: Option<FieldPath>,
    resp_seq: Option<FieldPath>,
    delayed: VecDeque<(u64, Vec<u8>)>,
    next_log_ns: u64,
    log_count: u64,
    events: Vec<FrameEvent>,
    stats: Arc<SimStats>,
}

impl Simulator {
    /// Builds a simulator from a project config and schema.
    pub fn new(
        config: &ProjectConfig,
        schema: &Schema,
        clock: SharedClock,
    ) -> Result<Simulator, CoreError> {
        let codec = Codec::new(config, schema, Role::Device)?;
        let mut streams = Vec::new();
        for s in &config.simulator.streams {
            let target = codec.resolve_tx(&s.message)?;
            let fields = compile_fields(
                &target.inner(),
                &s.fields,
                &format!("simulator stream `{}`", s.message),
            )?;
            streams.push(Stream {
                name: s.message.clone(),
                target,
                fields,
                interval_ns: (1e9 / s.rate_hz.max(0.001)) as u64,
                enabled: s.start_enabled,
                next_ns: 0,
            });
        }
        let mut responses = Vec::new();
        for r in &config.simulator.responses {
            let on = codec.resolve_rx(&r.on)?;
            let reply = codec.resolve_tx(&r.reply)?;
            let fields = compile_fields(
                &reply.inner(),
                &r.payload,
                &format!("simulator response to `{}`", r.on),
            )?;
            let action =
                match &r.action {
                    Some(a) => Some(parse_sim_action(a).ok_or_else(|| {
                        CoreError::Config(format!("unknown simulator action `{a}`"))
                    })?),
                    None => None,
                };
            responses.push(Response {
                on,
                reply,
                fields,
                action,
            });
        }
        let start_ns = clock.now_ns();
        Ok(Simulator {
            framer: build_framer(&config.framing),
            text: config.framing.text_passthrough,
            cfg: config.simulator.clone(),
            streams,
            responses,
            rng: Rng::new(config.simulator.seed),
            req_seq: config
                .correlation
                .request_field
                .as_deref()
                .and_then(|p| FieldPath::parse(p).ok()),
            resp_seq: config
                .correlation
                .response_field
                .as_deref()
                .and_then(|p| FieldPath::parse(p).ok()),
            codec,
            clock,
            start_ns,
            delayed: VecDeque::new(),
            next_log_ns: start_ns + 200_000_000,
            log_count: 0,
            events: Vec::new(),
            stats: Arc::new(SimStats::default()),
        })
    }

    /// Builds a simulator for a project.
    pub fn for_project(project: &Project, clock: SharedClock) -> Result<Simulator, CoreError> {
        let schema = project.require_schema()?;
        Self::new(&project.config, &schema, clock)
    }

    /// Shared counters.
    pub fn stats(&self) -> Arc<SimStats> {
        self.stats.clone()
    }

    /// Enables or disables a stream by message name.
    pub fn set_stream(&mut self, name: &str, on: bool) {
        let now = self.clock.now_ns();
        for s in &mut self.streams {
            if s.name == name || s.target.name() == name {
                if on && !s.enabled {
                    s.next_ns = now;
                }
                s.enabled = on;
            }
        }
    }

    fn t_s(&self, ns: u64) -> f64 {
        ns.saturating_sub(self.start_ns) as f64 / 1e9
    }

    /// Frames a payload with fault injection, appending to `out`.
    fn emit(&mut self, payload: &[u8], out: &mut Vec<u8>) {
        if self.rng.chance(self.cfg.drop_rate) {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.rng.chance(self.cfg.garbage_rate) {
            let n = 1 + self.rng.below(8) as usize;
            for _ in 0..n {
                out.push(self.rng.below(256) as u8);
            }
            self.stats.garbage.fetch_add(1, Ordering::Relaxed);
        }
        let mut wire = self.framer.encode(payload);
        if self.rng.chance(self.cfg.corrupt_rate) && wire.len() > 1 {
            // Flip bits in one byte, keeping the trailing delimiter intact and
            // never creating a new delimiter-valued byte.
            let i = self.rng.below((wire.len() - 1) as u64) as usize;
            let delimiters = [0x00u8, 0xC0];
            for _ in 0..8 {
                let mask = 1u8 << self.rng.below(8);
                let flipped = wire[i] ^ mask;
                if !delimiters.contains(&flipped) {
                    wire[i] = flipped;
                    break;
                }
            }
            self.stats.corrupted.fetch_add(1, Ordering::Relaxed);
        }
        out.extend_from_slice(&wire);
    }

    fn build(&self, target: &MessageTarget, payload: Json) -> Result<DynamicMessage, CoreError> {
        self.codec.build(target, payload)
    }

    /// Feeds bytes received from the host and advances time to `now_ns`.
    /// Returns the bytes the device writes.
    pub fn step(&mut self, incoming: &[u8], now_ns: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut events = std::mem::take(&mut self.events);
        events.clear();
        self.framer.push(incoming, &mut events);
        for ev in events.drain(..) {
            if let FrameEvent::Frame(p) = ev {
                self.handle_request(&p, now_ns, &mut out);
            }
        }
        self.events = events;

        // Delayed responses.
        while self.delayed.front().is_some_and(|(due, _)| *due <= now_ns) {
            if let Some((_, payload)) = self.delayed.pop_front() {
                self.emit(&payload, &mut out);
            }
        }

        // Streams.
        for i in 0..self.streams.len() {
            if !self.streams[i].enabled {
                continue;
            }
            if self.streams[i].next_ns == 0 {
                self.streams[i].next_ns = now_ns;
            }
            // Skip ahead after a long stall instead of bursting.
            if now_ns.saturating_sub(self.streams[i].next_ns) > 1_000_000_000 {
                self.streams[i].next_ns = now_ns;
            }
            while self.streams[i].next_ns <= now_ns {
                let due = self.streams[i].next_ns;
                let t = self.t_s(due);
                let json = eval_fields(&mut self.streams[i].fields, t, &mut self.rng);
                let target = self.streams[i].target.clone();
                self.streams[i].next_ns += self.streams[i].interval_ns.max(1);
                self.stats.stream_msgs.fetch_add(1, Ordering::Relaxed);
                match self
                    .build(&target, json)
                    .and_then(|m| self.codec.encode(&m))
                {
                    Ok(bytes) => self.emit(&bytes, &mut out),
                    Err(e) => tracing::warn!("simulator stream: {e}"),
                }
            }
        }

        // Firmware-style log lines.
        if self.text && self.cfg.log_interval_ms > 0 && now_ns >= self.next_log_ns {
            self.next_log_ns = now_ns + self.cfg.log_interval_ms * 1_000_000;
            self.log_count += 1;
            let t = self.t_s(now_ns);
            let line = match self.log_count % 10 {
                0 => format!("W: sim supply ripple high at t={t:.1}s\n"),
                5 => format!("D: sim adc dma restart #{}\n", self.log_count),
                3 if self.cfg.corrupt_rate > 0.0 || self.cfg.garbage_rate > 0.0 => {
                    "E: sim link noise injected\n".to_string()
                }
                _ => format!(
                    "I: sim uptime={t:.1}s msgs={}\n",
                    self.stats.stream_msgs.load(Ordering::Relaxed)
                ),
            };
            out.extend_from_slice(line.as_bytes());
        }
        out
    }

    fn handle_request(&mut self, payload: &[u8], now_ns: u64, out: &mut Vec<u8>) {
        let Ok(req) = self.codec.decode(payload, now_ns) else {
            return;
        };
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        let Some(idx) = self
            .responses
            .iter()
            .position(|r| self.codec.matches(&req, &r.on))
        else {
            return;
        };
        let seq = self
            .req_seq
            .as_ref()
            .and_then(|p| get_int_at_path(&req.msg, p));
        let t = self.t_s(now_ns);
        let (reply, json, action) = {
            let r = &mut self.responses[idx];
            (
                r.reply.clone(),
                eval_fields(&mut r.fields, t, &mut self.rng),
                r.action.clone(),
            )
        };
        if let Some(a) = action {
            let inner = req
                .member
                .as_deref()
                .and_then(|m| req.msg.get_field_by_name(m))
                .and_then(|v| v.as_message().cloned())
                .unwrap_or_else(|| req.msg.clone());
            let enable = inner.get_field_by_name("enable").and_then(|v| v.as_bool());
            let rate = inner
                .get_field_by_name("rate_hz")
                .and_then(|v| pb_core::schema::value_to_f64(&v))
                .filter(|r| *r > 0.0);
            let (name, on) = match &a {
                SimAction::Toggle(n) => {
                    let cur = self
                        .streams
                        .iter()
                        .any(|s| (s.name == *n || s.target.name() == *n) && s.enabled);
                    (n.clone(), enable.unwrap_or(!cur))
                }
                SimAction::Enable(n) => (n.clone(), true),
                SimAction::Disable(n) => (n.clone(), false),
            };
            if let Some(r) = rate {
                for s in &mut self.streams {
                    if s.name == name || s.target.name() == name {
                        s.interval_ns = (1e9 / r) as u64;
                    }
                }
            }
            self.set_stream(&name, on);
        }
        let mut msg = match self.build(&reply, json) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("simulator response: {e}");
                return;
            }
        };
        if let (Some(p), Some(s)) = (&self.resp_seq, seq) {
            set_int_at_path(&mut msg, p, s);
        }
        let Ok(bytes) = self.codec.encode(&msg) else {
            return;
        };
        self.stats.responses.fetch_add(1, Ordering::Relaxed);
        if self.cfg.latency_ms > 0 {
            self.delayed
                .push_back((now_ns + self.cfg.latency_ms * 1_000_000, bytes));
        } else {
            self.emit(&bytes, out);
        }
    }

    /// Runs against a transport until it fails or `stop` is set.
    pub fn run(mut self, mut transport: Box<dyn Transport>, stop: Arc<AtomicBool>) {
        let mut buf = vec![0u8; 16 * 1024];
        let mut had_peer = transport.has_peer();
        while !stop.load(Ordering::Relaxed) {
            let n = match transport.read(&mut buf) {
                Ok(n) => n,
                Err(_) => return,
            };
            let peer = transport.has_peer();
            if peer && !had_peer {
                self.framer.reset();
            }
            had_peer = peer;
            let now = self.clock.now_ns();
            let out = self.step(&buf[..n], now);
            if !out.is_empty() && transport.write_all(&out).is_err() {
                return;
            }
        }
    }

    /// Message descriptor names of the configured streams (for display).
    pub fn stream_names(&self) -> Vec<String> {
        self.streams.iter().map(|s| s.name.clone()).collect()
    }

    /// Descriptor of the type the simulator sends.
    pub fn tx_root(&self) -> Option<String> {
        self.codec.tx_root().map(|d| d.full_name().to_string())
    }

    /// Full name of a decoded message's type (used in tests).
    pub fn type_of(msg: &DynamicMessage) -> String {
        msg.descriptor().full_name().to_string()
    }
}

/// A factory for the in-process `sim` transport: each call starts a fresh
/// simulator thread on one end of a loopback pair and returns the other.
pub fn sim_factory(project: &Project) -> Result<SimFactory, CoreError> {
    let schema = project.require_schema()?;
    let config = project.config.clone();
    // Validate once so configuration errors surface at connect time.
    Simulator::new(&config, &schema, SystemClock::shared())?;
    Ok(Arc::new(move || {
        let sim = Simulator::new(&config, &schema, SystemClock::shared())?;
        let (host, dev) = loopback_pair();
        let stop = Arc::new(AtomicBool::new(false));
        std::thread::Builder::new()
            .name("protobench-sim".into())
            .spawn(move || sim.run(Box::new(dev), stop))
            .map_err(CoreError::IoBare)?;
        Ok(Box::new(host) as Box<dyn Transport>)
    }))
}

/// Runs the simulator as a TCP server on `port` (0 = any) until `stop`.
/// Calls `on_bound` with the bound port.
pub fn run_tcp_server(
    project: &Project,
    host: &str,
    port: u16,
    stop: Arc<AtomicBool>,
    on_bound: impl FnOnce(u16),
) -> Result<(), CoreError> {
    let sim = Simulator::for_project(project, SystemClock::shared())?;
    let server = TcpServer::bind(host, port)?;
    let bound = server.local_addr()?.port();
    on_bound(bound);
    sim.run(Box::new(server), stop);
    Ok(())
}

/// Starts a TCP simulator in a background thread and returns its port and a
/// stop flag.
pub fn spawn_tcp_server(project: &Project) -> Result<(u16, Arc<AtomicBool>), CoreError> {
    let sim = Simulator::for_project(project, SystemClock::shared())?;
    let server = TcpServer::bind("127.0.0.1", 0)?;
    let port = server.local_addr()?.port();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    std::thread::Builder::new()
        .name("protobench-sim-tcp".into())
        .spawn(move || sim.run(Box::new(server), s2))
        .map_err(CoreError::IoBare)?;
    Ok((port, stop))
}

#[cfg(test)]
mod tests;
