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
use super::input::OperatorInput;
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

/// What a `prompt_operator` step asks.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorPrompt {
    /// Instruction, with `${var}` substituted.
    pub message: String,
    /// Values to enter, each with its pre-fill (the variable's current
    /// value, else the input's `default`).
    pub inputs: Vec<(OperatorInput, Option<Json>)>,
}

/// Operator's answer to `prompt_operator`.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptAnswer {
    /// Done, continue, with the entered values as (var, value).
    Continue(Vec<(String, Json)>),
    /// Fail the step.
    Fail,
}

/// Interactive prompt handler (GUI); `None` means headless.
pub type PromptFn<'a> = Box<dyn FnMut(&OperatorPrompt) -> PromptAnswer + 'a>;

/// Options for a run.
#[derive(Clone, Default)]
pub struct RunOptions {
    /// Headless prompt handling; overrides the sequence's setting.
    pub non_interactive: Option<NonInteractive>,
    /// Set to abort the run.
    pub abort: Arc<AtomicBool>,
    /// Variables that override the sequence's `[vars]` (`--var`).
    pub vars: Vec<(String, Json)>,
}

/// A device a sequence can use: `main` and the `[[devices]]` entries.
#[derive(Clone, Copy)]
pub struct Target<'a> {
    /// Device name.
    pub name: &'a str,
    /// Its session; None while it is configured but not connected.
    pub session: Option<&'a Session>,
    /// Its project config (commands, messages).
    pub config: &'a ProjectConfig,
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

struct Dev<'a> {
    name: &'a str,
    session: Option<&'a Session>,
    config: &'a ProjectConfig,
    /// Messages received from this device, for `expect`.
    inbox: Option<Inbox>,
}

struct Ctx<'a> {
    devs: Vec<Dev<'a>>,
    /// A connected session, for the time base (all devices share one clock).
    clock: &'a Session,
    seq: &'a SequenceFile,
    vars: BTreeMap<String, Json>,
    failed: bool,
    steps: Vec<StepReport>,
    log: Vec<String>,
    on_event: &'a mut dyn FnMut(RunEvent),
    prompt: Option<PromptFn<'a>>,
    headless: NonInteractive,
    abort: Arc<AtomicBool>,
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

/// Runs `seq` on `session` (the project's own device) and returns the
/// report. `on_event` gets live progress; `prompt` answers
/// `prompt_operator` steps (None = headless).
pub fn run_sequence<'a>(
    session: &'a Session,
    config: &'a ProjectConfig,
    seq: &'a SequenceFile,
    opts: RunOptions,
    on_event: &'a mut dyn FnMut(RunEvent),
    prompt: Option<PromptFn<'a>>,
) -> RunReport {
    let targets = [Target {
        name: crate::config::MAIN_DEVICE,
        session: Some(session),
        config,
    }];
    run_sequence_on(&targets, seq, opts, on_event, prompt)
}

/// Device names that steps (and loops) name with `device = …`.
fn named_devices(steps: &[Step], out: &mut Vec<String>) {
    for s in steps {
        if let Some(d) = s.device() {
            if !out.iter().any(|x| x == d) {
                out.push(d.to_string());
            }
        }
        if let Step::Loop { steps, .. } = s {
            named_devices(steps, out);
        }
    }
}

