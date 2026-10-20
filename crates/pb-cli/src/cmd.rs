//! Subcommand implementations.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use prost_reflect::{Kind, MessageDescriptor};

use pb_core::clock::SystemClock;
use pb_core::commands::{build_command, parse_value};
use pb_core::project::Project;
use pb_core::schema::Schema;
use pb_core::session::{
    ConnState, Direction, RequestOutcome, Session, SessionEvent, SessionParams,
};

use crate::NonInteractive;

/// Opens a project and fails (exit 2) if it has errors.
fn open_project(dir: &Path) -> Result<Project> {
    let p = Project::open(dir);
    for d in &p.diagnostics {
        eprintln!("{d}");
    }
    if p.has_errors() {
        bail!("project {} has errors", dir.display());
    }
    Ok(p)
}

/// Connects to a project's device with the chosen profile.
pub fn connect(p: &Project, profile: Option<&str>, port: Option<String>) -> Result<Session> {
    let prof = match profile {
        Some(name) => p
            .config
            .profile(name)
            .ok_or_else(|| anyhow!("no connection profile `{name}`"))?,
        None => p
            .config
            .default_profile()
            .ok_or_else(|| anyhow!("project has no connection profiles"))?,
    }
    .clone();
    let sim = if prof.transport == pb_core::config::TransportKind::Sim {
        Some(pb_sim::sim_factory(p)?)
    } else {
        None
    };
    let connector = pb_core::transport::connector(&prof, &p.dir, &p.config.framing, sim, port)?;
    let mut params = SessionParams::new(p, &prof)?;
    // The CLI fails fast instead of retrying forever on the first connect.
    let auto = params.auto_reconnect;
    params.auto_reconnect = false;
    let session = Session::start(params.clone(), connector, SystemClock::shared())?;
    match session.wait_connected(Duration::from_secs(5)) {
        ConnState::Connected => {}
        ConnState::Failed(e) => bail!("cannot connect ({}): {e}", prof.endpoint()),
        other => bail!("cannot connect ({}): {}", prof.endpoint(), other.label()),
    }
    let _ = auto;
    Ok(session)
}

