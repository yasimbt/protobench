//! Integration: session ↔ simulator over the in-process loopback and TCP.

use std::time::Duration;

use pb_core::clock::SystemClock;
use pb_core::commands::build_command;
use pb_core::project::Project;
use pb_core::session::{ConnState, RequestOutcome, Session, SessionParams};
use pb_core::transport::connector;

fn demo() -> Project {
    Project::open(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/demo-sensor"),
    )
}

fn start(p: &Project, profile: &str, port: Option<String>) -> Session {
    let prof = p.config.profile(profile).unwrap().clone();
    let sim = pb_sim::sim_factory(p).unwrap();
    let conn = connector(&prof, &p.dir, &p.config.framing, Some(sim), port).unwrap();
    let params = SessionParams::new(p, &prof).unwrap();
    let s = Session::start(params, conn, SystemClock::shared()).unwrap();
    assert_eq!(
        s.wait_connected(Duration::from_secs(3)),
        ConnState::Connected
    );
    s
}

fn send(s: &Session, p: &Project, name: &str) -> RequestOutcome {
    let r = build_command(
        &p.config,
        s.codec(),
        p.config.command(name).unwrap(),
        &[],
        &Default::default(),
    )
    .unwrap();
    s.send_and_wait(r)
}

fn exercise(s: &Session, p: &Project) {
    assert!(send(s, p, "Ping").is_ok());
    assert!(send(s, p, "Set Bias").is_ok());
    assert!(send(s, p, "Start Stream").is_ok());
    std::thread::sleep(Duration::from_millis(400));
    let ch = s
        .telemetry()
        .get("telemetry.current_na")
        .expect("current channel");
    let st = ch.stats();
    assert!(st.count > 200, "{st:?}");
    assert!((200.0..300.0).contains(&st.mean), "{st:?}");
    assert!(s.telemetry().get("telemetry.adc[3]").is_some());
    assert!(
        s.telemetry().get("status.battery_mv").is_some(),
        "poll telemetry"
    );
}

#[test]
fn loopback_simulator_end_to_end() {
    let p = demo();
    let s = start(&p, "sim", None);
    exercise(&s, &p);
    assert_eq!(s.stats().errors(), 0);
}

#[test]
fn tcp_simulator_end_to_end() {
    let p = demo();
    let (port, stop) = pb_sim::spawn_tcp_server(&p).unwrap();
    let s = start(&p, "net", Some(format!("127.0.0.1:{port}")));
    exercise(&s, &p);
    assert!(s.endpoint().contains(&port.to_string()));
    drop(s);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn fault_injection_counts_errors_and_retries_succeed() {
    let mut p = demo();
    p.config.simulator.corrupt_rate = 0.05;
    p.config.simulator.garbage_rate = 0.05;
    let s = start(&p, "sim", None);
    assert!(send(&s, &p, "Start Stream").is_ok() || send(&s, &p, "Start Stream").is_ok());
    std::thread::sleep(Duration::from_millis(800));
    let mut ok = 0;
    for _ in 0..10 {
        let cfg = p.config.command("Ping").unwrap().clone();
        let mut r = build_command(&p.config, s.codec(), &cfg, &[], &Default::default()).unwrap();
        r.retries = 5;
        r.timeout_ms = 100;
        if s.send_and_wait(r).is_ok() {
            ok += 1;
        }
    }
    assert_eq!(ok, 10, "ping with retries");
    let st = s.shared().counters.snapshot();
    assert!(st.crc_errors > 0, "{st:?}");
    assert!(st.framing_errors > 0, "{st:?}");
    let ch = s.telemetry().get("telemetry.vout_raw").unwrap();
    assert!(ch.total() > 500, "telemetry keeps flowing: {}", ch.total());
}

/// 5 kHz for 10 s through the loopback: no backlog, no dropped samples.
#[test]
#[ignore]
fn performance_5khz_10s() {
    let mut p = demo();
    p.config.simulator.streams[0].rate_hz = 5000.0;
    p.config.simulator.streams[0].start_enabled = true;
    // 10 numeric channels: vout, current, temperature, 4 adc, state, power + timestamp
    p.config.telemetry[0]
        .channels
        .push("telemetry.timestamp_us".into());
    p.config
        .telemetry
        .retain(|t| t.mode != pb_core::config::TelemetryMode::Poll);
    let s = start(&p, "sim", None);
    let _events = s.subscribe();
    let t0 = std::time::Instant::now();
    std::thread::sleep(Duration::from_secs(10));
    let elapsed = t0.elapsed().as_secs_f64();
    std::thread::sleep(Duration::from_millis(300));
    let ch = s.telemetry().get("telemetry.vout_raw").unwrap();
    let n = ch.total();
    let channels = s.telemetry().channels().len();
    let st = s.shared().counters.snapshot();
    println!(
        "{n} samples in {elapsed:.2}s on {channels} channels, rx_msgs {}, dropped events {}",
        st.rx_msgs, st.dropped_events
    );
    assert!(channels >= 10, "{channels}");
    assert!(n as f64 >= 5000.0 * elapsed * 0.97, "rate too low: {n}");
    assert_eq!(st.rx_msgs, st.rx_frames);
    assert_eq!(st.errors(), 0);
    assert_eq!(st.dropped_events, 0);
    // Every message produced the same number of samples on every channel.
    let adc = s.telemetry().get("telemetry.adc[3]").unwrap().total();
    assert_eq!(adc, n);
}

#[test]
fn recording_round_trip_reproduces_messages() {
    let p = demo();
    let s = start(&p, "sim", None);
    let events = s.subscribe();
    let path = std::env::temp_dir().join(format!("pb-it-rec-{}.pbsession", std::process::id()));
    let w = pb_core::recorder::SessionWriter::create(&path, &p).unwrap();
    s.start_recording(Box::new(w));
    assert!(send(&s, &p, "Ping").is_ok());
    assert!(send(&s, &p, "Start Stream").is_ok());
    std::thread::sleep(Duration::from_millis(200));
    s.add_marker("load connected");
    let mut w = s.stop_recording().unwrap();
    w.finish().unwrap();
    let live: Vec<_> = events
        .try_iter()
        .filter_map(|e| match e {
            pb_core::session::SessionEvent::Message {
                dir: pb_core::session::Direction::Rx,
                msg,
            } => Some(msg.to_json()),
            _ => None,
        })
        .collect();
    let off = pb_core::recorder::OfflineSession::open(&path).unwrap();
    let decoded = off.decoded();
    let rx: Vec<_> = decoded
        .iter()
        .filter(|d| d.record.kind == pb_core::session::RecordKind::RxFrame)
        .filter_map(|d| d.msg.as_ref().map(|m| m.to_json()))
        .collect();
    assert!(rx.len() > 100);
    // Every message seen live while recording is in the file, in order.
    assert_eq!(
        &live[..rx.len().min(live.len())],
        &rx[..rx.len().min(live.len())]
    );
    assert!(decoded
        .iter()
        .any(|d| d.record.kind == pb_core::session::RecordKind::Marker));
    assert!(decoded
        .iter()
        .any(|d| d.record.kind == pb_core::session::RecordKind::TxFrame && d.msg.is_some()));
    let store = off.telemetry(10_000).unwrap();
    assert!(store.get("telemetry.vout_raw").unwrap().total() > 100);
    let _ = std::fs::remove_file(path);
}
