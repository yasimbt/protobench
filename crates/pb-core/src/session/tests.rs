use std::sync::atomic::{AtomicBool, AtomicU32};
use std::time::Duration;

use super::*;
use crate::clock::SystemClock;
use crate::codec::Role;
use crate::commands::build_command;
use crate::transport::{loopback_pair, LoopbackEnd, Transport};

fn project() -> Project {
    let mut p = Project::open(&crate::schema::tests::demo_dir());
    // Polls would add traffic that the counter assertions do not expect.
    p.config.telemetry.retain(|t| t.mode != TelemetryMode::Poll);
    p
}

/// Minimal device for session tests: answers ping with pong and set_bias with
/// ack (echoing seq), optionally ignoring everything, and sends telemetry on
/// `stream_ctrl`.
fn spawn_device(
    mut end: LoopbackEnd,
    project: &Project,
    mute: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let cfg = project.config.clone();
    let schema = project.schema.clone().unwrap();
    std::thread::spawn(move || {
        let codec = Codec::new(&cfg, &schema, Role::Device).unwrap();
        let mut framer = build_framer(&cfg.framing);
        let seq_path = FieldPath::parse("seq").unwrap();
        let mut buf = [0u8; 4096];
        let mut ev = Vec::new();
        loop {
            let n = match end.read(&mut buf) {
                Ok(n) => n,
                Err(_) => return,
            };
            ev.clear();
            framer.push(&buf[..n], &mut ev);
            for e in ev.drain(..) {
                let FrameEvent::Frame(p) = e else { continue };
                let Ok(req) = codec.decode(&p, 0) else {
                    continue;
                };
                if mute.load(Ordering::Relaxed) {
                    continue;
                }
                let seq = get_int_at_path(&req.msg, &seq_path).unwrap_or(0);
                let (member, payload) = match req.member.as_deref() {
                    Some("ping") => ("pong", serde_json::json!({"firmware": "test"})),
                    Some("set_bias") => ("ack", serde_json::json!({"ok": true})),
                    Some("stream_ctrl") => {
                        // Send 50 telemetry messages, then the ack.
                        for i in 0..50u64 {
                            let t = codec.resolve_tx("telemetry").unwrap();
                            let m = codec
                                .build(&t, serde_json::json!({"timestamp_us": i * 1000, "vout_raw": 100 + i, "current_na": 250.0}))
                                .unwrap();
                            let w = framer.encode(&codec.encode(&m).unwrap());
                            if end.write_all(&w).is_err() {
                                return;
                            }
                        }
                        ("ack", serde_json::json!({"ok": true}))
                    }
                    _ => continue,
                };
                let t = codec.resolve_tx(member).unwrap();
                let mut m = codec.build(&t, payload).unwrap();
                set_int_at_path(&mut m, &seq_path, seq);
                let w = framer.encode(&codec.encode(&m).unwrap());
                let mut w2 = b"I: handled\n".to_vec();
                w2.extend(w);
                if end.write_all(&w2).is_err() {
                    return;
                }
            }
        }
    })
}

fn start(project: &Project, mute: Arc<AtomicBool>) -> Session {
    let (host, dev) = loopback_pair();
    spawn_device(dev, project, mute);
    let mut host = Some(host);
    let connector: Connector = Box::new(move || {
        host.take()
            .map(|h| Box::new(h) as Box<dyn Transport>)
            .ok_or_else(|| CoreError::Transport("loopback used".into()))
    });
    let profile = project.config.profile("sim").unwrap().clone();
    let mut params = SessionParams::new(project, &profile).unwrap();
    params.auto_reconnect = false;
    let s = Session::start(params, connector, SystemClock::shared()).unwrap();
    assert_eq!(
        s.wait_connected(Duration::from_secs(2)),
        ConnState::Connected
    );
    s
}

fn cmd(s: &Session, p: &Project, name: &str) -> SendRequest {
    build_command(
        &p.config,
        s.codec(),
        p.config.command(name).unwrap(),
        &[],
        &Default::default(),
    )
    .unwrap()
}

