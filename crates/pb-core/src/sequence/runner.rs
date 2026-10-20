//! Executes a sequence against a live session.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde_json::Value as Json;

use super::assert::{
    check, check_each, check_stat, measure_all, measure_field, AssertionResult, Measured,
};
use super::model::{Assertion, NonInteractive, SequenceFile, Step};
use super::report::{RunReport, StepReport, StepStatus, Summary};
use crate::codec::DecodedMessage;
use crate::commands::{
    build_command, build_inline, interpolate, substitute_vars, table_to_json, toml_to_json,
};
use crate::config::ProjectConfig;
use crate::session::{Direction, RequestOutcome, Session, SessionEvent};
use crate::telemetry::{stats_of, StatsSnapshot};

/// Progress notifications.
#[derive(Debug, Clone)]
pub enum RunEvent {
    /// A step started.
    StepStarted {
        /// Index (e.g. `7.2.1`).
        index: String,
        /// Label.
        label: String,
    },
    /// A step finished.
    StepFinished(StepReport),
    /// A log line.
    Log(String),
    /// The run finished.
    Finished(Box<RunReport>),
}

/// Operator's answer to `prompt_operator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAnswer {
    /// Done, continue.
    Continue,
    /// Fail the step.
    Fail,
}

/// Interactive prompt handler (GUI); `None` means headless.
pub type PromptFn<'a> = Box<dyn FnMut(&str) -> PromptAnswer + 'a>;

/// Options for a run.
#[derive(Clone, Default)]
pub struct RunOptions {
    /// Headless prompt handling; overrides the sequence's setting.
    pub non_interactive: Option<NonInteractive>,
    /// Set to abort the run.
    pub abort: Arc<AtomicBool>,
}

/// Most messages kept for `expect` steps.
const INBOX_MAX: usize = 50_000;

/// Received messages buffered for `expect` steps. One subscription lives for
/// the whole run, so a reply that arrives before its `expect` step starts
/// (e.g. right after a fire-and-forget `send`) is not missed.
struct Inbox {
    rx: Receiver<SessionEvent>,
    msgs: VecDeque<Arc<DecodedMessage>>,
    /// Replies already used by `send` steps that have not been received
    /// from the subscription yet; they are skipped when they arrive.
    claimed: Vec<Arc<DecodedMessage>>,
}

impl Inbox {
    fn new(rx: Receiver<SessionEvent>) -> Self {
        Inbox {
            rx,
            msgs: VecDeque::new(),
            claimed: Vec::new(),
        }
    }

    /// Buffers a received message; returns it unless it was already claimed.
    fn accept(&mut self, ev: SessionEvent) -> Option<Arc<DecodedMessage>> {
        let SessionEvent::Message {
            dir: Direction::Rx,
            msg,
        } = ev
        else {
            return None;
        };
        if let Some(i) = self.claimed.iter().position(|c| Arc::ptr_eq(c, &msg)) {
            self.claimed.swap_remove(i);
            return None;
        }
        if self.msgs.len() >= INBOX_MAX {
            self.msgs.pop_front();
        }
        self.msgs.push_back(msg.clone());
        Some(msg)
    }

    /// Moves everything queued on the subscription into the buffer.
    fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            self.accept(ev);
        }
    }

    /// Marks a message as used so no `expect` step matches it.
    fn claim(&mut self, msg: &Arc<DecodedMessage>) {
        if let Some(i) = self.msgs.iter().position(|m| Arc::ptr_eq(m, msg)) {
            self.msgs.remove(i);
        } else {
            if self.claimed.len() >= 1000 {
                self.claimed.remove(0);
            }
            self.claimed.push(msg.clone());
        }
    }

    /// Drops messages received before `ns`; no step can match them anymore.
    fn prune(&mut self, ns: u64) {
        self.msgs.retain(|m| m.host_ts_ns >= ns);
    }

    /// Removes and returns the oldest message received at or after `since`
    /// that satisfies `pred`.
    fn take(
        &mut self,
        since: u64,
        pred: impl Fn(&DecodedMessage) -> bool,
    ) -> Option<Arc<DecodedMessage>> {
        let i = self
            .msgs
            .iter()
            .position(|m| m.host_ts_ns >= since && pred(m))?;
        self.msgs.remove(i)
    }
}

