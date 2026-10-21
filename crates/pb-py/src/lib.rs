//! `protobench`: drive ProtoBench projects from Python.
//!
//! ```python
//! import protobench
//!
//! p = protobench.Project("examples/demo-sensor")
//! with p.connect(profile="sim") as s:           # every device of the project
//!     print(s.send("Ping"))                     # {'status': 'ok', 'response': {...}, ...}
//!     s.send("Set Bias", millivolts=300)
//!     s.send("Start Stream")
//!     time.sleep(1)
//!     print(s.stats("telemetry.vout_raw")["mean"])
//!     t, v = s.samples("telemetry.vout_raw")
//!     print(s.device("ref").last("reading.vout_mv"))
//!     print(s.run_sequence("smoke", vars={"load_ohms": 4700})["passed"])
//! ```
//!
//! Blocking calls (connecting, sending, waiting, running a sequence)
//! release the GIL, so other Python threads keep running.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use parking_lot::Mutex;
use pyo3::exceptions::{
    PyConnectionError, PyKeyError, PyRuntimeError, PyTimeoutError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use serde_json::Value as Json;

use pb_core::bench::Bench;
use pb_core::codec::DecodedMessage;
use pb_core::commands::{build_command, build_inline};
use pb_core::config::{channel_prefix, MAIN_DEVICE};
use pb_core::sequence::{run_sequence_on, NonInteractive, RunOptions, SequenceFile, Target};
use pb_core::session::{
    ConnState, Direction, RequestOutcome, Session as CoreSession, SessionEvent,
};

fn rt(e: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// JSON to Python objects.
fn to_py<'py>(py: Python<'py>, v: &Json) -> PyResult<Bound<'py, PyAny>> {
    Ok(match v {
        Json::Null => py.None().into_bound(py),
        Json::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        Json::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => i.into_pyobject(py)?.into_any(),
            (None, Some(u)) => u.into_pyobject(py)?.into_any(),
            _ => PyFloat::new(py, n.as_f64().unwrap_or(f64::NAN)).into_any(),
        },
        Json::String(s) => PyString::new(py, s).into_any(),
        Json::Array(a) => {
            let items = a
                .iter()
                .map(|x| to_py(py, x))
                .collect::<PyResult<Vec<_>>>()?;
            PyList::new(py, items)?.into_any()
        }
        Json::Object(o) => {
            let d = PyDict::new(py);
            for (k, x) in o {
                d.set_item(k, to_py(py, x)?)?;
            }
            d.into_any()
        }
    })
}

/// Python objects to JSON (dicts, lists, tuples, str, int, float, bool,
/// None).
fn from_py(o: &Bound<'_, PyAny>) -> PyResult<Json> {
    if o.is_none() {
        return Ok(Json::Null);
    }
    if let Ok(b) = o.cast::<PyBool>() {
        return Ok(Json::Bool(b.is_true()));
    }
    if o.cast::<PyInt>().is_ok() {
        if let Ok(i) = o.extract::<i64>() {
            return Ok(Json::from(i));
        }
        return Ok(Json::from(o.extract::<u64>()?));
    }
    if let Ok(f) = o.cast::<PyFloat>() {
        return Ok(serde_json::Number::from_f64(f.value())
            .map(Json::Number)
            .unwrap_or(Json::Null));
    }
    if let Ok(s) = o.cast::<PyString>() {
        return Ok(Json::String(s.extract::<String>()?));
    }
    if let Ok(d) = o.cast::<PyDict>() {
        let mut m = serde_json::Map::new();
        for (k, v) in d.iter() {
            m.insert(k.str()?.extract::<String>()?, from_py(&v)?);
        }
        return Ok(Json::Object(m));
    }
    if let Ok(l) = o.cast::<PyList>() {
        return Ok(Json::Array(
            l.iter().map(|x| from_py(&x)).collect::<PyResult<_>>()?,
        ));
    }
    if let Ok(t) = o.cast::<PyTuple>() {
        return Ok(Json::Array(
            t.iter().map(|x| from_py(&x)).collect::<PyResult<_>>()?,
        ));
    }
    Err(PyValueError::new_err(format!(
        "cannot send a {} value",
        o.get_type().name()?
    )))
}

