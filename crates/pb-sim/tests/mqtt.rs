//! Integration: the MQTT transport against `MiniBroker` (and, ignored by
//! default, a real Mosquitto).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pb_core::clock::SystemClock;
use pb_core::config::{ConnectionProfile, FramingType, TransportKind};
use pb_core::project::Project;
use pb_core::session::{ConnState, Session, SessionParams};
use pb_core::transport::mqtt::{MqttOptions, MqttTransport, SharedMqttState};
use pb_core::transport::{connector, Transport};
use pb_sim::mqtt::{spawn_mqtt_device, MiniBroker};

fn demo() -> Project {
    Project::open(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/demo-sensor"),
    )
}

fn host_profile(broker: &str, qos: u8) -> ConnectionProfile {
    ConnectionProfile {
        name: "mqtt".into(),
        transport: TransportKind::Mqtt,
        broker: broker.into(),
        subscribe: vec!["bench/+/up".into()],
        publish: Some("bench/sim/down".into()),
        qos,
        keep_alive_s: 5,
        ..Default::default()
    }
}

/// The demo project without framing (one message per PUBLISH) and with an
/// MQTT profile.
fn mqtt_project(broker: &str, qos: u8) -> Project {
    let mut p = demo();
    p.config.framing.kind = FramingType::None;
    p.config.connection.profiles.push(host_profile(broker, qos));
    p
}

fn start_host(p: &Project) -> Session {
    let prof = p.config.profile("mqtt").unwrap().clone();
    let conn = connector(&prof, &p.dir, &p.config.framing, None, None).unwrap();
    let params = SessionParams::new(p, &prof).unwrap();
    let s = Session::start(params, conn, SystemClock::shared()).unwrap();
    assert_eq!(
        s.wait_connected(Duration::from_secs(5)),
        ConnState::Connected
    );
    s
}

fn run_smoke(broker: &str, qos: u8) {
    let p = mqtt_project(broker, qos);
    let stop = Arc::new(AtomicBool::new(false));
    let host = p.config.profile("mqtt").unwrap().clone();
    let _dev = spawn_mqtt_device(&p, &host, broker, stop.clone()).unwrap();
    let s = start_host(&p);
    let seq = pb_core::sequence::SequenceFile::load(&p.sequence_path("smoke")).unwrap();
    let mut ev = |_e: pb_core::sequence::RunEvent| {};
    let r = pb_core::sequence::run_sequence(&s, &p.config, &seq, Default::default(), &mut ev, None);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(r.passed, "{}", r.summary_text());
    let st = s.shared().counters.snapshot();
    assert_eq!(st.framing_errors + st.decode_errors, 0, "{st:?}");
    assert!(s.endpoint().starts_with("mqtt://"), "{}", s.endpoint());
}

#[test]
fn smoke_sequence_over_minibroker() {
    let b = MiniBroker::start().unwrap();
    run_smoke(&b.addr().to_string(), 0);
    assert!(b.publishes() > 1000, "{}", b.publishes());
}

#[test]
fn smoke_sequence_over_minibroker_with_qos1() {
    let b = MiniBroker::start().unwrap();
    run_smoke(&b.addr().to_string(), 1);
}

fn client(broker: &str, sub: &[&str], publish: Option<&str>, qos: u8, ka: u16) -> MqttTransport {
    let prof = ConnectionProfile {
        transport: TransportKind::Mqtt,
        broker: broker.into(),
        subscribe: sub.iter().map(|s| s.to_string()).collect(),
        publish: publish.map(str::to_string),
        qos,
        keep_alive_s: ka,
        ..Default::default()
    };
    let opts = MqttOptions::from_profile(&prof, std::path::Path::new(".")).unwrap();
    MqttTransport::connect(opts, SharedMqttState::default()).unwrap()
}

/// Reads until a message arrives (or `limit` passes).
fn recv(t: &mut MqttTransport, limit: Duration) -> Option<Vec<u8>> {
    let end = Instant::now() + limit;
    let mut buf = vec![0u8; 4096];
    while Instant::now() < end {
        let n = t.read(&mut buf).unwrap();
        if n > 0 {
            return Some(buf[..n].to_vec());
        }
    }
    None
}

#[test]
fn keep_alive_pings_and_notices_a_silent_broker() {
    let b = MiniBroker::start().unwrap();
    let mut t = client(&b.addr().to_string(), &["x/y"], None, 0, 1);
    let mut buf = [0u8; 64];
    let end = Instant::now() + Duration::from_millis(2600);
    while Instant::now() < end {
        t.read(&mut buf).unwrap();
    }
    assert!(b.pings() >= 2, "pings: {}", b.pings());
    // The broker goes silent: the next unanswered ping ends the link.
    b.mute(true);
    let t0 = Instant::now();
    let e = loop {
        match t.read(&mut buf) {
            Ok(_) if t0.elapsed() < Duration::from_secs(5) => continue,
            Ok(_) => panic!("no error from a silent broker"),
            Err(e) => break e,
        }
    };
    assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
    assert!(e.to_string().contains("did not answer a ping"), "{e}");
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
}