#[test]
fn request_response_with_seq_and_text() {
    let p = project();
    let s = start(&p, Arc::new(AtomicBool::new(false)));
    let events = s.subscribe();
    let out = s.send_and_wait(cmd(&s, &p, "Ping"));
    let RequestOutcome::Ok {
        response, attempts, ..
    } = &out
    else {
        panic!("{out:?}")
    };
    assert_eq!(*attempts, 1);
    assert_eq!(response.kind(), "pong");
    let out2 = s.send_and_wait(cmd(&s, &p, "Set Bias"));
    assert!(out2.is_ok(), "{out2:?}");
    // seq increments and is echoed
    let seq1 = get_int_at_path(&response.msg, &FieldPath::parse("seq").unwrap()).unwrap();
    let seq2 = get_int_at_path(
        &out2.response().unwrap().msg,
        &FieldPath::parse("seq").unwrap(),
    )
    .unwrap();
    assert_eq!(seq2, seq1 + 1);
    std::thread::sleep(Duration::from_millis(50));
    let evs: Vec<_> = events.try_iter().collect();
    assert!(evs
        .iter()
        .any(|e| matches!(e, SessionEvent::Text { line, .. } if line == "I: handled")));
    assert!(evs.iter().any(|e| matches!(
        e,
        SessionEvent::Message {
            dir: Direction::Tx,
            ..
        }
    )));
    assert!(evs.iter().any(|e| matches!(
        e,
        SessionEvent::Request {
            outcome: RequestOutcome::Ok { .. },
            ..
        }
    )));
    let st = s.shared().counters.snapshot();
    assert_eq!(st.tx_frames, 2);
    assert_eq!(st.rx_msgs, 2);
    assert_eq!(st.text_lines, 2);
    assert_eq!(s.shared().latency.list().len(), 2);
}

#[test]
fn timeout_and_retries_when_device_is_silent() {
    let p = project();
    let mute = Arc::new(AtomicBool::new(true));
    let s = start(&p, mute);
    let mut r = cmd(&s, &p, "Ping");
    r.timeout_ms = 30;
    r.retries = 2;
    let out = s.send_and_wait(r);
    assert!(
        matches!(out, RequestOutcome::Timeout { attempts: 3 }),
        "{out:?}"
    );
    let st = s.shared().counters.snapshot();
    assert_eq!(st.retries, 2);
    assert_eq!(st.timeouts, 1);
    assert_eq!(st.tx_frames, 3);
}

#[test]
fn telemetry_flows_into_store_with_device_time() {
    let p = project();
    let s = start(&p, Arc::new(AtomicBool::new(false)));
    let out = s.send_and_wait(cmd(&s, &p, "Start Stream"));
    assert!(out.is_ok(), "{out:?}");
    let ch = s.telemetry().get("telemetry.vout_raw").unwrap();
    assert_eq!(ch.total(), 50);
    let data = ch.data.read();
    let (t0, _) = data.ring.first().unwrap();
    let (t1, v1) = data.ring.last().unwrap();
    // 49 ms of device time between first and last sample.
    assert!((t1 - t0 - 0.049).abs() < 1e-6, "{t0} {t1}");
    assert!((v1 - 149.0 * 0.0503).abs() < 1e-9);
    drop(data);
    assert!(s.telemetry().get("power_uw").is_some());
}

#[test]
fn send_while_disconnected_fails_fast() {
    let p = project();
    let profile = p.config.profile("sim").unwrap().clone();
    let mut params = SessionParams::new(&p, &profile).unwrap();
    params.auto_reconnect = false;
    let connector: Connector = Box::new(|| Err(CoreError::Transport("no device".into())));
    let s = Session::start(params, connector, SystemClock::shared()).unwrap();
    assert!(matches!(
        s.wait_connected(Duration::from_secs(1)),
        ConnState::Failed(_)
    ));
    let out = s.send_and_wait(cmd(&s, &p, "Ping"));
    assert!(
        matches!(out, RequestOutcome::Error(ref m) if m == "not connected"),
        "{out:?}"
    );
}

#[test]
fn reconnects_with_backoff() {
    let p = project();
    let profile = p.config.profile("sim").unwrap().clone();
    let params = SessionParams::new(&p, &profile).unwrap();
    let tries = Arc::new(AtomicU32::new(0));
    let t2 = tries.clone();
    let (host, _dev) = loopback_pair();
    let mut host = Some(host);
    let connector: Connector = Box::new(move || {
        if t2.fetch_add(1, Ordering::SeqCst) < 2 {
            Err(CoreError::Transport("unplugged".into()))
        } else {
            host.take()
                .map(|h| Box::new(h) as Box<dyn Transport>)
                .ok_or_else(|| CoreError::Transport("gone".into()))
        }
    });
    let s = Session::start(params, connector, SystemClock::shared()).unwrap();
    assert_eq!(
        s.wait_connected(Duration::from_secs(3)),
        ConnState::Connected
    );
    assert_eq!(tries.load(Ordering::SeqCst), 3);
    assert_eq!(s.shared().counters.snapshot().reconnects, 2);
}

