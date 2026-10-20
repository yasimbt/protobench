use super::*;
use pb_core::clock::{Clock, ManualClock};
use pb_core::codec::DecodedMessage;

fn demo() -> Project {
    let mut p = Project::open(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/demo-sensor"),
    );
    // The demo adds 2 ms of latency; most tests want immediate replies.
    p.config.simulator.latency_ms = 0;
    p
}

struct Host {
    codec: Codec,
    framer: Box<dyn Framer>,
    project: Project,
}

impl Host {
    fn new(p: &Project) -> Host {
        let s = p.schema.clone().unwrap();
        Host {
            codec: Codec::new(&p.config, &s, Role::Host).unwrap(),
            framer: build_framer(&p.config.framing),
            project: p.clone(),
        }
    }

    fn request(&self, member: &str, payload: Json, seq: u64) -> Vec<u8> {
        let t = self.codec.resolve_tx(member).unwrap();
        let mut m = self.codec.build(&t, payload).unwrap();
        set_int_at_path(&mut m, &FieldPath::parse("seq").unwrap(), seq);
        self.framer.encode(&self.codec.encode(&m).unwrap())
    }

    fn decode(&mut self, bytes: &[u8]) -> (Vec<DecodedMessage>, Vec<String>, usize) {
        let mut ev = Vec::new();
        self.framer.push(bytes, &mut ev);
        self.framer.idle(&mut ev);
        let mut msgs = Vec::new();
        let mut text = Vec::new();
        let mut errors = 0;
        for e in ev {
            match e {
                FrameEvent::Frame(p) => match self.codec.decode(&p, 0) {
                    Ok(m) => msgs.push(m),
                    Err(_) => errors += 1,
                },
                FrameEvent::Text(t) => text.push(t),
                FrameEvent::Error { .. } => errors += 1,
            }
        }
        let _ = &self.project;
        (msgs, text, errors)
    }
}

#[test]
fn responds_with_seq_and_toggles_stream() {
    let p = demo();
    let clock = ManualClock::new();
    let mut sim = Simulator::for_project(&p, Arc::new(clock.clone())).unwrap();
    let mut host = Host::new(&p);

    let out = sim.step(
        &host.request("ping", serde_json::json!({}), 7),
        clock.now_ns(),
    );
    let (msgs, _, errs) = host.decode(&out);
    assert_eq!(errs, 0);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].kind(), "pong");
    assert_eq!(
        get_int_at_path(&msgs[0].msg, &FieldPath::parse("seq").unwrap()),
        Some(7)
    );

    // Nothing streams before stream_ctrl.
    clock.set_ns(10_000_000);
    let (msgs, _, _) = host.decode(&sim.step(&[], clock.now_ns()));
    assert!(msgs.is_empty());

    let out = sim.step(
        &host.request(
            "stream_ctrl",
            serde_json::json!({"enable": true, "rate_hz": 1000}),
            8,
        ),
        clock.now_ns(),
    );
    let (msgs, _, _) = host.decode(&out);
    assert_eq!(msgs[0].kind(), "ack");
    clock.set_ns(20_000_000);
    let (msgs, _, _) = host.decode(&sim.step(&[], clock.now_ns()));
    let tel: Vec<_> = msgs.iter().filter(|m| m.kind() == "telemetry").collect();
    // First sample at t=10 ms plus one per ms through 20 ms.
    assert!((10..=12).contains(&tel.len()), "{}", tel.len());
    let j = tel[0].to_json();
    let adc = j["telemetry"]["adc"].as_array().unwrap();
    assert_eq!(adc.len(), 4);
    assert!(j["telemetry"]["state"].is_string());

    // enable = false stops it.
    sim.step(
        &host.request("stream_ctrl", serde_json::json!({"enable": false}), 9),
        clock.now_ns(),
    );
    clock.set_ns(40_000_000);
    let (msgs, _, _) = host.decode(&sim.step(&[], clock.now_ns()));
    assert!(msgs.iter().all(|m| m.kind() != "telemetry"));
}

#[test]
fn status_uses_generators_and_text_lines_appear() {
    let p = demo();
    let clock = ManualClock::new();
    let mut sim = Simulator::for_project(&p, Arc::new(clock.clone())).unwrap();
    let mut host = Host::new(&p);
    clock.set_ns(60_000_000_000);
    let out = sim.step(
        &host.request("get_status", serde_json::json!({}), 1),
        clock.now_ns(),
    );
    let (msgs, text, _) = host.decode(&out);
    let j = msgs[0].to_json();
    assert_eq!(j["status"]["uptimeS"], 60);
    let mv = j["status"]["batteryMv"].as_u64().unwrap();
    assert!((3840..=3860).contains(&mv), "{mv}");
    assert!(
        text.iter().any(|t| t.starts_with("I: sim uptime")),
        "{text:?}"
    );
}

#[test]
fn latency_delays_responses() {
    let mut p = demo();
    p.config.simulator.latency_ms = 50;
    let clock = ManualClock::new();
    let mut sim = Simulator::for_project(&p, Arc::new(clock.clone())).unwrap();
    let mut host = Host::new(&p);
    let (msgs, _, _) = host.decode(&sim.step(&host.request("ping", serde_json::json!({}), 1), 0));
    assert!(msgs.is_empty());
    clock.set_ns(49_000_000);
    assert!(host.decode(&sim.step(&[], clock.now_ns())).0.is_empty());
    clock.set_ns(50_000_000);
    assert_eq!(host.decode(&sim.step(&[], clock.now_ns())).0.len(), 1);
}

#[test]
fn faults_produce_errors() {
    let mut p = demo();
    p.config.simulator.corrupt_rate = 0.2;
    p.config.simulator.garbage_rate = 0.2;
    p.config.simulator.streams[0].start_enabled = true;
    let clock = ManualClock::new();
    let mut sim = Simulator::for_project(&p, Arc::new(clock.clone())).unwrap();
    let mut host = Host::new(&p);
    clock.set_ns(1);
    sim.step(&[], clock.now_ns());
    clock.set_ns(500_000_000);
    let (msgs, _, errs) = host.decode(&sim.step(&[], clock.now_ns()));
    assert!(errs > 20, "{errs}");
    assert!(msgs.len() > 200, "{}", msgs.len());
    assert!(sim.stats().corrupted.load(Ordering::Relaxed) > 0);
}

#[test]
fn bad_generator_is_a_config_error() {
    let mut p = demo();
    p.config.simulator.streams[0]
        .fields
        .insert("vout_raw".into(), toml::Value::String("sine(amp=".into()));
    let e = Simulator::for_project(&p, Arc::new(ManualClock::new()))
        .err()
        .unwrap();
    assert!(e.to_string().contains("vout_raw"), "{e}");
}