struct Ctx<'a> {
    session: &'a Session,
    config: &'a ProjectConfig,
    seq: &'a SequenceFile,
    vars: BTreeMap<String, Json>,
    failed: bool,
    steps: Vec<StepReport>,
    log: Vec<String>,
    on_event: &'a mut dyn FnMut(RunEvent),
    prompt: Option<PromptFn<'a>>,
    headless: NonInteractive,
    abort: Arc<AtomicBool>,
    inbox: Inbox,
    /// Session time when the current step started.
    step_start_ns: u64,
    /// Session time when the previous step started: `expect` matches
    /// messages received since then.
    prev_step_start_ns: u64,
}

struct Outcome {
    status: StepStatus,
    message: Option<String>,
    measured: BTreeMap<String, Json>,
    assertions: Vec<AssertionResult>,
}

impl Outcome {
    fn pass() -> Self {
        Outcome {
            status: StepStatus::Pass,
            message: None,
            measured: BTreeMap::new(),
            assertions: Vec::new(),
        }
    }
    fn error(m: impl Into<String>) -> Self {
        Outcome {
            status: StepStatus::Error,
            message: Some(m.into()),
            ..Self::pass()
        }
    }
    fn fail(m: impl Into<String>) -> Self {
        Outcome {
            status: StepStatus::Fail,
            message: Some(m.into()),
            ..Self::pass()
        }
    }
    /// Sets the status from the assertions.
    fn judge(mut self) -> Self {
        let failed = self.assertions.iter().filter(|a| !a.passed).count();
        if failed > 0 && self.status == StepStatus::Pass {
            self.status = StepStatus::Fail;
            self.message = Some(format!(
                "{failed} of {} assertion{} failed",
                self.assertions.len(),
                if self.assertions.len() == 1 { "" } else { "s" }
            ));
        }
        self
    }
}

/// Runs `seq` on `session` and returns the report. `on_event` gets live
/// progress; `prompt` answers `prompt_operator` steps (None = headless).
pub fn run_sequence<'a>(
    session: &'a Session,
    config: &'a ProjectConfig,
    seq: &'a SequenceFile,
    opts: RunOptions,
    on_event: &'a mut dyn FnMut(RunEvent),
    prompt: Option<PromptFn<'a>>,
) -> RunReport {
    let started = chrono::Local::now().to_rfc3339();
    let t0 = Instant::now();
    let vars = seq
        .vars
        .iter()
        .map(|(k, v)| (k.clone(), toml_to_json(v)))
        .collect();
    let mut ctx = Ctx {
        session,
        config,
        seq,
        vars,
        failed: false,
        steps: Vec::new(),
        log: Vec::new(),
        on_event,
        prompt,
        headless: opts
            .non_interactive
            .or(seq.non_interactive)
            .unwrap_or(NonInteractive::Fail),
        abort: opts.abort,
        inbox: Inbox::new(session.subscribe()),
        step_start_ns: session.now_ns(),
        prev_step_start_ns: session.now_ns(),
    };
    ctx.run_steps(&seq.steps, "");
    let aborted = ctx.abort.load(Ordering::Relaxed);
    let mut report = RunReport {
        sequence: seq.name.clone(),
        description: seq.description.clone(),
        project: config.name.clone(),
        endpoint: session.endpoint(),
        tool_version: crate::VERSION.to_string(),
        started,
        duration_s: t0.elapsed().as_secs_f64(),
        passed: false,
        aborted,
        summary: Summary::default(),
        steps: std::mem::take(&mut ctx.steps),
        vars: ctx.vars.clone(),
        log: std::mem::take(&mut ctx.log),
    };
    report.finalize();
    (ctx.on_event)(RunEvent::Finished(Box::new(report.clone())));
    report
}