#[test]
fn unknown_ids_and_garbage_are_counted_not_fatal() {
    let mut p = project();
    // Switch the demo to id_prefix mode.
    p.config.codec.mode = crate::config::CodecMode::IdPrefix;
    p.config.codec.ids = [
        ("1".to_string(), "demo.Pong".to_string()),
        ("2".to_string(), "demo.Ping".to_string()),
    ]
    .into_iter()
    .collect();
    p.config.telemetry.clear();
    p.config.derived.clear();
    p.config.timestamp = Default::default();
    let (host, mut dev) = loopback_pair();
    let mut host = Some(host);
    let connector: Connector = Box::new(move || {
        host.take()
            .map(|h| Box::new(h) as Box<dyn Transport>)
            .ok_or_else(|| CoreError::Transport("used".into()))
    });
    let profile = p.config.profile("sim").unwrap().clone();
    let params = SessionParams::new(&p, &profile).unwrap();
    let s = Session::start(params, connector, SystemClock::shared()).unwrap();
    s.wait_connected(Duration::from_secs(1));
    let framer = build_framer(&p.config.framing);
    let mut bytes = framer.encode(&[0x7F, 0x08, 0x01]); // unknown id 0x7F
    bytes.extend(framer.encode(&[0x01])); // Pong
    bytes.extend([0x13, 0x37, 0x00]); // garbage frame
    bytes.extend(framer.encode(&[0x01])); // Pong
    dev.write_all(&bytes).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while s.shared().counters.snapshot().rx_msgs < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let st = s.shared().counters.snapshot();
    assert_eq!(st.unknown_ids, 1);
    assert_eq!(st.rx_msgs, 2);
    assert!(st.framing_errors + st.crc_errors >= 1);
    assert!(s
        .shared()
        .bad_frames
        .list()
        .iter()
        .any(|b| b.kind == "unknown_id"));
}

#[test]
fn periodic_send_repeats_until_stopped() {
    let p = project();
    let s = start(&p, Arc::new(AtomicBool::new(false)));
    s.set_periodic("hb", Some((cmd(&s, &p, "Ping"), 10)));
    assert_eq!(s.periodic_keys(), vec!["hb".to_string()]);
    std::thread::sleep(Duration::from_millis(120));
    s.set_periodic("hb", None);
    std::thread::sleep(Duration::from_millis(20));
    let n = s.shared().counters.snapshot().tx_frames;
    assert!(n >= 5, "{n}");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(s.shared().counters.snapshot().tx_frames, n);
}

