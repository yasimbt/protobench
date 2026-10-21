//! Rhai scripts for the `script` sequence step.
//!
//! Scripts run sandboxed: no module imports, no `eval`, no file or network
//! access, an operation limit and a wall-clock limit. They talk to the
//! device and the run through a small API:
//!
//! | Function | Does |
//! |---|---|
//! | `get_var(name)` | a sequence variable, or `()` (`var` is a Rhai keyword) |
//! | `set_var(name, value)` | sets a sequence variable |
//! | `send(command)`, `send(command, #{field: value})` | sends a project command (with payload overrides) |
//! | `send_msg(message, #{...})`, `send_msg(message, #{...}, expect)` | sends any message, optionally waiting for a reply |
//! | `last(channel)` | the newest value of a channel, or `()` |
//! | `stats(channel)`, `stats(channel, ms)` | statistics over all samples or the last `ms` |
//! | `log(text)`, `print(text)` | a line in the run log |
//! | `check(cond, message)` | records an assertion; a false one fails the step |
//! | `fail(message)` | stops the script and fails the step |
//! | `sleep(ms)` | waits, honouring abort and the time limit |
//!
//! `send` and `send_msg` return `#{status, response, latency_ms, attempts,
//! error}` with `status` one of `"ok"`, `"sent"`, `"timeout"`, `"error"`.
//! A script that throws (or calls `fail`) fails the step; any other runtime
//! problem (a type error, a limit, an abort) is a step error.
//!
//! With several devices, `send("ref:Ping")` and `send_msg("ref:set_rate",
//! …)` address device `ref`, and channels have their full names
//! (`last("ref.reading.vout_mv")`). `device("ref")` returns a handle with
//! the same `send`, `send_msg`, `last` and `stats`, where channel names are
//! the device's own (`device("ref").last("reading.vout_mv")`).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rhai::{Dynamic, Engine, EvalAltResult, Map, Position, Scope};
use serde_json::Value as Json;

use super::assert::AssertionResult;
use crate::commands::{build_command, build_inline, SendRequest};
use crate::config::ProjectConfig;
use crate::session::{RequestOutcome, SessionHandle};
use crate::telemetry::stats_of;

/// Most operations a script may perform.
pub const MAX_OPERATIONS: u64 = 50_000_000;

/// A device a script can talk to.
#[derive(Clone)]
pub struct ScriptDevice {
    /// Device name (`main` for the project's own).
    pub name: String,
    /// Its session.
    pub session: SessionHandle,
    /// Its project configuration (commands).
    pub config: Arc<ProjectConfig>,
}

/// What a script needs from the run.
pub struct ScriptEnv {
    /// The connected devices, `main` first.
    pub devices: Vec<ScriptDevice>,
    /// Sequence variables before the script.
    pub vars: BTreeMap<String, Json>,
    /// Set to abort the run.
    pub abort: Arc<AtomicBool>,
    /// Wall-clock limit.
    pub timeout: Duration,
}

/// What a script did.
#[derive(Debug, Default)]
pub struct ScriptOutcome {
    /// Sequence variables after the script.
    pub vars: BTreeMap<String, Json>,
    /// Log lines, in order.
    pub log: Vec<String>,
    /// `check()` results.
    pub checks: Vec<AssertionResult>,
    /// The script's final value, unless it is `()`.
    pub result: Option<Json>,
    /// The script threw or called `fail`: the step fails.
    pub failure: Option<String>,
    /// A compile error, runtime error, limit or abort: the step errors.
    pub error: Option<String>,
}

#[derive(Default)]
struct State {
    vars: BTreeMap<String, Json>,
    log: Vec<String>,
    checks: Vec<AssertionResult>,
}

type Shared = Arc<Mutex<State>>;

/// An engine with the sandbox limits and no API functions.
fn sandbox() -> Engine {
    let mut e = Engine::new();
    e.set_max_operations(MAX_OPERATIONS);
    e.set_max_call_levels(64);
    e.set_max_expr_depths(64, 32);
    e.set_max_string_size(1 << 20);
    e.set_max_array_size(100_000);
    e.set_max_map_size(10_000);
    e.set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());
    e.disable_symbol("eval");
    e
}

/// Compiles a script without running it. The error carries the 1-based line
/// within the script, when known.
pub fn compile_check(code: &str) -> Result<(), (String, Option<usize>)> {
    sandbox()
        .compile(code)
        .map(|_| ())
        .map_err(|e| (e.err_type().to_string(), e.position().line()))
}