#[test]
fn qos1_message_is_redelivered_after_the_connection_drops() {
    let b = MiniBroker::start().unwrap();
    let broker = b.addr().to_string();
    let mut sub = client(&broker, &["t/x"], None, 1, 0);
    // The publisher is a session, which reconnects through its connector.
    let mut p = mqtt_project(&broker, 1);
    // Only the message under test goes out (no status polls).
    p.config
        .telemetry
        .retain(|t| t.mode != pb_core::config::TelemetryMode::Poll);
    let prof = p.config.connection.profiles.last_mut().unwrap();
    prof.publish = Some("t/x".into());
    prof.subscribe = vec!["t/unused".into()];
    let s = start_host(&p);
    b.drop_next_publishes(1);
    s.send_raw(b"hello".to_vec());
    // Lost before its PUBACK; the reconnect sends it again.
    let got = recv(&mut sub, Duration::from_secs(5));
    assert_eq!(got.as_deref(), Some(&b"hello"[..]));
    assert!(b.connections() >= 3, "{}", b.connections());
    assert!(s.shared().counters.snapshot().reconnects >= 1 || s.state().is_connected());
    // Once acknowledged it is not sent again.
    assert_eq!(recv(&mut sub, Duration::from_millis(500)), None);
}

#[test]
fn persistent_session_receives_what_was_sent_while_away() {
    let b = MiniBroker::start().unwrap();
    let broker = b.addr().to_string();
    let prof = ConnectionProfile {
        transport: TransportKind::Mqtt,
        broker: broker.clone(),
        client_id: "keeper".into(),
        subscribe: vec!["k/#".into()],
        qos: 1,
        keep_alive_s: 0,
        ..Default::default()
    };
    let opts = MqttOptions::from_profile(&prof, std::path::Path::new(".")).unwrap();
    let state = SharedMqttState::default();
    drop(MqttTransport::connect(opts.clone(), state.clone()).unwrap());
    std::thread::sleep(Duration::from_millis(100));
    let mut pubr = client(&broker, &[], Some("k/1"), 1, 0);
    pubr.write_all(b"while away").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let mut back = MqttTransport::connect(opts, state).unwrap();
    assert_eq!(
        recv(&mut back, Duration::from_secs(2)).as_deref(),
        Some(&b"while away"[..])
    );
}

#[test]
fn tls_with_a_private_ca() {
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    let ca_params = {
        let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p
    };
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
    let srv_key = KeyPair::generate().unwrap();
    let srv_cert = CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .signed_by(&srv_key, &ca)
        .unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![srv_cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(srv_key.serialize_der().into()),
        )
        .unwrap();
    let b = MiniBroker::start_tls(Arc::new(server)).unwrap();
    let dir = std::env::temp_dir().join(format!("pb-mqtt-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ca.pem"), ca.pem()).unwrap();
    let prof = ConnectionProfile {
        transport: TransportKind::Mqtt,
        broker: format!("localhost:{}", b.addr().port()),
        subscribe: vec!["s/#".into()],
        publish: Some("s/echo".into()),
        tls: true,
        tls_ca: Some("ca.pem".into()),
        keep_alive_s: 0,
        ..Default::default()
    };
    let opts = MqttOptions::from_profile(&prof, &dir).unwrap();
    let mut t = MqttTransport::connect(opts, SharedMqttState::default()).unwrap();
    assert!(t.describe().starts_with("mqtts://"), "{}", t.describe());
    t.write_all(b"secret").unwrap();
    assert_eq!(
        recv(&mut t, Duration::from_secs(3)).as_deref(),
        Some(&b"secret"[..])
    );
    // Without the private CA the broker's certificate is not trusted.
    let untrusted = ConnectionProfile {
        tls_ca: None,
        ..prof
    };
    match MqttOptions::from_profile(&untrusted, &dir) {
        Ok(opts) => {
            let e = MqttTransport::connect(opts, SharedMqttState::default())
                .err()
                .expect("an untrusted certificate must fail")
                .to_string();
            assert!(e.to_lowercase().contains("certificate"), "{e}");
        }
        // No OS root store at all on this machine: also a refusal.
        Err(e) => assert!(e.to_string().contains("trusted"), "{e}"),
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A real broker: `nix shell nixpkgs#mosquitto --command cargo test -p
/// pb-sim --test mqtt -- --ignored`.
#[test]
#[ignore = "needs mosquitto on PATH"]
fn smoke_sequence_over_mosquitto() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dir = std::env::temp_dir().join(format!("pb-mosquitto-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("mosquitto.conf");
    std::fs::write(
        &conf,
        format!("listener {port} 127.0.0.1\nallow_anonymous true\npersistence false\n"),
    )
    .unwrap();
    let mut child = std::process::Command::new("mosquitto")
        .arg("-c")
        .arg(&conf)
        .spawn()
        .expect("mosquitto on PATH (nix shell nixpkgs#mosquitto)");
    let broker = format!("127.0.0.1:{port}");
    let up = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect(&broker).is_err() {
        assert!(Instant::now() < up, "mosquitto did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    let r = std::panic::catch_unwind(|| {
        run_smoke(&broker, 0);
        run_smoke(&broker, 1);
    });
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(dir);
    if let Err(e) = r {
        std::panic::resume_unwind(e);
    }
}