fn kwargs_json(kw: Option<&Bound<'_, PyDict>>) -> PyResult<Vec<(String, Json)>> {
    let mut out = Vec::new();
    if let Some(kw) = kw {
        for (k, v) in kw.iter() {
            out.push((k.str()?.extract::<String>()?, from_py(&v)?));
        }
    }
    Ok(out)
}

/// Messages received from one device, for `wait` and `messages`.
struct Inbox {
    rx: Receiver<SessionEvent>,
    msgs: VecDeque<Arc<DecodedMessage>>,
    claimed: Vec<Arc<DecodedMessage>>,
}

/// Most messages kept per device.
const INBOX_MAX: usize = 10_000;

impl Inbox {
    fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            let SessionEvent::Message {
                dir: Direction::Rx,
                msg,
            } = ev
            else {
                continue;
            };
            if let Some(i) = self.claimed.iter().position(|c| Arc::ptr_eq(c, &msg)) {
                self.claimed.swap_remove(i);
                continue;
            }
            if self.msgs.len() >= INBOX_MAX {
                self.msgs.pop_front();
            }
            self.msgs.push_back(msg);
        }
    }

    /// A reply `send` already returned is not offered to `wait` again.
    fn claim(&mut self, msg: &Arc<DecodedMessage>) {
        self.drain();
        match self.msgs.iter().position(|m| Arc::ptr_eq(m, msg)) {
            Some(i) => {
                self.msgs.remove(i);
            }
            None => {
                if self.claimed.len() >= 1000 {
                    self.claimed.remove(0);
                }
                self.claimed.push(msg.clone());
            }
        }
    }
}

struct State {
    project: pb_core::project::Project,
    bench: Bench,
    inboxes: HashMap<String, Inbox>,
}

type Shared = Arc<Mutex<State>>;

fn session_of(st: &Shared, device: &str) -> PyResult<Arc<CoreSession>> {
    st.lock()
        .bench
        .session(device)
        .cloned()
        .ok_or_else(|| PyConnectionError::new_err(format!("device `{device}` is not connected")))
}

fn outcome_json(o: &RequestOutcome) -> Json {
    match o {
        RequestOutcome::Sent => serde_json::json!({"status": "sent"}),
        RequestOutcome::Ok {
            response,
            latency_ns,
            attempts,
        } => serde_json::json!({
            "status": "ok",
            "response": response.to_json(),
            "latency_ms": *latency_ns as f64 / 1e6,
            "attempts": attempts,
        }),
        RequestOutcome::Timeout { attempts } => {
            serde_json::json!({"status": "timeout", "attempts": attempts})
        }
        RequestOutcome::Error(e) => serde_json::json!({"status": "error", "error": e}),
    }
}

fn do_send(
    py: Python<'_>,
    st: &Shared,
    device: &str,
    req: Result<pb_core::commands::SendRequest, String>,
) -> PyResult<Py<PyAny>> {
    let req = req.map_err(PyValueError::new_err)?;
    let s = session_of(st, device)?;
    let out = py.detach(move || s.send_and_wait(req));
    if let RequestOutcome::Ok { response, .. } = &out {
        if let Some(inbox) = st.lock().inboxes.get_mut(device) {
            inbox.claim(response);
        }
    }
    Ok(to_py(py, &outcome_json(&out))?.unbind())
}

fn send_command(
    py: Python<'_>,
    st: &Shared,
    device: &str,
    command: &str,
    fields: Vec<(String, Json)>,
) -> PyResult<Py<PyAny>> {
    let req = {
        let g = st.lock();
        let d = g
            .project
            .device(device)
            .ok_or_else(|| PyKeyError::new_err(format!("no device `{device}`")))?;
        let cfg = &d.project.config;
        let cmd = cfg
            .command(command)
            .ok_or_else(|| PyKeyError::new_err(format!("no command `{command}` on `{device}`")))?;
        let s = g.bench.session(device).ok_or_else(|| {
            PyConnectionError::new_err(format!("device `{device}` is not connected"))
        })?;
        build_command(cfg, s.codec(), cmd, &fields, &Default::default()).map_err(|e| e.to_string())
    };
    do_send(py, st, device, req)
}