/// Why the devices the steps name cannot be used, if they cannot.
fn device_problem(targets: &[Target<'_>], steps: &[Step]) -> Option<String> {
    if targets.iter().all(|t| t.session.is_none()) {
        return Some("no device is connected".into());
    }
    let mut named = Vec::new();
    named_devices(steps, &mut named);
    for d in named {
        match targets.iter().find(|t| t.name == d) {
            None => {
                return Some(format!(
                    "no device `{d}` (this project has {})",
                    targets
                        .iter()
                        .map(|t| format!("`{}`", t.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
            Some(Target { session: None, .. }) => {
                return Some(format!("device `{d}` is not connected"))
            }
            Some(Target {
                session: Some(s), ..
            }) if !s.state().is_connected() => {
                return Some(format!(
                    "device `{d}` is not connected ({})",
                    s.state().label()
                ))
            }
            _ => {}
        }
    }
    None
}

/// Runs `seq` with several devices: `targets` lists every device of the
/// project (`main` first), connected or not. A run whose steps name a
/// device that is missing or not connected fails at the start with one
/// `devices` error step.
pub fn run_sequence_on<'a>(
    targets: &'a [Target<'a>],
    seq: &'a SequenceFile,
    opts: RunOptions,
    on_event: &'a mut dyn FnMut(RunEvent),
    prompt: Option<PromptFn<'a>>,
) -> RunReport {
    let started = chrono::Local::now().to_rfc3339();
    let t0 = Instant::now();
    let mut vars: BTreeMap<String, Json> = seq
        .vars
        .iter()
        .map(|(k, v)| (k.clone(), toml_to_json(v)))
        .collect();
    vars.extend(opts.vars);
    let main = targets
        .iter()
        .find(|t| t.name == crate::config::MAIN_DEVICE)
        .or(targets.first());
    let project = main.map(|t| t.config.name.clone()).unwrap_or_default();
    let connected: Vec<&Target> = targets.iter().filter(|t| t.session.is_some()).collect();
    let endpoint = match connected.as_slice() {
        [one] if one.name == crate::config::MAIN_DEVICE => {
            one.session.map(|s| s.endpoint()).unwrap_or_default()
        }
        many => many
            .iter()
            .map(|t| {
                format!(
                    "{}: {}",
                    t.name,
                    t.session.map(|s| s.endpoint()).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("; "),
    };
    let mut report = RunReport {
        sequence: seq.name.clone(),
        description: seq.description.clone(),
        project,
        endpoint,
        tool_version: crate::VERSION.to_string(),
        started,
        duration_s: 0.0,
        passed: false,
        aborted: false,
        summary: Summary::default(),
        steps: Vec::new(),
        vars: vars.clone(),
        log: Vec::new(),
    };
    let problem = device_problem(targets, &seq.steps);
    let clock = main
        .and_then(|t| t.session)
        .or_else(|| connected.first().and_then(|t| t.session));
    let (Some(clock), None) = (clock, problem.clone()) else {
        let message = problem.unwrap_or_else(|| "no device is connected".into());
        let step = StepReport {
            index: "0".into(),
            action: "devices".into(),
            label: "check devices".into(),
            status: StepStatus::Error,
            duration_ms: 0.0,
            message: Some(message),
            measured: BTreeMap::new(),
            assertions: Vec::new(),
        };
        on_event(RunEvent::StepFinished(step.clone()));
        report.steps.push(step);
        report.duration_s = t0.elapsed().as_secs_f64();
        report.finalize();
        on_event(RunEvent::Finished(Box::new(report.clone())));
        return report;
    };
    let devs = targets
        .iter()
        .map(|t| Dev {
            name: t.name,
            session: t.session,
            config: t.config,
            inbox: t.session.map(|s| Inbox::new(s.subscribe())),
        })
        .collect();
    let mut ctx = Ctx {
        devs,
        clock,
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
        step_start_ns: clock.now_ns(),
        prev_step_start_ns: clock.now_ns(),
    };
    ctx.run_steps(&seq.steps, "");
    let aborted = ctx.abort.load(Ordering::Relaxed);
    let mut report = RunReport {
        aborted,
        duration_s: t0.elapsed().as_secs_f64(),
        steps: std::mem::take(&mut ctx.steps),
        vars: ctx.vars.clone(),
        log: std::mem::take(&mut ctx.log),
        ..report
    };
    report.finalize();
    (ctx.on_event)(RunEvent::Finished(Box::new(report.clone())));
    report
}

impl<'a> Ctx<'a> {
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
        self.step_start_ns = self.clock.now_ns();
        let prune = self.prev_step_start_ns;
        for inbox in self.devs.iter_mut().filter_map(|d| d.inbox.as_mut()) {
            inbox.drain();
            inbox.prune(prune);
        }
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
            for inbox in self.devs.iter_mut().filter_map(|d| d.inbox.as_mut()) {
                inbox.drain();
            }
            std::thread::sleep((end - Instant::now()).min(Duration::from_millis(10)));
        }
        true
    }

    fn exec(&mut self, step: &Step) -> Outcome {
        match step {
            Step::Send {
                device,
                command,
                message,
                payload,
                expect,
                timeout_ms,
                retries,
                assert,
                save,
            } => self.send(
                device.as_deref(),
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
                device,
                message,
                timeout_ms,
                assert,
            } => self.expect(device.as_deref(), message, *timeout_ms, assert),
            Step::Capture {
                device,
                channels,
                duration_ms,
                samples,
                timeout_ms,
                assert,
            } => self.capture(
                device.as_deref(),
                channels,
                *duration_ms,
                *samples,
                *timeout_ms,
                assert,
            ),
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
            Step::Script {
                code, timeout_ms, ..
            } => self.script(code.as_deref().unwrap_or(""), *timeout_ms),
            Step::PromptOperator { message, inputs } => self.prompt_operator(message, inputs),
        }
    }

    fn prompt_operator(&mut self, message: &str, inputs: &[OperatorInput]) -> Outcome {
        let msg = interpolate(message, &self.vars).unwrap_or_else(|_| message.to_string());
        let prompt = OperatorPrompt {
            message: msg.clone(),
            inputs: inputs
                .iter()
                .map(|i| {
                    let v = self.vars.get(&i.var).cloned().or_else(|| i.default_json());
                    (i.clone(), v)
                })
                .collect(),
        };
        let answers = match self.prompt.as_mut() {
            Some(p) => match p(&prompt) {
                PromptAnswer::Continue(values) => values,
                PromptAnswer::Fail => return Outcome::fail("operator answered fail"),
            },
            None => {
                let missing: Vec<&str> = prompt
                    .inputs
                    .iter()
                    .filter(|(_, v)| v.is_none())
                    .map(|(i, _)| i.var.as_str())
                    .collect();
                if !inputs.is_empty() && missing.is_empty() {
                    prompt
                        .inputs
                        .iter()
                        .filter_map(|(i, v)| Some((i.var.clone(), v.clone()?)))
                        .collect()
                } else if !missing.is_empty() {
                    return Outcome::fail(format!(
                        "no value for {} in non-interactive mode (use --var): {msg}",
                        missing
                            .iter()
                            .map(|m| format!("`{m}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                } else {
                    return match self.headless {
                        NonInteractive::Continue => {
                            self.log_line(format!("operator prompt (auto-continued): {msg}"));
                            let mut o = Outcome::pass();
                            o.message = Some("auto-continued (non-interactive)".into());
                            o
                        }
                        NonInteractive::Fail => {
                            Outcome::fail(format!("operator prompt in non-interactive mode: {msg}"))
                        }
                    };
                }
            }
        };
        // All values are checked before any is stored.
        let mut checked = Vec::new();
        for input in inputs {
            let Some((_, v)) = answers.iter().find(|(k, _)| *k == input.var) else {
                return Outcome::fail(format!("no value for `{}`", input.var));
            };
            match input.validate(v) {
                Ok(v) => checked.push((input.var.clone(), v)),
                Err(e) => return Outcome::fail(format!("input `{}`: {e}", input.var)),
            }
        }
        let mut o = Outcome::pass();
        for (var, v) in checked {
            self.log_line(format!("input {var} = {v}"));
            self.vars.insert(var.clone(), v.clone());
            o.measured.insert(var, v);
        }
        o
    }

    /// The device a step talks to (`main` when it names none).
    fn dev(&self, name: Option<&str>) -> Result<(&'a Session, &'a ProjectConfig, usize), String> {
        let name = name.unwrap_or(crate::config::MAIN_DEVICE);
        let i = self
            .devs
            .iter()
            .position(|d| d.name == name)
            .ok_or_else(|| format!("no device `{name}`"))?;
        let d = &self.devs[i];
        let s = d
            .session
            .ok_or_else(|| format!("device `{name}` is not connected"))?;
        Ok((s, d.config, i))
    }

    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        device: Option<&str>,
        command: Option<&str>,
        message: Option<&str>,
        payload: Option<&toml::Table>,
        expect: Option<&str>,
        timeout_ms: Option<u64>,
        retries: Option<u32>,
        asserts: &[Assertion],
        save: &toml::Table,
    ) -> Outcome {
        let (session, config, di) = match self.dev(device) {
            Ok(d) => d,
            Err(e) => return Outcome::error(e),
        };
        let codec = session.codec();
        let built = match (command, message) {
            (Some(name), _) => {
                let name = interpolate(name, &self.vars).unwrap_or_else(|_| name.to_string());
                let Some(cmd) = config.command(&name) else {
                    return Outcome::error(format!(
                        "no command `{name}` in {}",
                        if di == 0 && device.is_none() {
                            "project.toml".to_string()
                        } else {
                            format!("the project of device `{}`", self.devs[di].name)
                        }
                    ));
                };
                let overrides = match payload {
                    Some(p) => match substitute_vars(table_to_json(p), &self.vars) {
                        Ok(Json::Object(o)) => o.into_iter().collect(),
                        Ok(_) => Vec::new(),
                        Err(e) => return Outcome::error(e.to_string()),
                    },
                    None => Vec::new(),
                };
                build_command(config, codec, cmd, &overrides, &self.vars)
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
                build_inline(config, codec, m, p, None, None, None)
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
        let out = session.send_and_wait(req);
        let mut o = Outcome::pass();
        match out {
            RequestOutcome::Sent => {}
            RequestOutcome::Ok {
                response,
                latency_ns,
                attempts,
            } => {
                if let Some(inbox) = self.devs[di].inbox.as_mut() {
                    inbox.claim(&response);
                }
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

    fn script(&mut self, code: &str, timeout_ms: u64) -> Outcome {
        let devices = self
            .devs
            .iter()
            .filter_map(|d| {
                Some(super::script::ScriptDevice {
                    name: d.name.to_string(),
                    session: d.session?.handle(),
                    config: Arc::new(d.config.clone()),
                })
            })
            .collect();
        let env = super::script::ScriptEnv {
            devices,
            vars: self.vars.clone(),
            abort: self.abort.clone(),
            timeout: Duration::from_millis(timeout_ms),
        };
        let r = super::script::run_script(code, env);
        self.vars = r.vars;
        for line in r.log {
            self.log_line(line);
        }
        let mut o = match (r.error, r.failure) {
            (Some(e), _) => Outcome::error(e),
            (None, Some(f)) => Outcome::fail(f),
            (None, None) => Outcome::pass(),
        };
        if let Some(v) = r.result {
            o.measured.insert("result".into(), v);
        }
        o.assertions = r.checks;
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
        let m = match self.clock.telemetry().get(&ch) {
            None => Err(format!("no channel `{ch}`")),
            Some(c) => match c.last() {
                Some((_, v)) => Ok(Measured::Num(v)),
                None => Err(format!("no samples on `{ch}`")),
            },
        };
        check(a, &ch, m)
    }

    fn expect(
        &mut self,
        device: Option<&str>,
        message: &str,
        timeout_ms: u64,
        asserts: &[Assertion],
    ) -> Outcome {
        let (session, _, di) = match self.dev(device) {
            Ok(d) => d,
            Err(e) => return Outcome::error(e),
        };
        let codec = session.codec().clone();
        let target = match codec.resolve_rx(message) {
            Ok(t) => t,
            Err(e) => return Outcome::error(e.to_string()),
        };
        let since = self.prev_step_start_ns;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let abort = self.abort.clone();
        let Some(inbox) = self.devs[di].inbox.as_mut() else {
            return Outcome::error("internal: no inbox");
        };
        inbox.drain();
        let mut found = inbox.take(since, |m| codec.matches(m, &target));
        while found.is_none() {
            if abort.load(Ordering::Relaxed) {
                return Outcome::error("aborted");
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Outcome::fail(format!("no `{message}` within {timeout_ms} ms"));
            }
            if let Ok(ev) = inbox.rx.recv_timeout(left.min(Duration::from_millis(20))) {
                if let Some(m) = inbox.accept(ev) {
                    if m.host_ts_ns >= since && codec.matches(&m, &target) {
                        found = inbox.msgs.pop_back();
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
        device: Option<&str>,
        channels: &[String],
        duration_ms: Option<u64>,
        samples: Option<usize>,
        timeout_ms: u64,
        asserts: &[Assertion],
    ) -> Outcome {
        let (session, _, di) = match self.dev(device) {
            Ok(d) => d,
            Err(e) => return Outcome::error(e),
        };
        let store = session.telemetry().clone();
        // With a device, names are that device's own channels.
        let prefix = match device {
            Some(_) => crate::config::channel_prefix(self.devs[di].name),
            None => String::new(),
        };
        let channels: Vec<String> = channels.iter().map(|c| format!("{prefix}{c}")).collect();
        let asserts: Vec<Assertion> = asserts
            .iter()
            .map(|a| Assertion {
                channel: a.channel.as_ref().map(|c| format!("{prefix}{c}")),
                ..a.clone()
            })
            .collect();
        let mark = |name: &str| store.get(name).map(|c| c.total()).unwrap_or(0);
        // Every channel the step names, listed or only asserted on, is
        // measured over the capture window alone.
        let mut names: Vec<String> = channels.to_vec();
        for a in &asserts {
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
        for a in &asserts {
            o.assertions.push(check_stat(a, &stats));
        }
        if stats.iter().all(|(_, s)| s.is_none()) && asserts.is_empty() {
            return Outcome::fail("no samples captured");
        }
        o.judge()
    }
}
