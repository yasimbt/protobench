//! Integration: several devices at once (`[[devices]]`): the demo sensor
//! (`main`) and the reference meter (`ref`), each with its own simulator.

use std::time::Duration;

use pb_core::bench::Bench;
use pb_core::config::MAIN_DEVICE;
use pb_core::project::Project;
use pb_core::recorder::OfflineSession;
use pb_core::sequence::{run_sequence_on, RunEvent, RunReport, SequenceFile, StepStatus};
use pb_core::session::{ConnState, RecordKind, RequestOutcome};

fn demo() -> Project {
    let p = Project::open(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/demo-sensor"),
    );
    assert!(!p.has_errors(), "{:#?}", p.diagnostics);
    p
}

/// Starts the given devices on their simulators and waits until connected.
fn bench(p: &Project, devices: &[&str]) -> Bench {
    let mut b = Bench::default();
    for d in devices {
        b.start(
            p,
            d,
            Some("sim"),
            None,
            &|dp: &Project| pb_sim::sim_factory(dp),
            |_| {},
        )
        .unwrap();
    }
    for d in devices {
        let s = b.session(d).unwrap();
        assert_eq!(
            s.wait_connected(Duration::from_secs(3)),
            ConnState::Connected,
            "{d}"
        );
    }
    b
}

fn run(b: &Bench, p: &Project, seq: &SequenceFile) -> RunReport {
    let targets = b.targets(p);
    let mut ev = |_e: RunEvent| {};
    run_sequence_on(&targets, seq, Default::default(), &mut ev, None)
}

fn send(b: &Bench, device: &str, name: &str) -> RequestOutcome {
    let d = b.get(device).unwrap();
    let cmd = d.project.config.command(name).unwrap();
    let r = pb_core::commands::build_command(
        &d.project.config,
        d.session.codec(),
        cmd,
        &[],
        &Default::default(),
    )
    .unwrap();
    d.session.send_and_wait(r)
}

#[test]
fn compare_sequence_passes_against_both_simulators() {
    let p = demo();
    assert_eq!(p.devices.len(), 1);
    assert_eq!(p.devices[0].name, "ref");
    let b = bench(&p, &[MAIN_DEVICE, "ref"]);
    let seq = SequenceFile::load(&p.sequence_path("compare")).unwrap();
    let r = run(&b, &p, &seq);
    assert!(r.passed, "{}", r.summary_text());
    assert_eq!(r.summary.total, 8);
    // Both devices share one store: main's channels plain, ref's prefixed,
    // and the derived err_mv reads both.
    let t = b.telemetry();
    assert!(t.get("telemetry.vout_raw").is_some());
    assert!(t.get("ref.reading.vout_mv").is_some());
    let err = t.get("err_mv").unwrap().stats();
    assert!(err.count > 100, "{err:?}");
    assert!(
        r.endpoint.contains("main: ") && r.endpoint.contains("ref: "),
        "{}",
        r.endpoint
    );
}

#[test]
fn run_fails_at_the_start_when_a_device_is_missing() {
    let p = demo();
    let b = bench(&p, &[MAIN_DEVICE]);
    let seq = SequenceFile::load(&p.sequence_path("compare")).unwrap();
    let r = run(&b, &p, &seq);
    assert!(!r.passed);
    assert_eq!(r.steps.len(), 1, "no step runs");
    assert_eq!(r.steps[0].action, "devices");
    assert_eq!(r.steps[0].status, StepStatus::Error);
    assert_eq!(
        r.steps[0].message.as_deref(),
        Some("device `ref` is not connected")
    );
    let src = "[[steps]]\naction = \"expect\"\ndevice = \"meter2\"\nmessage = \"reading\"\n";
    let seq = SequenceFile::parse(src, std::path::Path::new("x.toml")).unwrap();
    let r = run(&b, &p, &seq);
    let m = r.steps[0].message.clone().unwrap();
    assert!(m.starts_with("no device `meter2`"), "{m}");
    // A sequence that names no device runs on main alone as before.
    let seq = SequenceFile::load(&p.sequence_path("smoke")).unwrap();
    assert!(run(&b, &p, &seq).passed);
}

