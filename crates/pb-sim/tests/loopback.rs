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

fn run_smoke(p: &Project, profile: &str, port: Option<String>) -> pb_core::sequence::RunReport {
    let s = start(p, profile, port);
    let seq = pb_core::sequence::SequenceFile::load(&p.sequence_path("smoke")).unwrap();
    let mut ev = |_e: pb_core::sequence::RunEvent| {};
    pb_core::sequence::run_sequence(&s, &p.config, &seq, Default::default(), &mut ev, None)
}

#[test]
fn smoke_sequence_passes_on_loopback() {
    let p = demo();
    let r = run_smoke(&p, "sim", None);
    assert!(r.passed, "{}", r.summary_text());
    // 7 top-level steps + 3 loop iterations (the loop itself is not a step).
    assert_eq!(r.summary.total, 10, "{}", r.summary_text());
    let cap = r.steps.iter().find(|s| s.action == "capture").unwrap();
    assert_eq!(cap.assertions.len(), 3);
    assert!(cap.measured.contains_key("telemetry.current_na"));
    let x = r.to_junit();
    assert_eq!(x.matches("<testcase ").count(), 10);
}

#[test]
fn smoke_sequence_passes_over_tcp() {
    let p = demo();
    let (port, stop) = pb_sim::spawn_tcp_server(&p).unwrap();
    let r = run_smoke(&p, "net", Some(format!("127.0.0.1:{port}")));
    assert!(r.passed, "{}", r.summary_text());
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn failing_assertion_fails_and_skips_the_rest() {
    let p = demo();
    let s = start(&p, "sim", None);
    let src = r#"
name = "neg"
[[steps]]
action = "send"
command = "Set Bias"
payload = { millivolts = 5 }
expect = "ack"
assert = [ { path = "ack.ok", eq = false } ]
[[steps]]
action = "prompt_operator"
message = "never shown"
"#;
    let seq =
        pb_core::sequence::SequenceFile::parse(src, std::path::Path::new("neg.toml")).unwrap();
    let mut ev = |_e: pb_core::sequence::RunEvent| {};
    let r = pb_core::sequence::run_sequence(&s, &p.config, &seq, Default::default(), &mut ev, None);
    assert!(!r.passed);
    assert_eq!(r.steps[0].status, pb_core::sequence::StepStatus::Fail);
    assert_eq!(r.steps[1].status, pb_core::sequence::StepStatus::Skipped);
    // Headless prompt defaults to fail.
    let src = "[[steps]]\naction = \"prompt_operator\"\nmessage = \"x\"\n";
    let seq = pb_core::sequence::SequenceFile::parse(src, std::path::Path::new("p.toml")).unwrap();
    let r = pb_core::sequence::run_sequence(&s, &p.config, &seq, Default::default(), &mut ev, None);
    assert!(!r.passed);
}

#[test]
fn expect_set_save_and_vars() {
    let p = demo();
    let s = start(&p, "sim", None);
    let src = r#"
[vars]
cmd = "Ping"
[[steps]]
action = "send"
command = "${cmd}"
save = { fw = "pong.firmware" }
[[steps]]
action = "set"
var = "rate"
value = 500
[[steps]]
action = "send"
message = "stream_ctrl"
payload = { enable = true, rate_hz = "${rate}" }
expect = "ack"
[[steps]]
action = "expect"
message = "telemetry"
timeout_ms = 1000
assert = [ { path = "telemetry.vout_raw", min = 0, max = 4095 } ]
[[steps]]
action = "capture"
channels = ["telemetry.vout_raw"]
samples = 100
assert = [ { channel = "telemetry.vout_raw", stat = "count", min = 100 } ]
[[steps]]
action = "log"
text = "fw=${fw} rate=${rate}"
"#;
    let seq = pb_core::sequence::SequenceFile::parse(src, std::path::Path::new("v.toml")).unwrap();
    let mut ev = |_e: pb_core::sequence::RunEvent| {};
    let r = pb_core::sequence::run_sequence(&s, &p.config, &seq, Default::default(), &mut ev, None);
    assert!(r.passed, "{}", r.summary_text());
    assert_eq!(r.log, vec!["fw= rate=500".to_string()]);
}

#[test]
fn stored_samples_are_time_ordered_and_smooth() {
    let p = demo();
    let s = start(&p, "sim", None);
    assert!(send(&s, &p, "Start Stream").is_ok());
    std::thread::sleep(Duration::from_millis(500));
    let ch = s.telemetry().get("telemetry.vout_raw").unwrap();
    let d = ch.data.read();
    let pts: Vec<(f64, f64)> = d.ring.iter().collect();
    assert!(pts.len() > 300);
    for w in pts.windows(2) {
        assert!(w[1].0 >= w[0].0, "time went backwards: {:?}", w);
        let dt = w[1].0 - w[0].0;
        assert!((dt - 0.001).abs() < 1e-6, "uneven spacing {dt}");
        // 2 Hz sine of 75 mV amplitude changes < 1 mV per ms.
        assert!((w[1].1 - w[0].1).abs() < 1.0, "jump {:?}", w);
    }
}

fn record_some(p: &Project, ms: u64) -> std::path::PathBuf {
    let s = start(p, "sim", None);
    let path = std::env::temp_dir().join(format!("pb-it-{}-{ms}.pbsession", std::process::id()));
    s.start_recording(Box::new(
        pb_core::recorder::SessionWriter::create(&path, p).unwrap(),
    ));
    assert!(send(&s, p, "Start Stream").is_ok());
    std::thread::sleep(Duration::from_millis(ms));
    s.add_marker("m1");
    s.stop_recording().unwrap().finish().unwrap();
    path
}

#[test]
fn replay_transport_plays_back_a_recording() {
    let p = demo();
    let path = record_some(&p, 300);
    let off = pb_core::recorder::OfflineSession::open(&path).unwrap();
    let rx = off
        .records
        .iter()
        .filter(|r| r.kind == pb_core::session::RecordKind::RxFrame)
        .count();
    let mut prof = p.config.profile("sim").unwrap().clone();
    prof.transport = pb_core::config::TransportKind::Replay;
    prof.file = Some(path.display().to_string());
    prof.speed = 0.0;
    prof.auto_reconnect = false;
    let conn = connector(&prof, &p.dir, &p.config.framing, None, None).unwrap();
    let mut params = SessionParams::new(&p, &prof).unwrap();
    params.auto_reconnect = false;
    let s = Session::start(params, conn, pb_core::clock::SystemClock::shared()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while s.state() != ConnState::Finished && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(s.state(), ConnState::Finished);
    assert_eq!(s.shared().counters.snapshot().rx_msgs as usize, rx);
    assert!(s.telemetry().get("telemetry.vout_raw").unwrap().total() > 200);
    // Real-time replay takes about as long as the recording.
    prof.speed = 1.0;
    let mut r = pb_core::transport::ReplayTransport::open(&path, 1.0, &p.config.framing).unwrap();
    use pb_core::transport::Transport;
    let t0 = std::time::Instant::now();
    let mut buf = vec![0u8; 4096];
    while !r.finished() {
        r.read(&mut buf).unwrap();
    }
    assert!(
        t0.elapsed() >= Duration::from_millis(200),
        "{:?}",
        t0.elapsed()
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn mcap_export_has_one_channel_per_type() {
    let p = demo();
    let path = record_some(&p, 200);
    let off = pb_core::recorder::OfflineSession::open(&path).unwrap();
    let out = path.with_extension("mcap");
    let n = pb_core::recorder::mcap_export::export_mcap(&off, &out).unwrap();
    assert!(n > 100);
    let bytes = std::fs::read(&out).unwrap();
    let mut topics = std::collections::BTreeMap::<String, usize>::new();
    let mut last_time = 0;
    for m in mcap::MessageStream::new(&bytes).unwrap() {
        let m = m.unwrap();
        *topics.entry(m.channel.topic.clone()).or_default() += 1;
        if m.channel.topic == "/rx/telemetry" {
            let schema = m.channel.schema.as_ref().unwrap();
            assert_eq!(schema.name, "demo.Telemetry");
            assert_eq!(schema.encoding, "protobuf");
            assert_eq!(m.channel.message_encoding, "protobuf");
            assert!(m.log_time >= last_time);
            last_time = m.log_time;
            // Payload decodes as the schema type.
            let desc = off.schema.message("demo.Telemetry").unwrap();
            prost_reflect::DynamicMessage::decode(desc, m.data.as_ref()).unwrap();
        }
    }
    assert!(topics["/rx/telemetry"] > 100, "{topics:?}");
    assert!(topics.contains_key("/tx/stream_ctrl"), "{topics:?}");
    assert!(topics.contains_key("/rx/ack"), "{topics:?}");
    assert!(topics.contains_key("/markers"), "{topics:?}");
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(out);
}

fn run_src(s: &Session, p: &Project, src: &str) -> pb_core::sequence::RunReport {
    let seq = pb_core::sequence::SequenceFile::parse(src, std::path::Path::new("t.toml")).unwrap();
    let mut ev = |_e: pb_core::sequence::RunEvent| {};
    pb_core::sequence::run_sequence(s, &p.config, &seq, Default::default(), &mut ev, None)
}

/// R4: a reply that arrives before its `expect` step starts is not missed.
#[test]
fn expect_sees_reply_to_an_earlier_fire_and_forget_send() {
    let p = demo();
    let s = start(&p, "sim", None);
    let r = run_src(
        &s,
        &p,
        r#"
[[steps]]
action = "send"
message = "ping"
[[steps]]
action = "wait"
ms = 100
[[steps]]
action = "expect"
message = "pong"
timeout_ms = 200
"#,
    );
    assert!(r.passed, "{}", r.summary_text());
}

/// R4: each message is consumed once: a reply used by `send … expect` is not
/// matched again by a following `expect`, and two expects need two messages.
#[test]
fn expect_consumes_each_message_once() {
    let p = demo();
    let s = start(&p, "sim", None);
    let r = run_src(
        &s,
        &p,
        r#"
[[steps]]
action = "send"
command = "Ping"
[[steps]]
action = "expect"
message = "pong"
timeout_ms = 200
"#,
    );
    assert!(!r.passed, "the pong belongs to the send step");
    let r = run_src(
        &s,
        &p,
        r#"
[[steps]]
action = "send"
message = "ping"
[[steps]]
action = "expect"
message = "pong"
timeout_ms = 300
[[steps]]
action = "expect"
message = "pong"
timeout_ms = 200
"#,
    );
    use pb_core::sequence::StepStatus::*;
    let st: Vec<_> = r.steps.iter().map(|s| s.status).collect();
    // One ping, one pong: the second expect has nothing left to match.
    assert_eq!(st, vec![Pass, Pass, Fail], "{}", r.summary_text());
}

/// R8: an assertion on a channel that is not in `channels` measures only the
/// capture window, not everything since the session started.
#[test]
fn capture_assertion_on_unlisted_channel_uses_the_window() {
    let p = demo();
    let s = start(&p, "sim", None);
    let r = run_src(
        &s,
        &p,
        r#"
[[steps]]
action = "send"
command = "Start Stream"
[[steps]]
action = "wait"
ms = 1000
[[steps]]
action = "capture"
channels = ["telemetry.vout_raw"]
duration_ms = 200
assert = [ { channel = "telemetry.current_na", stat = "count", min = 100, max = 400 } ]
"#,
    );
    assert!(r.passed, "{}", r.summary_text());
}

#[test]
fn scripted_demo_sequence_passes() {
    let p = demo();
    let s = start(&p, "sim", None);
    let seq = pb_core::sequence::SequenceFile::load(&p.sequence_path("scripted")).unwrap();
    let mut ev = |_e: pb_core::sequence::RunEvent| {};
    let r = pb_core::sequence::run_sequence(&s, &p.config, &seq, Default::default(), &mut ev, None);
    assert!(r.passed, "{}\n{:#?}", r.summary_text(), r.steps);
    assert_eq!(r.steps[0].assertions.len(), 3);
    let stats = &r.steps[2];
    assert_eq!(stats.assertions.len(), 2);
    assert!(stats.measured["result"]["mean_na"].as_f64().unwrap() > 200.0);
    assert_eq!(r.vars["last_bias"], serde_json::json!(300));
    assert_eq!(r.log.last().unwrap(), "Sweep done, bias left at 300 mV");
    assert!(r.log.iter().any(|l| l.starts_with("current: mean")));
}