fn to_dyn(j: &Json) -> Dynamic {
    rhai::serde::to_dynamic(j).unwrap_or(Dynamic::UNIT)
}

fn from_dyn(d: &Dynamic) -> Json {
    rhai::serde::from_dynamic::<Json>(d).unwrap_or(Json::Null)
}

fn outcome_map(out: Result<RequestOutcome, String>) -> Map {
    let mut m = Map::new();
    let mut put = |k: &str, v: Dynamic| {
        m.insert(k.into(), v);
    };
    match out {
        Ok(RequestOutcome::Sent) => put("status", "sent".into()),
        Ok(RequestOutcome::Ok {
            response,
            latency_ns,
            attempts,
        }) => {
            put("status", "ok".into());
            put("response", to_dyn(&response.to_json()));
            put("latency_ms", Dynamic::from_float(latency_ns as f64 / 1e6));
            put("attempts", Dynamic::from_int(i64::from(attempts)));
        }
        Ok(RequestOutcome::Timeout { attempts }) => {
            put("status", "timeout".into());
            put("attempts", Dynamic::from_int(i64::from(attempts)));
        }
        Ok(RequestOutcome::Error(e)) | Err(e) => {
            put("status", "error".into());
            put("error", e.into());
        }
    }
    m
}

/// The script's view of the devices.
#[derive(Clone)]
struct Api {
    devices: Arc<Vec<ScriptDevice>>,
    state: Shared,
}

impl Api {
    /// `"ref:Ping"` → (index of `ref`, `"Ping"`) when `ref` is a connected
    /// device; anything else is `main`'s (the first device).
    fn split<'s>(&self, s: &'s str) -> (usize, &'s str) {
        if let Some((dev, rest)) = s.split_once(':') {
            if let Some(i) = self.devices.iter().position(|d| d.name == dev) {
                return (i, rest);
            }
        }
        (0, s)
    }

    fn send_req(&self, d: usize, r: Result<SendRequest, String>) -> Map {
        let Some(dev) = self.devices.get(d) else {
            return outcome_map(Err("no device is connected".into()));
        };
        outcome_map(r.map(|req| dev.session.send_and_wait(req)))
    }

    fn send_cmd(&self, d: usize, name: &str, overrides: &Map) -> Map {
        let built = (|| {
            let dev = self.devices.get(d).ok_or("no device is connected")?;
            let cmd = dev.config.command(name).ok_or_else(|| {
                if dev.name == crate::config::MAIN_DEVICE {
                    format!("no command `{name}` in project.toml")
                } else {
                    format!("no command `{name}` on device `{}`", dev.name)
                }
            })?;
            let ov: Vec<(String, Json)> = overrides
                .iter()
                .map(|(k, v)| (k.to_string(), from_dyn(v)))
                .collect();
            let vars = self.state.lock().vars.clone();
            build_command(&dev.config, dev.session.codec(), cmd, &ov, &vars)
                .map_err(|e| e.to_string())
        })();
        self.send_req(d, built)
    }

    fn send_msg(&self, d: usize, msg: &str, payload: &Map, expect: Option<&str>) -> Map {
        let built = (|| {
            let dev = self.devices.get(d).ok_or("no device is connected")?;
            build_inline(
                &dev.config,
                dev.session.codec(),
                msg,
                from_dyn(&Dynamic::from_map(payload.clone())),
                expect,
                None,
                None,
            )
            .map_err(|e| e.to_string())
        })();
        self.send_req(d, built)
    }

    /// The telemetry store all devices share.
    fn store(&self) -> Option<Arc<crate::telemetry::TelemetryStore>> {
        self.devices.first().map(|d| d.session.telemetry().clone())
    }

    fn last(&self, ch: &str) -> Dynamic {
        self.store()
            .and_then(|s| s.get(ch))
            .and_then(|c| c.last())
            .map(|(_, v)| Dynamic::from_float(v))
            .unwrap_or(Dynamic::UNIT)
    }

    fn stats(&self, ch: &str, ms: Option<i64>) -> Dynamic {
        let Some(c) = self.store().and_then(|s| s.get(ch)) else {
            return Dynamic::UNIT;
        };
        let Some(ms) = ms else {
            return to_dyn(&serde_json::to_value(c.stats()).unwrap_or(Json::Null));
        };
        let data = c.data.read();
        let Some((t_last, _)) = data.ring.last() else {
            return Dynamic::UNIT;
        };
        let window = ms.max(0) as f64 / 1000.0;
        let samples: Vec<(f64, f64)> = data.ring.range(t_last - window, f64::INFINITY).collect();
        to_dyn(&serde_json::to_value(stats_of(&samples, Some(window))).unwrap_or(Json::Null))
    }
}