impl Ctx<'_> {
    fn log_line(&mut self, s: String) {
        self.log.push(s.clone());
        (self.on_event)(RunEvent::Log(s));
    }

    fn run_steps(&mut self, steps: &[Step], prefix: &str) {
        for (i, step) in steps.iter().enumerate() {
            let index = format!("{prefix}{}", i + 1);
            if let Step::Loop { count, steps } = step {
                for it in 1..=*count {
                    self.run_steps(steps, &format!("{index}.{it}."));
                }
                continue;
            }
            self.run_one(step, index);
        }
    }

    fn run_one(&mut self, step: &Step, index: String) {
        let label = step.label();
        let skip = if self.abort.load(Ordering::Relaxed) {
            Some("aborted")
        } else if self.failed && !self.seq.continue_on_failure {
            Some("skipped after an earlier failure")
        } else {
            None
        };
        if let Some(why) = skip {
            let r = StepReport {
                index,
                action: step.action().into(),
                label,
                status: StepStatus::Skipped,
                duration_ms: 0.0,
                message: Some(why.into()),
                measured: BTreeMap::new(),
                assertions: Vec::new(),
            };
            (self.on_event)(RunEvent::StepFinished(r.clone()));
            self.steps.push(r);
            return;
        }
        (self.on_event)(RunEvent::StepStarted {
            index: index.clone(),
            label: label.clone(),
        });
        let t = Instant::now();
        self.prev_step_start_ns = self.step_start_ns;
        self.step_start_ns = self.session.now_ns();
        self.inbox.drain();
        self.inbox.prune(self.prev_step_start_ns);
        let out = self.exec(step);
        if matches!(out.status, StepStatus::Fail | StepStatus::Error) {
            self.failed = true;
        }
        let r = StepReport {
            index,
            action: step.action().into(),
            label,
            status: out.status,
            duration_ms: t.elapsed().as_secs_f64() * 1000.0,
            message: out.message,
            measured: out.measured,
            assertions: out.assertions,
        };
        (self.on_event)(RunEvent::StepFinished(r.clone()));
        self.steps.push(r);
    }

    /// Sleeps up to `d`, returning false if aborted. Keeps the inbox
    /// drained so the subscription queue never fills.
    fn sleep(&mut self, d: Duration) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.abort.load(Ordering::Relaxed) {
                return false;
            }
            self.inbox.drain();
            std::thread::sleep((end - Instant::now()).min(Duration::from_millis(10)));
        }
        true
    }

    fn exec(&mut self, step: &Step) -> Outcome {
        match step {
            Step::Send {
                command,
                message,
                payload,
                expect,
                timeout_ms,
                retries,
                assert,
                save,
            } => self.send(
                command.as_deref(),
                message.as_deref(),
                payload.as_ref(),
                expect.as_deref(),
                *timeout_ms,
                *retries,
                assert,
                save,
            ),
            Step::Wait { ms } => {
                if self.sleep(Duration::from_millis(*ms)) {
                    Outcome::pass()
                } else {
                    Outcome::error("aborted")
                }
            }
            Step::Expect {
                message,
                timeout_ms,
                assert,
            } => self.expect(message, *timeout_ms, assert),
            Step::Capture {
                channels,
                duration_ms,
                samples,
                timeout_ms,
                assert,
            } => self.capture(channels, *duration_ms, *samples, *timeout_ms, assert),
            Step::Set { var, value, vars } => {
                let mut set = |k: &str, v: &toml::Value| -> Result<(), String> {
                    let j =
                        substitute_vars(toml_to_json(v), &self.vars).map_err(|e| e.to_string())?;
                    self.vars.insert(k.to_string(), j);
                    Ok(())
                };
                let mut r = Ok(());
                if let (Some(k), Some(v)) = (var, value) {
                    r = r.and_then(|_| set(k, v));
                }
                for (k, v) in vars {
                    r = r.and_then(|_| set(k, v));
                }
                match r {
                    Ok(()) => Outcome::pass(),
                    Err(e) => Outcome::error(e),
                }
            }
            Step::Loop { .. } => Outcome::pass(),
            Step::Log { text } => match interpolate(text, &self.vars) {
                Ok(t) => {
                    self.log_line(t.clone());
                    let mut o = Outcome::pass();
                    o.measured.insert("text".into(), Json::String(t));
                    o
                }
                Err(e) => Outcome::error(e.to_string()),
            },
            Step::PromptOperator { message } => {
                let msg = interpolate(message, &self.vars).unwrap_or_else(|_| message.clone());
                match self.prompt.as_mut() {
                    Some(p) => match p(&msg) {
                        PromptAnswer::Continue => Outcome::pass(),
                        PromptAnswer::Fail => Outcome::fail("operator answered fail"),
                    },
                    None => match self.headless {
                        NonInteractive::Continue => {
                            self.log_line(format!("operator prompt (auto-continued): {msg}"));
                            let mut o = Outcome::pass();
                            o.message = Some("auto-continued (non-interactive)".into());
                            o
                        }
                        NonInteractive::Fail => {
                            Outcome::fail(format!("operator prompt in non-interactive mode: {msg}"))
                        }
                    },
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        command: Option<&str>,
        message: Option<&str>,
        payload: Option<&toml::Table>,
        expect: Option<&str>,
        timeout_ms: Option<u64>,
        retries: Option<u32>,
        asserts: &[Assertion],
        save: &toml::Table,
    ) -> Outcome {
        let codec = self.session.codec();
        let built = match (command, message) {
            (Some(name), _) => {
                let name = interpolate(name, &self.vars).unwrap_or_else(|_| name.to_string());
                let Some(cmd) = self.config.command(&name) else {
                    return Outcome::error(format!("no command `{name}` in project.toml"));
                };
                let overrides = match payload {
                    Some(p) => match substitute_vars(table_to_json(p), &self.vars) {
                        Ok(Json::Object(o)) => o.into_iter().collect(),
                        Ok(_) => Vec::new(),
                        Err(e) => return Outcome::error(e.to_string()),
                    },
                    None => Vec::new(),
                };
                build_command(self.config, codec, cmd, &overrides, &self.vars)
            }
            (None, Some(m)) => {
                let p = match substitute_vars(
                    payload
                        .map(table_to_json)
                        .unwrap_or(Json::Object(Default::default())),
                    &self.vars,
                ) {
                    Ok(p) => p,
                    Err(e) => return Outcome::error(e.to_string()),
                };
                build_inline(self.config, codec, m, p, None, None, None)
            }
            (None, None) => return Outcome::error("send needs `command` or `message`"),
        };
        let mut req = match built {
            Ok(r) => r,
            Err(e) => return Outcome::error(e.to_string()),
        };
        if let Some(e) = expect {
            match codec.resolve_rx(e) {
                Ok(t) => req.expect = Some(t),
                Err(e) => return Outcome::error(e.to_string()),
            }
        }
        if let Some(t) = timeout_ms {
            req.timeout_ms = t;
        }
        if let Some(r) = retries {
            req.retries = r;
        }
        if req.expect.is_none() && asserts.iter().any(|a| a.path.is_some()) {
            return Outcome::error("field assertions need an expected reply (`expect`)");
        }
        let out = self.session.send_and_wait(req);
        let mut o = Outcome::pass();
        match out {
            RequestOutcome::Sent => {}
            RequestOutcome::Ok {
                response,
                latency_ns,
                attempts,
            } => {
                self.inbox.claim(&response);
                o.measured.insert("response".into(), response.to_json());
                o.measured
                    .insert("latency_ms".into(), Json::from(latency_ns as f64 / 1e6));
                o.measured.insert("attempts".into(), Json::from(attempts));
                o.assertions = self.check_message(asserts, &response);
                for (var, path) in save {
                    let Some(path) = path.as_str() else { continue };
                    if let Ok(m) = measure_field(&response.msg, path) {
                        let j = match m {
                            Measured::Num(v) => Json::from(v),
                            Measured::Bool(b) => Json::Bool(b),
                            Measured::Str(s) => Json::String(s),
                            Measured::Enum(_, n) => Json::String(n),
                        };
                        self.vars.insert(var.clone(), j);
                    }
                }
            }
            RequestOutcome::Timeout { attempts } => {
                return Outcome::fail(format!("no reply after {attempts} attempt(s)"));
            }
            RequestOutcome::Error(e) if e == "not connected" => return Outcome::error(e),
            RequestOutcome::Error(e) => return Outcome::fail(e),
        }
        if o.assertions.is_empty() {
            o.assertions = asserts
                .iter()
                .filter(|a| a.channel.is_some())
                .map(|a| self.check_channel_last(a))
                .collect();
        }
        o.judge()
    }

    fn check_message(&self, asserts: &[Assertion], msg: &DecodedMessage) -> Vec<AssertionResult> {
        asserts
            .iter()
            .map(|a| match (&a.path, &a.channel) {
                (Some(p), _) => check_each(a, p, measure_all(&msg.msg, p)),
                (None, Some(_)) => self.check_channel_last(a),
                (None, None) => check(a, "?", Err("assertion needs `path` or `channel`".into())),
            })
            .collect()
    }

    fn check_channel_last(&self, a: &Assertion) -> AssertionResult {
        let ch = a.channel.clone().unwrap_or_default();
        let m = match self.session.telemetry().get(&ch) {
            None => Err(format!("no channel `{ch}`")),
            Some(c) => match c.last() {
                Some((_, v)) => Ok(Measured::Num(v)),
                None => Err(format!("no samples on `{ch}`")),
            },
        };
        check(a, &ch, m)
    }

    fn expect(&mut self, message: &str, timeout_ms: u64, asserts: &[Assertion]) -> Outcome {
        let codec = self.session.codec().clone();
        let target = match codec.resolve_rx(message) {
            Ok(t) => t,
            Err(e) => return Outcome::error(e.to_string()),
        };
        let since = self.prev_step_start_ns;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        self.inbox.drain();
        let mut found = self.inbox.take(since, |m| codec.matches(m, &target));
        while found.is_none() {
            if self.abort.load(Ordering::Relaxed) {
                return Outcome::error("aborted");
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Outcome::fail(format!("no `{message}` within {timeout_ms} ms"));
            }
            if let Ok(ev) = self
                .inbox
                .rx
                .recv_timeout(left.min(Duration::from_millis(20)))
            {
                if let Some(m) = self.inbox.accept(ev) {
                    if m.host_ts_ns >= since && codec.matches(&m, &target) {
                        found = self.inbox.msgs.pop_back();
                    }
                }
            }
        }
        let Some(msg) = found else {
            return Outcome::error("internal: no message");
        };
        let mut o = Outcome::pass();
        o.measured.insert("message".into(), msg.to_json());
        o.assertions = self.check_message(asserts, &msg);
        o.judge()
    }

    fn capture(
        &mut self,
        channels: &[String],
        duration_ms: Option<u64>,
        samples: Option<usize>,
        timeout_ms: u64,
        asserts: &[Assertion],
    ) -> Outcome {
        let store = self.session.telemetry().clone();
        let mark = |name: &str| store.get(name).map(|c| c.total()).unwrap_or(0);
        // Every channel the step names, listed or only asserted on, is
        // measured over the capture window alone.
        let mut names: Vec<String> = channels.to_vec();
        for a in asserts {
            if let Some(ch) = &a.channel {
                if !ch.is_empty() && !names.contains(ch) {
                    names.push(ch.clone());
                }
            }
        }
        let marks: Vec<u64> = names.iter().map(|c| mark(c)).collect();
        let t0 = Instant::now();
        match (duration_ms, samples) {
            (Some(d), _) => {
                if !self.sleep(Duration::from_millis(d)) {
                    return Outcome::error("aborted");
                }
            }
            (None, Some(n)) => {
                let deadline = t0 + Duration::from_millis(timeout_ms);
                loop {
                    let got = mark(&channels[0]).saturating_sub(marks[0]);
                    if got >= n as u64 {
                        break;
                    }
                    if Instant::now() >= deadline {
                        return Outcome::fail(format!(
                            "only {got} of {n} samples within {timeout_ms} ms"
                        ));
                    }
                    if !self.sleep(Duration::from_millis(5)) {
                        return Outcome::error("aborted");
                    }
                }
            }
            (None, None) => return Outcome::error("capture needs `duration_ms` or `samples`"),
        }
        let elapsed = t0.elapsed().as_secs_f64();
        let mut o = Outcome::pass();
        let mut stats: Vec<(String, Option<StatsSnapshot>)> = Vec::new();
        for (name, m) in names.iter().zip(&marks) {
            let snap = store.get(name).and_then(|c| {
                let data = c.data.read();
                let s = data
                    .ring
                    .since(*m)
                    .unwrap_or_else(|| data.ring.iter().collect());
                (!s.is_empty()).then(|| stats_of(&s, Some(elapsed)))
            });
            if let Some(s) = &snap {
                o.measured
                    .insert(name.clone(), serde_json::to_value(s).unwrap_or(Json::Null));
            }
            stats.push((name.clone(), snap));
        }
        for a in asserts {
            o.assertions.push(check_stat(a, &stats));
        }
        if stats.iter().all(|(_, s)| s.is_none()) && asserts.is_empty() {
            return Outcome::fail("no samples captured");
        }
        o.judge()
    }
}