fn wait_state(s: &Session, want: &ConnState, ms: u64) -> ConnState {
    let deadline = std::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let st = s.state();
        if &st == want || std::time::Instant::now() >= deadline {
            return st;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Answers one ping with a pong on a TCP client socket.
fn answer_ping(stream: &mut std::net::TcpStream, p: &Project) {
    use std::io::{Read, Write};
    let codec = Codec::new(&p.config, p.schema.as_ref().unwrap(), Role::Device).unwrap();
    let mut framer = build_framer(&p.config.framing);
    let seq_path = FieldPath::parse("seq").unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut buf = [0u8; 1024];
    let mut ev = Vec::new();
    loop {
        let n = stream.read(&mut buf).unwrap();
        assert!(n > 0, "host closed");
        framer.push(&buf[..n], &mut ev);
        for e in ev.drain(..) {
            let FrameEvent::Frame(f) = e else { continue };
            let req = codec.decode(&f, 0).unwrap();
            let seq = get_int_at_path(&req.msg, &seq_path).unwrap_or(0);
            let t = codec.resolve_tx("pong").unwrap();
            let mut m = codec.build(&t, serde_json::json!({})).unwrap();
            set_int_at_path(&mut m, &seq_path, seq);
            stream
                .write_all(&framer.encode(&codec.encode(&m).unwrap()))
                .unwrap();
            return;
        }
    }
}

/// R5: a TCP server tracks its client. Without one the session is
/// Listening and sends fail; losing the client fails pending requests and
/// resets the framer so a half frame from the old client cannot corrupt the
/// next client's first frame.
#[test]
fn tcp_server_tracks_client_changes() {
    use std::io::Write;
    let p = project();
    let server = crate::transport::TcpServer::bind("127.0.0.1", 0).unwrap();
    let addr = server.local_addr().unwrap();
    let mut server = Some(server);
    let connector: Connector = Box::new(move || {
        server
            .take()
            .map(|t| Box::new(t) as Box<dyn Transport>)
            .ok_or_else(|| CoreError::Transport("used".into()))
    });
    let profile = p.config.profile("sim").unwrap().clone();
    let mut params = SessionParams::new(&p, &profile).unwrap();
    params.auto_reconnect = false;
    let s = Session::start(params, connector, SystemClock::shared()).unwrap();
    assert_eq!(
        wait_state(&s, &ConnState::Listening, 2000),
        ConnState::Listening
    );
    assert_eq!(s.state().label(), "Waiting for client");
    let out = s.send_and_wait(cmd(&s, &p, "Ping"));
    assert!(
        matches!(out, RequestOutcome::Error(ref m) if m == "no client connected"),
        "{out:?}"
    );

    // First client: leaves a pending request and half a frame behind.
    let mut c1 = std::net::TcpStream::connect(addr).unwrap();
    assert_eq!(
        wait_state(&s, &ConnState::Connected, 2000),
        ConnState::Connected
    );
    let mut ping = cmd(&s, &p, "Ping");
    ping.timeout_ms = 5000;
    ping.retries = 0;
    let pending = s.send(ping);
    c1.write_all(&[0x05, 0x08, 0x01]).unwrap(); // start of a COBS frame
    std::thread::sleep(Duration::from_millis(50));
    drop(c1);
    assert_eq!(
        wait_state(&s, &ConnState::Listening, 2000),
        ConnState::Listening
    );
    let out = pending.wait(Duration::from_secs(2));
    assert!(
        matches!(out, RequestOutcome::Error(ref m) if m == "client disconnected"),
        "{out:?}"
    );

    // Second client answers normally.
    let mut c2 = std::net::TcpStream::connect(addr).unwrap();
    assert_eq!(
        wait_state(&s, &ConnState::Connected, 2000),
        ConnState::Connected
    );
    let p2 = p.clone();
    let dev = std::thread::spawn(move || answer_ping(&mut c2, &p2));
    let out = s.send_and_wait(cmd(&s, &p, "Ping"));
    assert!(out.is_ok(), "{out:?}");
    dev.join().unwrap();
    let st = s.shared().counters.snapshot();
    assert_eq!(st.framing_errors + st.crc_errors, 0, "{st:?}");
}

/// R6: a TCP peer that stops reading no longer blocks the I/O thread; the
/// write times out and the link is reported lost.
#[test]
fn stalled_tcp_peer_times_out_instead_of_hanging() {
    let p = project();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // Accept and never read.
    let hold = std::thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_secs(4));
        drop(sock);
    });
    let profile = p.config.profile("sim").unwrap().clone();
    let mut params = SessionParams::new(&p, &profile).unwrap();
    params.auto_reconnect = false;
    let connector: Connector = Box::new(move || {
        crate::transport::TcpClient::connect(&addr.ip().to_string(), addr.port())
            .map(|t| Box::new(t) as Box<dyn Transport>)
    });
    let mut s = Session::start(params, connector, SystemClock::shared()).unwrap();
    assert_eq!(
        s.wait_connected(Duration::from_secs(2)),
        ConnState::Connected
    );
    // Far more than the socket buffers hold.
    for _ in 0..16 {
        s.send_raw(vec![0x55; 4 << 20]);
    }
    let t0 = std::time::Instant::now();
    let st = loop {
        let st = s.state();
        if matches!(st, ConnState::Failed(_)) || t0.elapsed() > Duration::from_secs(6) {
            break st;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        matches!(&st, ConnState::Failed(m) if m.contains("peer stopped reading")),
        "{st:?}"
    );
    // The I/O thread is free: stopping returns promptly.
    let t1 = std::time::Instant::now();
    s.stop();
    assert!(t1.elapsed() < Duration::from_secs(1));
    hold.join().unwrap();
}