#[test]
fn scripts_reach_other_devices() {
    let p = demo();
    let b = bench(&p, &[MAIN_DEVICE, "ref"]);
    let src = r#"
[[steps]]
action = "script"
code = """
let a = send("ref:Ping");
check(a.status == "ok", "ref:Ping answered");
check(a.response.pong.model == "SIM-REF-1", "model");
let meter = device("ref");
check(meter.name == "ref", "handle name");
check(meter.send("Ping").status == "ok", "handle send");
check(send("Ping").status == "ok", "main still the default");
sleep(300);
check(meter.last("reading.vout_mv") != (), "own channel name");
check(last("ref.reading.vout_mv") != (), "full channel name");
check(meter.stats("reading.vout_mv", 200).count > 5, "windowed stats");
"""
"#;
    let seq = SequenceFile::parse(src, std::path::Path::new("x.toml")).unwrap();
    let r = run(&b, &p, &seq);
    assert!(r.passed, "{}", r.summary_text());
    assert_eq!(r.steps[0].assertions.len(), 8);
    let src = "[[steps]]\naction = \"script\"\ncode = 'device(\"nope\")'\n";
    let seq = SequenceFile::parse(src, std::path::Path::new("x.toml")).unwrap();
    let r = run(&b, &p, &seq);
    assert_eq!(r.steps[0].status, StepStatus::Fail);
    assert!(
        r.steps[0]
            .message
            .as_deref()
            .unwrap()
            .contains("no connected device `nope`"),
        "{:?}",
        r.steps[0].message
    );
}

