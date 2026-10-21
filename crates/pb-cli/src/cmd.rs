//! Subcommand implementations.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use prost_reflect::{Kind, MessageDescriptor};

use pb_core::bench::Bench;
use pb_core::commands::{build_command, parse_value};
use pb_core::config::MAIN_DEVICE;
use pb_core::project::Project;
use pb_core::schema::Schema;
use pb_core::session::{ConnState, Direction, RequestOutcome, SessionEvent};

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

/// Device options shared by `send`, `monitor` and `run`.
#[derive(Debug, Clone, Default)]
pub struct DeviceArgs {
    /// `--profile`: `NAME` for `main` (or the target of `send`), `DEVICE=NAME`.
    pub profiles: Vec<String>,
    /// `--port`: `PORT` for `main` (or the target of `send`), `DEVICE=PORT`.
    pub ports: Vec<String>,
    /// `--only`: devices to start (default: all).
    pub only: Vec<String>,
}

/// Splits `--profile`/`--port` values into per-device values. `DEVICE=VALUE`
/// names a device; a bare value is for `bare` (`main`, or the target of
/// `send`).
fn per_device(
    flag: &str,
    values: &[String],
    p: &Project,
    bare: &str,
) -> Result<std::collections::HashMap<String, String>> {
    let mut out = std::collections::HashMap::new();
    for v in values {
        let (dev, val) = match v.split_once('=') {
            Some((d, x)) if pb_core::config::is_device_name(d.trim()) => {
                let d = d.trim();
                if p.device(d).is_none() {
                    bail!(
                        "{flag} {v}: no device `{d}` (this project has {})",
                        device_list(p)
                    );
                }
                (d.to_string(), x.trim().to_string())
            }
            _ => (bare.to_string(), v.trim().to_string()),
        };
        if out.insert(dev.clone(), val).is_some() {
            bail!("{flag} given twice for device `{dev}`");
        }
    }
    Ok(out)
}