fn send_message(
    py: Python<'_>,
    st: &Shared,
    device: &str,
    message: &str,
    payload: Json,
    expect: Option<&str>,
    timeout_ms: Option<u64>,
) -> PyResult<Py<PyAny>> {
    let req = {
        let g = st.lock();
        let d = g
            .project
            .device(device)
            .ok_or_else(|| PyKeyError::new_err(format!("no device `{device}`")))?;
        let s = g.bench.session(device).ok_or_else(|| {
            PyConnectionError::new_err(format!("device `{device}` is not connected"))
        })?;
        build_inline(
            &d.project.config,
            s.codec(),
            message,
            payload,
            expect,
            timeout_ms,
            None,
        )
        .map_err(|e| e.to_string())
    };
    do_send(py, st, device, req)
}

/// Waits for a message of type `message` (oldest first).
fn wait_for(
    py: Python<'_>,
    st: &Shared,
    device: &str,
    message: &str,
    timeout_s: f64,
) -> PyResult<Py<PyAny>> {
    let s = session_of(st, device)?;
    let target = s
        .codec()
        .resolve_rx(message)
        .map_err(|e| PyKeyError::new_err(e.to_string()))?;
    let codec = s.codec().clone();
    let st2 = st.clone();
    let dev = device.to_string();
    let found = py.detach(move || {
        let end = Instant::now() + Duration::from_secs_f64(timeout_s.max(0.0));
        loop {
            {
                let mut g = st2.lock();
                if let Some(inbox) = g.inboxes.get_mut(&dev) {
                    inbox.drain();
                    if let Some(i) = inbox.msgs.iter().position(|m| codec.matches(m, &target)) {
                        return inbox.msgs.remove(i).map(|m| m.to_json());
                    }
                }
            }
            if Instant::now() >= end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    match found {
        Some(j) => Ok(to_py(py, &j)?.unbind()),
        None => Err(PyTimeoutError::new_err(format!(
            "no `{message}` from `{device}` within {timeout_s} s"
        ))),
    }
}

fn channel(st: &Shared, name: &str) -> Option<Arc<pb_core::telemetry::Channel>> {
    st.lock().bench.telemetry().get(name)
}

fn last_of(st: &Shared, name: &str) -> Option<f64> {
    channel(st, name).and_then(|c| c.last()).map(|(_, v)| v)
}

fn stats_of(py: Python<'_>, st: &Shared, name: &str) -> PyResult<Py<PyAny>> {
    let c = channel(st, name).ok_or_else(|| PyKeyError::new_err(format!("no channel `{name}`")))?;
    let j = serde_json::to_value(c.stats()).map_err(rt)?;
    Ok(to_py(py, &j)?.unbind())
}

fn samples_of(st: &Shared, name: &str, since_s: Option<f64>) -> PyResult<(Vec<f64>, Vec<f64>)> {
    let c = channel(st, name).ok_or_else(|| PyKeyError::new_err(format!("no channel `{name}`")))?;
    let d = c.data.read();
    let from = since_s.unwrap_or(f64::NEG_INFINITY);
    Ok(d.ring.iter().filter(|(t, _)| *t >= from).unzip())
}

fn state_label(st: &Shared, device: &str) -> String {
    st.lock()
        .bench
        .session(device)
        .map(|s| s.state().label())
        .unwrap_or_else(|| ConnState::Disconnected.label())
}

/// A ProtoBench project folder.
#[pyclass(module = "protobench", name = "Project", frozen)]
struct Project {
    inner: pb_core::project::Project,
}

#[pymethods]
impl Project {
    /// Opens a project folder; raises ValueError if it has errors.
    #[new]
    fn new(path: PathBuf) -> PyResult<Self> {
        let p = pb_core::project::Project::open(&path);
        if p.has_errors() {
            let errs: Vec<String> = p.errors().map(|d| d.to_string()).collect();
            return Err(PyValueError::new_err(format!(
                "project {} has errors:\n{}",
                path.display(),
                errs.join("\n")
            )));
        }
        Ok(Project { inner: p })
    }

    /// Project name.
    #[getter]
    fn name(&self) -> String {
        self.inner.config.name.clone()
    }

    /// Project folder.
    #[getter]
    fn path(&self) -> PathBuf {
        self.inner.dir.clone()
    }

    /// Devices: `main` and the `[[devices]]` names.
    #[getter]
    fn devices(&self) -> Vec<String> {
        self.inner
            .all_devices()
            .iter()
            .map(|d| d.name.to_string())
            .collect()
    }

    /// Sequence names.
    #[getter]
    fn sequences(&self) -> Vec<String> {
        self.inner.sequence_names()
    }

    /// Connection profile names of a device.
    #[pyo3(signature = (device = MAIN_DEVICE))]
    fn profiles(&self, device: &str) -> PyResult<Vec<String>> {
        let d = self
            .inner
            .device(device)
            .ok_or_else(|| PyKeyError::new_err(format!("no device `{device}`")))?;
        Ok(d.project
            .config
            .connection
            .profiles
            .iter()
            .map(|p| p.name.clone())
            .collect())
    }

    /// Connects every device (or those in `only`) and returns a Session.
    /// `profile`/`port` are for `main`; `profiles`/`ports` map other
    /// device names to theirs.
    #[pyo3(signature = (profile=None, port=None, *, profiles=None, ports=None, only=None, timeout_s=5.0))]
    #[allow(clippy::too_many_arguments)]
    fn connect(
        &self,
        py: Python<'_>,
        profile: Option<String>,
        port: Option<String>,
        profiles: Option<HashMap<String, String>>,
        ports: Option<HashMap<String, String>>,
        only: Option<Vec<String>>,
        timeout_s: f64,
    ) -> PyResult<Session> {
        let project = self.inner.clone();
        let mut profiles = profiles.unwrap_or_default();
        let mut ports = ports.unwrap_or_default();
        if let Some(p) = profile {
            profiles.insert(MAIN_DEVICE.into(), p);
        }
        if let Some(p) = port {
            ports.insert(MAIN_DEVICE.into(), p);
        }
        for d in profiles
            .keys()
            .chain(ports.keys())
            .chain(only.iter().flatten())
        {
            if project.device(d).is_none() {
                return Err(PyKeyError::new_err(format!("no device `{d}`")));
            }
        }
        let state = py.detach(move || -> Result<State, String> {
            let mut bench = Bench::default();
            let mut inboxes = HashMap::new();
            let names: Vec<String> = project
                .all_devices()
                .iter()
                .map(|d| d.name.to_string())
                .filter(|n| only.as_ref().is_none_or(|o| o.contains(n)))
                .collect();
            for n in &names {
                let d = bench
                    .start(
                        &project,
                        n,
                        profiles.get(n).map(String::as_str),
                        ports.get(n).cloned(),
                        &|dp: &pb_core::project::Project| pb_sim::sim_factory(dp),
                        |_| {},
                    )
                    .map_err(|e| format!("{n}: {e}"))?;
                inboxes.insert(
                    n.clone(),
                    Inbox {
                        rx: d.session.subscribe(),
                        msgs: VecDeque::new(),
                        claimed: Vec::new(),
                    },
                );
            }
            let limit = Duration::from_secs_f64(timeout_s.max(0.0));
            for n in &names {
                let s = bench.session(n).expect("started");
                let state = s.wait_connected(limit);
                if !state.is_connected() {
                    return Err(format!(
                        "{n}: cannot connect ({}): {}",
                        s.endpoint(),
                        state.label()
                    ));
                }
            }
            Ok(State {
                project,
                bench,
                inboxes,
            })
        });
        let state = state.map_err(PyConnectionError::new_err)?;
        Ok(Session {
            st: Arc::new(Mutex::new(state)),
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "<protobench.Project {:?} at {}>",
            self.inner.config.name,
            self.inner.dir.display()
        )
    }
}

/// The running devices of a project. Methods address `main`; use
/// `device(name)` for another device.
#[pyclass(module = "protobench", name = "Session", frozen)]
struct Session {
    st: Shared,
}

#[pymethods]
impl Session {
    /// Sends a project command; keyword arguments override payload fields.
    /// Returns {status, response, latency_ms, attempts, error}.
    #[pyo3(signature = (command, **fields))]
    fn send(
        &self,
        py: Python<'_>,
        command: &str,
        fields: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        send_command(py, &self.st, MAIN_DEVICE, command, kwargs_json(fields)?)
    }

    /// Sends any message (a oneof member or type name) with a payload.
    #[pyo3(signature = (message, payload=None, expect=None, timeout_ms=None))]
    fn send_msg(
        &self,
        py: Python<'_>,
        message: &str,
        payload: Option<&Bound<'_, PyAny>>,
        expect: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> PyResult<Py<PyAny>> {
        let p = payload
            .map(from_py)
            .transpose()?
            .unwrap_or(Json::Object(Default::default()));
        send_message(py, &self.st, MAIN_DEVICE, message, p, expect, timeout_ms)
    }

    /// Waits for a received message of a type; raises TimeoutError.
    #[pyo3(signature = (message, timeout_s=1.0))]
    fn wait(&self, py: Python<'_>, message: &str, timeout_s: f64) -> PyResult<Py<PyAny>> {
        wait_for(py, &self.st, MAIN_DEVICE, message, timeout_s)
    }

    /// The newest value of a channel (full name), or None.
    fn last(&self, channel: &str) -> Option<f64> {
        last_of(&self.st, channel)
    }

    /// Statistics of a channel: count, mean, min, max, std, rate_hz, last.
    fn stats(&self, py: Python<'_>, channel: &str) -> PyResult<Py<PyAny>> {
        stats_of(py, &self.st, channel)
    }

    /// (times, values) of a channel, optionally from `since_s` on.
    #[pyo3(signature = (channel, since_s=None))]
    fn samples(&self, channel: &str, since_s: Option<f64>) -> PyResult<(Vec<f64>, Vec<f64>)> {
        samples_of(&self.st, channel, since_s)
    }

    /// Iterates received messages of `main` until none arrives for
    /// `timeout_s`.
    #[pyo3(signature = (timeout_s=0.5))]
    fn messages(&self, timeout_s: f64) -> Messages {
        Messages {
            st: self.st.clone(),
            device: MAIN_DEVICE.into(),
            timeout_s,
        }
    }

    /// Every channel name, all devices.
    fn channels(&self) -> Vec<String> {
        self.st.lock().bench.telemetry().names()
    }

    /// Connected device names.
    #[getter]
    fn devices(&self) -> Vec<String> {
        self.st
            .lock()
            .bench
            .devices()
            .iter()
            .map(|d| d.name.clone())
            .collect()
    }

    /// Another device of the project, with the same methods.
    fn device(&self, name: &str) -> PyResult<Device> {
        if self.st.lock().bench.session(name).is_none() {
            return Err(PyKeyError::new_err(format!(
                "device `{name}` is not connected"
            )));
        }
        Ok(Device {
            st: self.st.clone(),
            name: name.to_string(),
        })
    }

    /// Connection state of `main`.
    #[getter]
    fn state(&self) -> String {
        state_label(&self.st, MAIN_DEVICE)
    }

    /// Runs a sequence of the project on the connected devices and returns
    /// the report (as `protobench run` writes it). `vars` override the
    /// sequence's `[vars]`.
    #[pyo3(signature = (name, vars=None, non_interactive=None))]
    fn run_sequence(
        &self,
        py: Python<'_>,
        name: &str,
        vars: Option<&Bound<'_, PyDict>>,
        non_interactive: Option<&str>,
    ) -> PyResult<Py<PyAny>> {
        let ni = match non_interactive {
            None => None,
            Some("fail") => Some(NonInteractive::Fail),
            Some("continue") => Some(NonInteractive::Continue),
            Some(o) => {
                return Err(PyValueError::new_err(format!(
                    "non_interactive must be \"fail\" or \"continue\", not {o:?}"
                )))
            }
        };
        let vars = kwargs_json(vars)?;
        let (seq, devices) = {
            let g = self.st.lock();
            let seq = SequenceFile::load(&g.project.sequence_path(name)).map_err(rt)?;
            let devices: Vec<(
                String,
                Option<Arc<CoreSession>>,
                pb_core::config::ProjectConfig,
            )> = g
                .project
                .all_devices()
                .iter()
                .map(|d| {
                    (
                        d.name.to_string(),
                        g.bench.session(d.name).cloned(),
                        d.project.config.clone(),
                    )
                })
                .collect();
            (seq, devices)
        };
        let report = py.detach(move || {
            let targets: Vec<Target> = devices
                .iter()
                .map(|(n, s, c)| Target {
                    name: n,
                    session: s.as_deref(),
                    config: c,
                })
                .collect();
            let opts = RunOptions {
                non_interactive: ni,
                vars,
                ..Default::default()
            };
            let mut ev = |_e: pb_core::sequence::RunEvent| {};
            run_sequence_on(&targets, &seq, opts, &mut ev, None)
        });
        let j = serde_json::to_value(&report).map_err(rt)?;
        Ok(to_py(py, &j)?.unbind())
    }

    /// Stops every device.
    fn close(&self, py: Python<'_>) {
        let st = self.st.clone();
        py.detach(move || st.lock().bench.stop_all());
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, py: Python<'_>, _args: &Bound<'_, PyTuple>) -> bool {
        self.close(py);
        false
    }

    fn __repr__(&self) -> String {
        let g = self.st.lock();
        format!(
            "<protobench.Session {:?}: {}>",
            g.project.config.name,
            g.bench
                .devices()
                .iter()
                .map(|d| format!("{} {}", d.name, d.session.state().label()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// One device of a session. Channel names are the device's own
/// (`reading.vout_mv` for `ref.reading.vout_mv`).
#[pyclass(module = "protobench", name = "Device", frozen)]
struct Device {
    st: Shared,
    name: String,
}

impl Device {
    fn full(&self, ch: &str) -> String {
        format!("{}{ch}", channel_prefix(&self.name))
    }
}

#[pymethods]
impl Device {
    /// Device name.
    #[getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    /// Sends one of the device's commands.
    #[pyo3(signature = (command, **fields))]
    fn send(
        &self,
        py: Python<'_>,
        command: &str,
        fields: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        send_command(py, &self.st, &self.name, command, kwargs_json(fields)?)
    }

    /// Sends any message of the device's schema.
    #[pyo3(signature = (message, payload=None, expect=None, timeout_ms=None))]
    fn send_msg(
        &self,
        py: Python<'_>,
        message: &str,
        payload: Option<&Bound<'_, PyAny>>,
        expect: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> PyResult<Py<PyAny>> {
        let p = payload
            .map(from_py)
            .transpose()?
            .unwrap_or(Json::Object(Default::default()));
        send_message(py, &self.st, &self.name, message, p, expect, timeout_ms)
    }

    /// Waits for a message from this device.
    #[pyo3(signature = (message, timeout_s=1.0))]
    fn wait(&self, py: Python<'_>, message: &str, timeout_s: f64) -> PyResult<Py<PyAny>> {
        wait_for(py, &self.st, &self.name, message, timeout_s)
    }

    /// Newest value of one of this device's channels.
    fn last(&self, channel: &str) -> Option<f64> {
        last_of(&self.st, &self.full(channel))
    }

    /// Statistics of one of this device's channels.
    fn stats(&self, py: Python<'_>, channel: &str) -> PyResult<Py<PyAny>> {
        stats_of(py, &self.st, &self.full(channel))
    }

    /// (times, values) of one of this device's channels.
    #[pyo3(signature = (channel, since_s=None))]
    fn samples(&self, channel: &str, since_s: Option<f64>) -> PyResult<(Vec<f64>, Vec<f64>)> {
        samples_of(&self.st, &self.full(channel), since_s)
    }

    /// Iterates this device's received messages.
    #[pyo3(signature = (timeout_s=0.5))]
    fn messages(&self, timeout_s: f64) -> Messages {
        Messages {
            st: self.st.clone(),
            device: self.name.clone(),
            timeout_s,
        }
    }

    /// Connection state.
    #[getter]
    fn state(&self) -> String {
        state_label(&self.st, &self.name)
    }

    fn __repr__(&self) -> String {
        format!("<protobench.Device {} {}>", self.name, self.state())
    }
}

/// Received messages: each a dict {t, type, name, msg, device}.
#[pyclass(module = "protobench", name = "Messages", frozen)]
struct Messages {
    st: Shared,
    device: String,
    timeout_s: f64,
}

#[pymethods]
impl Messages {
    fn __iter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        let st = self.st.clone();
        let dev = self.device.clone();
        let limit = Duration::from_secs_f64(self.timeout_s.max(0.0));
        let next = py.detach(move || {
            let end = Instant::now() + limit;
            loop {
                {
                    let mut g = st.lock();
                    if let Some(inbox) = g.inboxes.get_mut(&dev) {
                        inbox.drain();
                        if let Some(m) = inbox.msgs.pop_front() {
                            return Some(serde_json::json!({
                                "t": m.host_ts_ns as f64 / 1e9,
                                "type": m.type_name,
                                "name": m.kind(),
                                "msg": m.to_json(),
                                "device": dev,
                            }));
                        }
                    }
                }
                if Instant::now() >= end {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        next.map(|j| to_py(py, &j).map(Bound::unbind)).transpose()
    }
}

/// Decodes a recording: one dict per record.
#[pyfunction]
fn decode(py: Python<'_>, file: PathBuf) -> PyResult<Py<PyAny>> {
    let off = pb_core::recorder::OfflineSession::open(&file).map_err(rt)?;
    let all: Vec<Json> = off.decoded().iter().map(|r| r.to_json()).collect();
    Ok(to_py(py, &Json::Array(all))?.unbind())
}

fn recording_store(file: &std::path::Path) -> PyResult<pb_core::telemetry::TelemetryStore> {
    pb_core::recorder::OfflineSession::open(file)
        .and_then(|o| o.telemetry(usize::MAX / 4))
        .map_err(rt)
}

/// Exports a recording's channels to CSV (joined, or one file per channel).
/// Returns the files written.
#[pyfunction]
#[pyo3(signature = (file, out, channels=None, range=None, per_channel=false))]
fn export_csv(
    py: Python<'_>,
    file: PathBuf,
    out: PathBuf,
    channels: Option<Vec<String>>,
    range: Option<(f64, f64)>,
    per_channel: bool,
) -> PyResult<Vec<PathBuf>> {
    py.detach(move || {
        let store = recording_store(&file)?;
        let mode = if per_channel {
            pb_core::recorder::CsvMode::PerChannel
        } else {
            pb_core::recorder::CsvMode::Joined
        };
        pb_core::recorder::export_csv(&store, &channels.unwrap_or_default(), range, mode, &out)
            .map_err(rt)
    })
}

/// Exports a recording's channels to Parquet. Returns the row count.
#[pyfunction]
#[pyo3(signature = (file, out, channels=None, range=None))]
fn export_parquet(
    py: Python<'_>,
    file: PathBuf,
    out: PathBuf,
    channels: Option<Vec<String>>,
    range: Option<(f64, f64)>,
) -> PyResult<usize> {
    py.detach(move || {
        let store = recording_store(&file)?;
        pb_core::recorder::export_parquet(&store, &channels.unwrap_or_default(), range, &out)
            .map_err(rt)
    })
}

/// The `protobench` module.
#[pymodule]
fn protobench(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Project>()?;
    m.add_class::<Session>()?;
    m.add_class::<Device>()?;
    m.add_class::<Messages>()?;
    m.add_function(wrap_pyfunction!(decode, m)?)?;
    m.add_function(wrap_pyfunction!(export_csv, m)?)?;
    m.add_function(wrap_pyfunction!(export_parquet, m)?)?;
    m.add("__version__", pb_core::VERSION)?;
    Ok(())
}