#[test]
fn two_device_recording_round_trip() {
    let p = demo();
    let mut b = bench(&p, &[MAIN_DEVICE, "ref"]);
    let dir = std::env::temp_dir().join(format!("pb-devices-{}", std::process::id()));
    let path = dir.join("bench.pbsession");
    b.start_recording(&path, &p).unwrap();
    assert!(send(&b, MAIN_DEVICE, "Start Stream").is_ok());
    assert!(send(&b, "ref", "Ping").is_ok());
    std::thread::sleep(Duration::from_millis(300));
    b.add_marker("both running");
    b.stop_recording().unwrap();

    let off = OfflineSession::open(&path).unwrap();
    let names: Vec<&str> = off.devices.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["main", "ref"]);
    let rx = |dev: u8| {
        off.records
            .iter()
            .filter(|r| r.device == dev && r.kind == RecordKind::RxFrame)
            .count()
    };
    assert!(rx(0) > 200 && rx(1) > 10, "{} {}", rx(0), rx(1));
    // Each device's frames decode with its own schema.
    let decoded = off.decoded();
    assert!(decoded.iter().all(|d| d.error.is_none()), "decode errors");
    let reading = decoded
        .iter()
        .find(|d| d.device == "ref" && d.record.kind == RecordKind::RxFrame)
        .unwrap();
    assert_eq!(reading.msg.as_ref().unwrap().type_name, "ref.MeterToHost");
    assert_eq!(reading.to_json()["device"], "ref");
    assert!(reading.to_line().contains("ref:"), "{}", reading.to_line());
    assert!(decoded
        .iter()
        .any(|d| d.record.kind == RecordKind::Marker && d.device == "main"));
    // Telemetry comes back with the live names.
    let store = off.telemetry(100_000).unwrap();
    for ch in ["telemetry.vout_raw", "ref.reading.vout_mv", "err_mv"] {
        assert!(store.get(ch).is_some_and(|c| c.total() > 0), "{ch}");
    }
    // CSV columns use the prefixed names.
    let csv = dir.join("bench.csv");
    pb_core::recorder::export_csv(
        &store,
        &["telemetry.vout_raw".into(), "ref.reading.vout_mv".into()],
        None,
        pb_core::recorder::CsvMode::Joined,
        &csv,
    )
    .unwrap();
    let head = std::fs::read_to_string(&csv).unwrap();
    assert!(
        head.lines().next().unwrap().contains("ref.reading.vout_mv"),
        "{}",
        head.lines().next().unwrap()
    );
    // MCAP topics: main keeps its own, ref's live under /ref.
    let mcap = dir.join("bench.mcap");
    pb_core::recorder::mcap_export::export_mcap(&off, &mcap).unwrap();
    let data = std::fs::read(&mcap).unwrap();
    let mut topics = std::collections::BTreeMap::<String, String>::new();
    for m in mcap::MessageStream::new(&data).unwrap() {
        let m = m.unwrap();
        let schema = m.channel.schema.as_ref().map(|s| s.name.clone());
        topics.insert(m.channel.topic.clone(), schema.unwrap_or_default());
    }
    for t in [
        "/rx/telemetry",
        "/ref/rx/reading",
        "/ref/rx/pong",
        "/ref/tx/ping",
    ] {
        assert!(topics.contains_key(t), "{t} in {topics:?}");
    }
    assert_eq!(topics["/ref/rx/reading"], "ref.Reading");
    assert_eq!(topics["/rx/telemetry"], "demo.Telemetry");
    // Replaying device ref plays only its frames.
    let rp = p.devices[0].project.config.framing.clone();
    let mut replay =
        pb_core::transport::ReplayTransport::open_device(&path, 0.0, &rp, "ref").unwrap();
    use pb_core::transport::Transport;
    let mut framer = pb_core::framing::build_framer(&rp);
    let (mut buf, mut ev, mut frames) = (vec![0u8; 4096], Vec::new(), 0);
    while !replay.finished() {
        let n = replay.read(&mut buf).unwrap();
        framer.push(&buf[..n], &mut ev);
        replay.take_text();
    }
    for e in ev {
        if matches!(e, pb_core::framing::FrameEvent::Frame(_)) {
            frames += 1;
        }
    }
    assert_eq!(frames, rx(1));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn foxglove_serves_every_device_with_its_own_schema() {
    use base64::Engine as _;
    use tungstenite::client::IntoClientRequest;
    use tungstenite::Message;

    let p = demo();
    let b = bench(&p, &[MAIN_DEVICE, "ref"]);
    let devs: Vec<pb_core::foxglove::FoxgloveDevice> = b
        .devices()
        .iter()
        .map(|d| pb_core::foxglove::FoxgloveDevice {
            name: d.name.clone(),
            fds: d.project.schema.as_ref().unwrap().fds_bytes().to_vec(),
            topics: pb_core::topics::known_topics_on(&d.name, d.session.codec()),
        })
        .collect();
    let fox =
        pb_core::foxglove::FoxgloveServer::start_devices("127.0.0.1", 0, "bench", &devs).unwrap();
    for d in b.devices() {
        fox.attach_device(d.session.subscribe(), d.session.wall_offset_ns(), &d.name);
    }
    let req = format!("ws://{}", fox.local_addr())
        .into_client_request()
        .unwrap();
    let (mut ws, _) = tungstenite::connect(req).unwrap();
    let mut next_json = || loop {
        if let Message::Text(t) = ws.read().unwrap() {
            return serde_json::from_str::<serde_json::Value>(t.as_str()).unwrap();
        }
    };
    assert_eq!(next_json()["op"], "serverInfo");
    let adv = next_json();
    let chans = adv["channels"].as_array().unwrap().clone();
    let find = |topic: &str| chans.iter().find(|c| c["topic"] == topic).cloned();
    let reading = find("/ref/rx/reading").expect("ref reading channel");
    assert_eq!(reading["schemaName"], "ref.Reading");
    assert!(find("/rx/telemetry").is_some() && find("/ref/text").is_some());
    // The ref channel carries the meter's schema, not the sensor's.
    let schema = base64::engine::general_purpose::STANDARD
        .decode(reading["schema"].as_str().unwrap())
        .unwrap();
    let pool = prost_reflect::DescriptorPool::decode(schema.as_slice()).unwrap();
    assert!(pool.get_message_by_name("ref.Reading").is_some());
    assert!(pool.get_message_by_name("demo.Telemetry").is_none());
    ws.send(Message::text(
        serde_json::json!({"op": "subscribe", "subscriptions": [{"id": 7, "channelId": reading["id"]}]})
            .to_string(),
    ))
    .unwrap();
    let bin = loop {
        if let Message::Binary(b) = ws.read().unwrap() {
            break b;
        }
    };
    assert_eq!(u32::from_le_bytes(bin[1..5].try_into().unwrap()), 7);
    let desc = pool.get_message_by_name("ref.Reading").unwrap();
    let msg = prost_reflect::DynamicMessage::decode(desc, &bin[13..]).unwrap();
    let v = msg.get_field_by_name("vout_mv").unwrap().as_f32().unwrap();
    assert!((0.0..=160.0).contains(&v), "{v}");
}
