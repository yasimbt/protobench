//! Integration: the CAN transport (ISO-TP over an slcan adapter) against the
//! simulator behind an emulated slcan adapter.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pb_core::clock::SystemClock;
use pb_core::config::{ConnectionProfile, FramingType, TransportKind};
use pb_core::error::CoreError;
use pb_core::project::Project;
use pb_core::sequence::RunReport;
use pb_core::session::{ConnState, Session, SessionParams};
use pb_core::transport::can::{CanOptions, CanTransport, SlcanBus};
use pb_core::transport::{loopback_pair, Connector, Transport};
use pb_sim::can::spawn_slcan_device;

fn can_project(fd: bool) -> Project {
    let mut p = Project::open(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/demo-sensor"),
    );
    p.config.framing.kind = FramingType::None;
    p.config.framing.text_passthrough = false;
    p.config.connection.profiles.push(ConnectionProfile {
        name: "can".into(),
        transport: TransportKind::Can,
        port: "loopback".into(),
        bitrate: 1_000_000,
        data_bitrate: fd.then_some(2_000_000),
        tx_id: Some(0x7E0),
        rx_id: Some(0x7E8),
        fd,
        ..Default::default()
    });
    p
}

/// Host session on one end of a loopback "serial port", the simulator
/// behind an emulated slcan adapter on the other.
fn start(p: &Project) -> (Session, Arc<AtomicBool>) {
    let prof = p.config.profile("can").unwrap().clone();
    let (host, dev) = loopback_pair();
    let stop = Arc::new(AtomicBool::new(false));
    spawn_slcan_device(p, &prof, Box::new(dev), stop.clone()).unwrap();
    let o = CanOptions::from_profile(&prof).unwrap();
    let mut host = Some(host);
    let (bitrate, data) = (prof.bitrate, prof.data_bitrate);
    let connector: Connector = Box::new(move || {
        let link = host
            .take()
            .ok_or_else(|| CoreError::Transport("loopback used".into()))?;
        let bus = SlcanBus::open(Box::new(link), bitrate, data)?;
        Ok(Box::new(CanTransport::new(Box::new(bus), o.clone())) as Box<dyn Transport>)
    });
    let mut params = SessionParams::new(p, &prof).unwrap();
    params.auto_reconnect = false;
    let s = Session::start(params, connector, SystemClock::shared()).unwrap();
    assert_eq!(
        s.wait_connected(Duration::from_secs(3)),
        ConnState::Connected
    );
    (s, stop)
}

fn smoke(p: &Project, s: &Session) -> RunReport {
    let seq = pb_core::sequence::SequenceFile::load(&p.sequence_path("smoke")).unwrap();
    let mut ev = |_e: pb_core::sequence::RunEvent| {};
    pb_core::sequence::run_sequence(s, &p.config, &seq, Default::default(), &mut ev, None)
}

#[test]
fn smoke_sequence_over_emulated_slcan_can_fd() {
    let p = can_project(true);
    let (s, stop) = start(&p);
    assert!(s.endpoint().contains("slcan"), "{}", s.endpoint());
    let r = smoke(&p, &s);
    stop.store(true, Ordering::Relaxed);
    assert!(r.passed, "{}", r.summary_text());
    let st = s.shared().counters.snapshot();
    assert_eq!(
        st.decode_errors + st.crc_errors + st.framing_errors,
        0,
        "{st:?}"
    );
}

#[test]
fn smoke_sequence_over_emulated_slcan_classic_can() {
    // Classic frames carry 8 bytes, so every telemetry message is a first
    // frame, flow control and consecutive frames.
    let p = can_project(false);
    let (s, stop) = start(&p);
    let r = smoke(&p, &s);
    stop.store(true, Ordering::Relaxed);
    assert!(r.passed, "{}", r.summary_text());
    let st = s.shared().counters.snapshot();
    assert_eq!(st.decode_errors + st.crc_errors, 0, "{st:?}");
}