pub fn list_ports(json: bool) -> Result<ExitCode> {
    let ports = pb_core::transport::list_ports();
    if json {
        println!("{}", serde_json::to_string_pretty(&ports)?);
    } else if ports.is_empty() {
        println!("no serial ports found");
    } else {
        for p in ports {
            let ids = match (p.vid, p.pid) {
                (Some(v), Some(pid)) => format!("{v:04x}:{pid:04x}"),
                _ => "----:----".into(),
            };
            println!(
                "{:<24} {ids}  {}",
                p.name,
                p.description.unwrap_or_default()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn kind_name(f: &prost_reflect::FieldDescriptor) -> String {
    let base = match f.kind() {
        Kind::Message(m) if f.is_map() => {
            let k = m.map_entry_key_field();
            let v = m.map_entry_value_field();
            return format!("map<{}, {}>", kind_str(&k.kind()), kind_str(&v.kind()));
        }
        k => kind_str(&k),
    };
    if f.is_list() {
        format!("repeated {base}")
    } else {
        base
    }
}

fn kind_str(k: &Kind) -> String {
    match k {
        Kind::Message(m) => m.full_name().to_string(),
        Kind::Enum(e) => e.full_name().to_string(),
        other => format!("{other:?}").to_lowercase(),
    }
}

fn print_message(schema: &Schema, m: &MessageDescriptor) {
    println!("message {} {{", m.full_name());
    if let Some(c) = schema.message_comment(m) {
        for l in c.lines() {
            println!("  // {l}");
        }
    }
    for f in m.fields() {
        let oneof = f
            .containing_oneof()
            .filter(|o| !o.is_synthetic())
            .map(|o| format!("  (oneof {})", o.name()))
            .unwrap_or_default();
        let comment = schema
            .field_comment(&f)
            .map(|c| format!("  // {}", c.replace('\n', " ")))
            .unwrap_or_default();
        println!(
            "  {} {} = {};{oneof}{comment}",
            kind_name(&f),
            f.name(),
            f.number()
        );
    }
    println!("}}");
}

pub fn schema(dir: &Path, message: Option<&str>) -> Result<ExitCode> {
    let p = open_project(dir)?;
    let schema = p.require_schema()?;
    match message {
        Some(name) => {
            let m = schema
                .message(name)
                .or_else(|| {
                    schema
                        .user_messages()
                        .into_iter()
                        .find(|m| m.name() == name)
                })
                .ok_or_else(|| anyhow!("no message `{name}` in the schema"))?;
            print_message(&schema, &m);
            for f in m.fields() {
                if let Kind::Enum(e) = f.kind() {
                    print_enum(&schema, &e);
                }
            }
        }
        None => {
            for m in schema.user_messages() {
                print_message(&schema, &m);
                println!();
            }
            let mut enums: Vec<_> = schema
                .pool()
                .all_enums()
                .filter(|e| !pb_core::schema::is_support_file(e.parent_file().name()))
                .collect();
            enums.sort_by(|a, b| a.full_name().cmp(b.full_name()));
            for e in enums {
                print_enum(&schema, &e);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_enum(schema: &Schema, e: &prost_reflect::EnumDescriptor) {
    println!("enum {} {{", e.full_name());
    for v in e.values() {
        let c = schema
            .enum_value_comment(&v)
            .map(|c| format!("  // {}", c.replace('\n', " ")))
            .unwrap_or_default();
        println!("  {} = {};{c}", v.name(), v.number());
    }
    println!("}}");
}

fn parse_sets(sets: &[String]) -> Result<Vec<(String, serde_json::Value)>> {
    sets.iter()
        .map(|s| {
            let (k, v) = s
                .split_once('=')
                .ok_or_else(|| anyhow!("--set expects FIELD=VALUE, got `{s}`"))?;
            Ok((k.trim().to_string(), parse_value(v.trim())))
        })
        .collect()
}

pub fn send(
    dir: &Path,
    profile: Option<&str>,
    port: Option<String>,
    command: &str,
    set: &[String],
    wait: bool,
) -> Result<ExitCode> {
    let p = open_project(dir)?;
    let cmd = p
        .config
        .command(command)
        .ok_or_else(|| anyhow!("no command `{command}` in project.toml"))?
        .clone();
    let overrides = parse_sets(set)?;
    let session = connect(&p, profile, port)?;
    let mut req = build_command(
        &p.config,
        session.codec(),
        &cmd,
        &overrides,
        &Default::default(),
    )?;
    if !wait {
        req.expect = None;
    }
    let out = session.send_and_wait(req);
    match &out {
        RequestOutcome::Sent => println!("sent {command}"),
        RequestOutcome::Ok {
            response,
            latency_ns,
            attempts,
        } => {
            println!("{}", serde_json::to_string_pretty(&response.to_json())?);
            eprintln!(
                "ok: {} in {:.1} ms ({} attempt{})",
                response.kind(),
                *latency_ns as f64 / 1e6,
                attempts,
                if *attempts == 1 { "" } else { "s" }
            );
        }
        RequestOutcome::Timeout { attempts } => {
            eprintln!("timeout after {attempts} attempt(s)");
            return Ok(ExitCode::from(1));
        }
        RequestOutcome::Error(e) => {
            eprintln!("error: {e}");
            return Ok(ExitCode::from(1));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Arguments of `monitor`.
pub struct MonitorArgs {
    pub project: PathBuf,
    pub profile: Option<String>,
    pub port: Option<String>,
    pub json: bool,
    pub channels: Vec<String>,
    pub send: Vec<String>,
    pub duration: Option<f64>,
    pub record: Option<PathBuf>,
}

pub fn monitor(a: MonitorArgs) -> Result<ExitCode> {
    let p = open_project(&a.project)?;
    let session = connect(&p, a.profile.as_deref(), a.port.clone())?;
    let events = session.subscribe();
    if let Some(path) = &a.record {
        let w = pb_core::recorder::SessionWriter::create(path, &p)?;
        session.start_recording(Box::new(w));
        eprintln!("recording to {}", path.display());
    }
    for name in &a.send {
        let cmd = p
            .config
            .command(name)
            .ok_or_else(|| anyhow!("no command `{name}`"))?;
        let req = build_command(&p.config, session.codec(), cmd, &[], &Default::default())?;
        let out = session.send_and_wait(req);
        eprintln!("{name}: {}", out.label());
    }
    let deadline = a
        .duration
        .map(|d| Instant::now() + Duration::from_secs_f64(d));
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut last: std::collections::HashMap<String, u64> = Default::default();
    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        let ev = match events.recv_timeout(Duration::from_millis(50)) {
            Ok(e) => e,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if !a.channels.is_empty() {
                    print_channels(&session, &a.channels, a.json, &mut last, &mut out)?;
                }
                if matches!(session.state(), ConnState::Failed(_) | ConnState::Finished) {
                    break;
                }
                continue;
            }
            Err(_) => break,
        };
        if !a.channels.is_empty() {
            continue;
        }
        match ev {
            SessionEvent::Message { dir, msg } => {
                let t = msg.host_ts_ns as f64 / 1e9;
                let d = match dir {
                    Direction::Rx => "rx",
                    Direction::Tx => "tx",
                };
                if a.json {
                    let line = serde_json::json!({"t": t, "dir": d, "type": msg.type_name, "kind": msg.kind(), "msg": msg.to_json()});
                    writeln!(out, "{line}")?;
                } else {
                    writeln!(out, "{t:10.4} {d} {:<12} {}", msg.kind(), msg.to_json())?;
                }
            }
            SessionEvent::Text { host_ts_ns, line } => {
                if a.json {
                    writeln!(
                        out,
                        "{}",
                        serde_json::json!({"t": host_ts_ns as f64 / 1e9, "text": line})
                    )?;
                } else {
                    writeln!(out, "{:10.4} log {line}", host_ts_ns as f64 / 1e9)?;
                }
            }
            SessionEvent::BadFrame(b) => {
                if !a.json {
                    writeln!(
                        out,
                        "{:10.4} bad {} {}",
                        b.host_ts_ns as f64 / 1e9,
                        b.kind,
                        b.reason
                    )?;
                }
            }
            SessionEvent::State(s) => eprintln!("{}", s.label()),
            _ => {}
        }
    }
    if let Some(mut w) = session.stop_recording() {
        w.finish()?;
    }
    Ok(ExitCode::SUCCESS)
}

fn print_channels(
    session: &Session,
    names: &[String],
    json: bool,
    last: &mut std::collections::HashMap<String, u64>,
    out: &mut impl Write,
) -> Result<()> {
    let mut row = serde_json::Map::new();
    let mut changed = false;
    for n in names {
        if let Some(ch) = session.telemetry().get(n) {
            let total = ch.total();
            if last.get(n) != Some(&total) {
                changed = true;
                last.insert(n.clone(), total);
            }
            if let Some((_, v)) = ch.last() {
                row.insert(n.clone(), serde_json::json!(v));
            }
        }
    }
    if changed {
        if json {
            writeln!(out, "{}", serde_json::Value::Object(row))?;
        } else {
            let parts: Vec<String> = row.iter().map(|(k, v)| format!("{k}={v}")).collect();
            writeln!(out, "{}", parts.join("  "))?;
        }
    }
    Ok(())
}

pub fn run(
    dir: &Path,
    sequence: &str,
    profile: Option<&str>,
    port: Option<String>,
    report_dir: Option<PathBuf>,
    non_interactive: Option<NonInteractive>,
) -> Result<ExitCode> {
    crate::cmd_run::run(dir, sequence, profile, port, report_dir, non_interactive)
}

pub fn decode(
    file: &Path,
    json: bool,
    csv: Option<PathBuf>,
    mcap: Option<PathBuf>,
) -> Result<ExitCode> {
    crate::cmd_decode::decode(file, json, csv, mcap)
}

pub fn sim(dir: &Path, bind: &str, port: u16) -> Result<ExitCode> {
    let p = open_project(dir)?;
    let stop = Arc::new(AtomicBool::new(false));
    pb_sim::run_tcp_server(&p, bind, port, stop, |bound| {
        eprintln!(
            "simulator for `{}` listening on {bind}:{bound} (Ctrl-C to stop)",
            p.config.name
        );
    })
    .with_context(|| format!("simulator on {bind}:{port}"))?;
    Ok(ExitCode::SUCCESS)
}