/// `device("ref")` in a script.
#[derive(Clone)]
struct DeviceApi {
    api: Api,
    index: usize,
}

impl DeviceApi {
    /// A channel of this device by its own name.
    fn channel(&self, ch: &str) -> String {
        format!(
            "{}{ch}",
            crate::config::channel_prefix(&self.api.devices[self.index].name)
        )
    }
}

/// Runs a script to completion.
pub fn run_script(code: &str, env: ScriptEnv) -> ScriptOutcome {
    let state: Shared = Arc::new(Mutex::new(State {
        vars: env.vars,
        ..Default::default()
    }));
    let deadline = Instant::now() + env.timeout;
    let mut engine = sandbox();

    {
        let abort = env.abort.clone();
        engine.on_progress(move |_| {
            if abort.load(Ordering::Relaxed) {
                Some("aborted".into())
            } else if Instant::now() >= deadline {
                Some("time limit".into())
            } else {
                None
            }
        });
    }
    {
        let st = state.clone();
        engine.on_print(move |s| st.lock().log.push(s.to_string()));
        let st = state.clone();
        engine.on_debug(move |s, _, _| st.lock().log.push(s.to_string()));
    }

    // Variables.
    let st = state.clone();
    engine.register_fn("get_var", move |name: &str| -> Dynamic {
        st.lock()
            .vars
            .get(name)
            .map(to_dyn)
            .unwrap_or(Dynamic::UNIT)
    });
    let st = state.clone();
    engine.register_fn("set_var", move |name: &str, v: Dynamic| {
        st.lock().vars.insert(name.to_string(), from_dyn(&v));
    });

    // Devices, sending and telemetry.
    let api = Api {
        devices: Arc::new(env.devices),
        state: state.clone(),
    };
    {
        let a = api.clone();
        engine.register_fn("send", move |name: &str| -> Map {
            let (d, rest) = a.split(name);
            a.send_cmd(d, rest, &Map::new())
        });
        let a = api.clone();
        engine.register_fn("send", move |name: &str, ov: Map| -> Map {
            let (d, rest) = a.split(name);
            a.send_cmd(d, rest, &ov)
        });
        let a = api.clone();
        engine.register_fn("send_msg", move |msg: &str, p: Map| -> Map {
            let (d, rest) = a.split(msg);
            a.send_msg(d, rest, &p, None)
        });
        let a = api.clone();
        engine.register_fn("send_msg", move |msg: &str, p: Map, expect: &str| -> Map {
            let (d, rest) = a.split(msg);
            a.send_msg(d, rest, &p, Some(expect))
        });
        let a = api.clone();
        engine.register_fn("last", move |ch: &str| a.last(ch));
        let a = api.clone();
        engine.register_fn("stats", move |ch: &str| a.stats(ch, None));
        let a = api.clone();
        engine.register_fn("stats", move |ch: &str, ms: i64| a.stats(ch, Some(ms)));
        let a = api.clone();
        engine.register_type_with_name::<DeviceApi>("Device");
        engine.register_fn(
            "device",
            move |name: &str| -> Result<DeviceApi, Box<EvalAltResult>> {
                match a.devices.iter().position(|d| d.name == name) {
                    Some(index) => Ok(DeviceApi {
                        api: a.clone(),
                        index,
                    }),
                    None => Err(Box::new(EvalAltResult::ErrorRuntime(
                        format!(
                            "no connected device `{name}` (connected: {})",
                            a.devices
                                .iter()
                                .map(|d| d.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                        .into(),
                        Position::NONE,
                    ))),
                }
            },
        );
        engine.register_get("name", |d: &mut DeviceApi| -> String {
            d.api.devices[d.index].name.clone()
        });
        engine.register_fn("send", |d: &mut DeviceApi, name: &str| -> Map {
            d.api.send_cmd(d.index, name, &Map::new())
        });
        engine.register_fn("send", |d: &mut DeviceApi, name: &str, ov: Map| -> Map {
            d.api.send_cmd(d.index, name, &ov)
        });
        engine.register_fn("send_msg", |d: &mut DeviceApi, msg: &str, p: Map| -> Map {
            d.api.send_msg(d.index, msg, &p, None)
        });
        engine.register_fn(
            "send_msg",
            |d: &mut DeviceApi, msg: &str, p: Map, expect: &str| -> Map {
                d.api.send_msg(d.index, msg, &p, Some(expect))
            },
        );
        engine.register_fn("last", |d: &mut DeviceApi, ch: &str| {
            d.api.last(&d.channel(ch))
        });
        engine.register_fn("stats", |d: &mut DeviceApi, ch: &str| {
            d.api.stats(&d.channel(ch), None)
        });
        engine.register_fn("stats", |d: &mut DeviceApi, ch: &str, ms: i64| {
            d.api.stats(&d.channel(ch), Some(ms))
        });
    }

    // Run log and results.
    let st = state.clone();
    engine.register_fn("log", move |s: &str| st.lock().log.push(s.to_string()));
    let st = state.clone();
    engine.register_fn("check", move |cond: bool, msg: &str| {
        st.lock().checks.push(AssertionResult {
            description: msg.to_string(),
            measured: Json::Bool(cond),
            passed: cond,
            message: (!cond).then(|| format!("check failed: {msg}")),
        });
    });
    engine.register_fn("fail", |msg: &str| -> Result<(), Box<EvalAltResult>> {
        Err(Box::new(EvalAltResult::ErrorRuntime(
            msg.to_string().into(),
            Position::NONE,
        )))
    });
    {
        let abort = env.abort.clone();
        engine.register_fn("sleep", move |ms: i64| -> Result<(), Box<EvalAltResult>> {
            let end = Instant::now() + Duration::from_millis(ms.max(0) as u64);
            loop {
                if abort.load(Ordering::Relaxed) {
                    return Err(Box::new(EvalAltResult::ErrorTerminated(
                        "aborted".into(),
                        Position::NONE,
                    )));
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(Box::new(EvalAltResult::ErrorTerminated(
                        "time limit".into(),
                        Position::NONE,
                    )));
                }
                if now >= end {
                    return Ok(());
                }
                std::thread::sleep(
                    (end - now)
                        .min(deadline - now)
                        .min(Duration::from_millis(10)),
                );
            }
        });
    }

    let mut out = ScriptOutcome::default();
    match engine.compile(code) {
        Err(e) => {
            out.error = Some(match e.position().line() {
                Some(l) => format!("script line {l}: {}", e.err_type()),
                None => e.err_type().to_string(),
            });
        }
        Ok(ast) => {
            let mut scope = Scope::new();
            match engine.eval_ast_with_scope::<Dynamic>(&mut scope, &ast) {
                Ok(v) if v.is_unit() => {}
                Ok(v) => out.result = Some(from_dyn(&v)),
                Err(e) => {
                    let at = e.position().line().map(|l| format!("script line {l}: "));
                    let at = at.unwrap_or_default();
                    match *e {
                        EvalAltResult::ErrorRuntime(v, _) => {
                            let msg = v.into_string().unwrap_or_else(|v| v.to_string());
                            out.failure = Some(format!("{at}{msg}"));
                        }
                        EvalAltResult::ErrorTerminated(why, _) => {
                            out.error = Some(format!("{at}script stopped: {why}"));
                        }
                        EvalAltResult::ErrorTooManyOperations(_) => {
                            out.error =
                                Some(format!("{at}script exceeded {MAX_OPERATIONS} operations"));
                        }
                        other => out.error = Some(format!("{at}{other}")),
                    }
                }
            }
        }
    }
    drop(engine);
    let st = std::mem::take(&mut *state.lock());
    out.vars = st.vars;
    out.log = st.log;
    out.checks = st.checks;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use crate::error::CoreError;
    use crate::project::Project;
    use crate::session::{Session, SessionParams};
    use crate::transport::Connector;

    /// A session that never connects: sends fail with "not connected".
    fn offline() -> (Session, Arc<ProjectConfig>) {
        let p = Project::open(&crate::schema::tests::demo_dir());
        let profile = p.config.profile("sim").unwrap().clone();
        let mut params = SessionParams::new(&p, &profile).unwrap();
        params.auto_reconnect = false;
        let c: Connector = Box::new(|| Err(CoreError::Transport("no device".into())));
        let s = Session::start(params, c, SystemClock::shared()).unwrap();
        s.wait_connected(Duration::from_secs(1));
        (s, Arc::new(p.config.clone()))
    }

    fn run(code: &str, timeout_ms: u64, abort: Arc<AtomicBool>) -> ScriptOutcome {
        let (s, config) = offline();
        let mut vars = BTreeMap::new();
        vars.insert("bias_mv".to_string(), Json::from(200));
        run_script(
            code,
            ScriptEnv {
                devices: vec![ScriptDevice {
                    name: "main".into(),
                    session: s.handle(),
                    config,
                }],
                vars,
                abort,
                timeout: Duration::from_millis(timeout_ms),
            },
        )
    }

    fn quick(code: &str) -> ScriptOutcome {
        run(code, 5_000, Arc::new(AtomicBool::new(false)))
    }

    #[test]
    fn vars_log_checks_and_result() {
        let r = quick(
            r#"
            let b = get_var("bias_mv");
            set_var("double", b * 2);
            set_var("cfg", #{ name: "x", gains: [1, 2.5] });
            log(`bias is ${b}`);
            print("from print");
            check(b == 200, "bias is 200");
            check(get_var("missing") == (), "missing is unit");
            check(false, "this one fails");
            #{ ok: true, n: 3 }
            "#,
        );
        assert!(r.error.is_none() && r.failure.is_none(), "{r:?}");
        assert_eq!(r.vars["double"], Json::from(400));
        assert_eq!(
            r.vars["cfg"],
            serde_json::json!({"name": "x", "gains": [1, 2.5]})
        );
        assert_eq!(
            r.log,
            vec!["bias is 200".to_string(), "from print".to_string()]
        );
        assert_eq!(r.checks.len(), 3);
        assert!(r.checks[0].passed && r.checks[1].passed && !r.checks[2].passed);
        assert_eq!(r.result, Some(serde_json::json!({"ok": true, "n": 3})));
    }

    #[test]
    fn send_reports_errors_as_status() {
        let r = quick(
            r#"
            let a = send("Ping");
            let b = send("No Such Command");
            let c = send_msg("set_bias", #{ millivolts: 5 }, "ack");
            [a.status, a.error, b.status, b.error, c.status]
            "#,
        );
        assert!(r.error.is_none(), "{r:?}");
        let v = r.result.unwrap();
        assert_eq!(v[0], "error");
        assert_eq!(v[1], "not connected");
        assert_eq!(v[2], "error");
        assert!(v[3]
            .as_str()
            .unwrap()
            .contains("no command `No Such Command`"));
        assert_eq!(v[4], "error");
    }

    #[test]
    fn fail_and_throw_fail_the_step() {
        let r = quick(r#"log("before"); fail("bad gain"); log("after");"#);
        assert_eq!(r.failure.as_deref(), Some("script line 1: bad gain"));
        assert_eq!(r.log, vec!["before".to_string()]);
        let r = quick("throw \"oops\";");
        assert!(r.failure.unwrap().contains("oops"));
    }

    #[test]
    fn sandbox_limits() {
        // Endless loop: stopped by the operation limit or the time limit.
        let r = run("loop { }", 2_000, Arc::new(AtomicBool::new(false)));
        let e = r.error.unwrap();
        assert!(e.contains("operations") || e.contains("time limit"), "{e}");
        // Time limit while sleeping.
        let t = Instant::now();
        let r = run("sleep(10000);", 100, Arc::new(AtomicBool::new(false)));
        assert!(r.error.unwrap().contains("time limit"));
        assert!(t.elapsed() < Duration::from_secs(2));
        // Abort.
        let abort = Arc::new(AtomicBool::new(true));
        let r = run("sleep(10000);", 10_000, abort);
        assert!(r.error.unwrap().contains("aborted"));
        // No imports, no eval.
        let r = quick(r#"import "secrets" as s;"#);
        assert!(r.error.is_some(), "{r:?}");
        let r = quick(r#"eval("40 + 2")"#);
        assert!(r.error.is_some(), "{r:?}");
        // Compile errors carry the line.
        let r = quick("let a = 1;\nlet b = ;");
        assert!(r.error.unwrap().starts_with("script line 2:"));
        assert!(compile_check("let a = 1;").is_ok());
        assert_eq!(compile_check("\n\nlet = 3;").unwrap_err().1, Some(3));
    }
}