fn device_list(p: &Project) -> String {
    p.all_devices()
        .iter()
        .map(|d| format!("`{}`", d.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Starts devices of a project and waits until each is connected. With
/// `only` empty every device starts. `bare` is the device that bare
/// `--profile`/`--port` values belong to.
pub fn connect_devices(p: &Project, a: &DeviceArgs, only: &[&str], bare: &str) -> Result<Bench> {
    let profiles = per_device("--profile", &a.profiles, p, bare)?;
    let ports = per_device("--port", &a.ports, p, bare)?;
    for o in only {
        if p.device(o).is_none() {
            bail!(
                "--only {o}: no device `{o}` (this project has {})",
                device_list(p)
            );
        }
    }
    let mut bench = Bench::default();
    let mut started = Vec::new();
    for d in p.all_devices() {
        if !only.is_empty() && !only.contains(&d.name) {
            continue;
        }
        let what = if d.name == pb_core::config::MAIN_DEVICE {
            String::new()
        } else {
            format!("device `{}`: ", d.name)
        };
        let dev = bench
            .start(
                p,
                d.name,
                profiles.get(d.name).map(String::as_str),
                ports.get(d.name).cloned(),
                &|dp: &Project| pb_sim::sim_factory(dp),
                // The CLI fails fast instead of retrying forever.
                |params| params.auto_reconnect = false,
            )
            .map_err(|e| anyhow!("{what}{e}"))?;
        started.push((d.name.to_string(), what, dev.profile.endpoint()));
    }
    for (name, what, endpoint) in started {
        let session = bench.session(&name).expect("started").clone();
        let mut state = session.wait_connected(Duration::from_secs(5));
        if state == ConnState::Listening {
            // A server profile waits for its client as long as it takes.
            eprintln!("{what}waiting for a client on {} …", session.endpoint());
            while state == ConnState::Listening {
                state = session.wait_connected(Duration::from_secs(60));
            }
        }
        match state {
            ConnState::Connected => {}
            ConnState::Failed(e) => bail!("{what}cannot connect ({endpoint}): {e}"),
            other => bail!("{what}cannot connect ({endpoint}): {}", other.label()),
        }
    }
    Ok(bench)
}

pub fn list_ports(json: bool) -> Result<ExitCode> {
    let ports = pb_core::transport::list_ports();
    if json {
        println!("{}", serde_json::to_string_pretty(&ports)?);
    } else if ports.is_empty() {
        println!("no serial ports found");
    } else {
        println!(
            "{:<24} {:<9}  {:<24} DESCRIPTION",
            "PORT", "USB ID", "SERIAL"
        );
        for p in ports {
            println!(
                "{:<24} {}  {:<24} {}",
                p.name,
                p.usb_id(),
                p.serial_number.as_deref().unwrap_or("-"),
                p.description.as_deref().unwrap_or_default()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn ble_scan(seconds: u64, json: bool) -> Result<ExitCode> {
    let devices = pb_core::transport::ble::scan(seconds)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&devices)?);
    } else if devices.is_empty() {
        println!("no Bluetooth LE devices found in {seconds} s");
    } else {
        println!("{:<20} {:>5}  {:<24} SERVICES", "ADDRESS", "RSSI", "NAME");
        for d in devices {
            println!(
                "{:<20} {:>5}  {:<24} {}",
                d.address,
                d.rssi.map(|r| r.to_string()).unwrap_or_else(|| "-".into()),
                d.name.as_deref().unwrap_or("-"),
                d.services.join(" ")
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn list_probes(json: bool) -> Result<ExitCode> {
    use pb_core::transport::rtt;
    let probes = rtt::list_probes();
    if json {
        println!("{}", serde_json::to_string_pretty(&probes)?);
    } else if !rtt::SUPPORTED {
        println!("this build has no RTT support (cargo feature `rtt`)");
    } else if probes.is_empty() {
        println!("no debug probes found");
    } else {
        println!("{:<28} PROBE (use as `probe = \"…\"`)", "NAME");
        for p in probes {
            println!("{:<28} {}", p.identifier, p.selector());
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

/// Parses `--set` / `--var` arguments (`NAME=VALUE`, value in TOML syntax).
pub fn parse_sets(flag: &str, sets: &[String]) -> Result<Vec<(String, serde_json::Value)>> {
    sets.iter()
        .map(|s| {
            let (k, v) = s
                .split_once('=')
                .ok_or_else(|| anyhow!("{flag} expects NAME=VALUE, got `{s}`"))?;
            Ok((k.trim().to_string(), parse_value(v.trim())))
        })
        .collect()
}

pub fn send(
    dir: &Path,
    device: &str,
    devices: &DeviceArgs,
    command: &str,
    set: &[String],
    wait: bool,
) -> Result<ExitCode> {
    let p = open_project(dir)?;
    let d = p.device(device).ok_or_else(|| {
        anyhow!(
            "--device {device}: no device `{device}` (this project has {})",
            device_list(&p)
        )
    })?;
    let cmd = d
        .project
        .config
        .command(command)
        .ok_or_else(|| {
            if device == MAIN_DEVICE {
                anyhow!("no command `{command}` in project.toml")
            } else {
                anyhow!("no command `{command}` on device `{device}`")
            }
        })?
        .clone();
    let config = d.project.config.clone();
    let overrides = parse_sets("--set", set)?;
    // Only the target device connects; bare --profile/--port are its own.
    let bench = connect_devices(&p, devices, &[device], device)?;
    let session = bench.session(device).expect("connected");
    let mut req = build_command(
        &config,
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
    pub devices: DeviceArgs,
    pub json: bool,
    pub channels: Vec<String>,
    pub send: Vec<String>,
    pub duration: Option<f64>,
    pub record: Option<PathBuf>,
    pub foxglove: Option<u16>,
    pub foxglove_bind: String,
}

/// `ref:` for a device other than `main`.
fn tag(device: &str) -> String {
    if device == MAIN_DEVICE {
        String::new()
    } else {
        format!("{device}:")
    }
}

pub fn monitor(a: MonitorArgs) -> Result<ExitCode> {
    let p = open_project(&a.project)?;
    let only: Vec<&str> = a.devices.only.iter().map(String::as_str).collect();
    let mut bench = connect_devices(&p, &a.devices, &only, MAIN_DEVICE)?;
    // One stream of (device, event) from every device.
    let (ev_tx, events) = crossbeam_channel::unbounded();
    for d in bench.devices() {
        let rx = d.session.subscribe();
        let tx = ev_tx.clone();
        let name = d.name.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = rx.recv() {
                if tx.send((name.clone(), ev)).is_err() {
                    return;
                }
            }
        });
    }
    drop(ev_tx);
    // Keep the server alive for the whole monitor run.
    let _foxglove = match a.foxglove {
        Some(port) => {
            let mut devs = Vec::new();
            for d in bench.devices() {
                devs.push(pb_core::foxglove::FoxgloveDevice {
                    name: d.name.clone(),
                    fds: d.project.require_schema()?.fds_bytes().to_vec(),
                    topics: pb_core::topics::known_topics_on(&d.name, d.session.codec()),
                });
            }
            let server = pb_core::foxglove::FoxgloveServer::start_devices(
                &a.foxglove_bind,
                port,
                &format!("ProtoBench: {}", p.config.name),
                &devs,
            )?;
            for d in bench.devices() {
                server.attach_device(d.session.subscribe(), d.session.wall_offset_ns(), &d.name);
            }
            eprintln!(
                "foxglove server on ws://{} (open it in Foxglove: Open connection ▸ Foxglove WebSocket)",
                server.local_addr()
            );
            Some(server)
        }
        None => None,
    };
    if let Some(path) = &a.record {
        bench.start_recording(path, &p)?;
        eprintln!("recording to {}", path.display());
    }
    for item in &a.send {
        // `ref:Start Stream` sends to device `ref`.
        let (dev, name) = match item.split_once(':') {
            Some((d, n)) if bench.get(d).is_some() => (d, n),
            _ => (MAIN_DEVICE, item.as_str()),
        };
        let d = bench
            .get(dev)
            .ok_or_else(|| anyhow!("--send {item}: device `{dev}` is not connected"))?;
        let cmd = d
            .project
            .config
            .command(name)
            .ok_or_else(|| anyhow!("--send {item}: no command `{name}`"))?;
        let req = build_command(
            &d.project.config,
            d.session.codec(),
            cmd,
            &[],
            &Default::default(),
        )?;
        let out = d.session.send_and_wait(req);
        eprintln!("{item}: {}", out.label());
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
        let (dev, ev) = match events.recv_timeout(Duration::from_millis(50)) {
            Ok(e) => e,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if !a.channels.is_empty() {
                    print_channels(&bench, &a.channels, a.json, &mut last, &mut out)?;
                }
                let over = bench.devices().iter().all(|d| {
                    matches!(
                        d.session.state(),
                        ConnState::Failed(_) | ConnState::Finished
                    )
                });
                if over {
                    break;
                }
                continue;
            }
            Err(_) => break,
        };
        if !a.channels.is_empty() {
            continue;
        }
        let tg = tag(&dev);
        match ev {
            SessionEvent::Message { dir, msg } => {
                let t = msg.host_ts_ns as f64 / 1e9;
                let d = match dir {
                    Direction::Rx => "rx",
                    Direction::Tx => "tx",
                };
                if a.json {
                    let mut line = serde_json::json!({"t": t, "dir": d, "type": msg.type_name, "kind": msg.kind(), "msg": msg.to_json()});
                    if dev != MAIN_DEVICE {
                        line["device"] = serde_json::Value::String(dev.clone());
                    }
                    writeln!(out, "{line}")?;
                } else {
                    let kind = format!("{tg}{}", msg.kind());
                    writeln!(out, "{t:10.4} {d} {kind:<12} {}", msg.to_json())?;
                }
            }
            SessionEvent::Text { host_ts_ns, line } => {
                if a.json {
                    let mut j = serde_json::json!({"t": host_ts_ns as f64 / 1e9, "text": line});
                    if dev != MAIN_DEVICE {
                        j["device"] = serde_json::Value::String(dev.clone());
                    }
                    writeln!(out, "{j}")?;
                } else {
                    writeln!(out, "{:10.4} log {tg}{line}", host_ts_ns as f64 / 1e9)?;
                }
            }
            SessionEvent::BadFrame(b) if !a.json => {
                writeln!(
                    out,
                    "{:10.4} bad {tg}{} {}",
                    b.host_ts_ns as f64 / 1e9,
                    b.kind,
                    b.reason
                )?;
            }
            SessionEvent::State(s) => eprintln!("{tg}{}", s.label()),
            _ => {}
        }
    }
    bench.stop_recording()?;
    Ok(ExitCode::SUCCESS)
}

fn print_channels(
    bench: &Bench,
    names: &[String],
    json: bool,
    last: &mut std::collections::HashMap<String, u64>,
    out: &mut impl Write,
) -> Result<()> {
    let mut row = serde_json::Map::new();
    let mut changed = false;
    for n in names {
        if let Some(ch) = bench.telemetry().get(n) {
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
    devices: &DeviceArgs,
    report_dir: Option<PathBuf>,
    non_interactive: Option<NonInteractive>,
    vars: &[String],
) -> Result<ExitCode> {
    crate::cmd_run::run(dir, sequence, devices, report_dir, non_interactive, vars)
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

pub fn sim_mqtt(dir: &Path, broker: &str, profile: Option<&str>) -> Result<ExitCode> {
    let p = open_project(dir)?;
    let host = pb_sim::mqtt::mqtt_profile(&p, profile)?;
    let dev = pb_sim::mqtt::device_profile(&host);
    let stop = Arc::new(AtomicBool::new(false));
    let t = pb_sim::mqtt::spawn_mqtt_device(&p, &host, broker, stop)
        .with_context(|| format!("simulator on mqtt://{broker}"))?;
    eprintln!(
        "simulator for `{}` on mqtt://{broker}: receives {}, publishes to {} (Ctrl-C to stop)",
        p.config.name,
        dev.subscribe.join(", "),
        dev.publish.as_deref().unwrap_or("-")
    );
    let _ = t.join();
    Ok(ExitCode::SUCCESS)
}

pub fn sim_can(dir: &Path, port: &str, profile: Option<&str>) -> Result<ExitCode> {
    let p = open_project(dir)?;
    let host = pb_sim::can::can_profile(&p, profile)?;
    let serial = pb_core::config::ConnectionProfile {
        port: port.to_string(),
        auto_reconnect: false,
        ..host.clone()
    };
    let link = pb_core::transport::SerialTransport::open(&serial)?;
    let stop = Arc::new(AtomicBool::new(false));
    let t = pb_sim::can::spawn_slcan_device(&p, &host, Box::new(link), stop)
        .with_context(|| format!("simulator on {port}"))?;
    eprintln!(
        "simulator for `{}` behind an emulated slcan adapter on {port}: listens on {:#x}, answers on {:#x} (Ctrl-C to stop)",
        p.config.name,
        host.tx_id.unwrap_or_default(),
        host.rx_id.unwrap_or_default(),
    );
    let _ = t.join();
    Ok(ExitCode::SUCCESS)
}
