use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;

use super::*;
use crate::clock::SystemClock;
use crate::project::Project;
use crate::session::{ConnState, Session, SessionEvent, SessionParams};

/// A target whose RTT buffers are in memory. `room` limits how much a down
/// write takes at a time; `unplugged` makes every call fail.
#[derive(Default)]
struct Mem {
    up: HashMap<usize, VecDeque<u8>>,
    down: HashMap<usize, Vec<u8>>,
    room: Option<usize>,
    unplugged: bool,
}

#[derive(Clone, Default)]
struct MemTarget(Arc<Mutex<Mem>>);

impl MemTarget {
    fn push_up(&self, ch: usize, data: &[u8]) {
        self.0.lock().up.entry(ch).or_default().extend(data);
    }
    fn down(&self, ch: usize) -> Vec<u8> {
        self.0.lock().down.get(&ch).cloned().unwrap_or_default()
    }
}

impl RttTarget for MemTarget {
    fn read_up(&mut self, channel: usize, buf: &mut [u8]) -> io::Result<usize> {
        let mut m = self.0.lock();
        if m.unplugged {
            return Err(io::Error::other("probe unplugged"));
        }
        let q = m.up.entry(channel).or_default();
        let n = buf.len().min(q.len());
        for (i, b) in q.drain(..n).enumerate() {
            buf[i] = b;
        }
        Ok(n)
    }
    fn write_down(&mut self, channel: usize, data: &[u8]) -> io::Result<usize> {
        let mut m = self.0.lock();
        if m.unplugged {
            return Err(io::Error::other("probe unplugged"));
        }
        let n = m.room.map_or(data.len(), |r| r.min(data.len()));
        m.down
            .entry(channel)
            .or_default()
            .extend_from_slice(&data[..n]);
        Ok(n)
    }
    fn describe(&self) -> String {
        "memory target".into()
    }
}

fn opts(up: usize, down: usize, text: Option<usize>) -> RttOptions {
    RttOptions {
        chip: "TEST".into(),
        probe: None,
        up,
        down,
        text,
        attach: true,
        elf: None,
        speed_khz: None,
    }
}

#[test]
fn data_and_text_channels_are_routed() {
    let m = MemTarget::default();
    let mut t = RttTransport::new(Box::new(m.clone()), &opts(1, 1, Some(0)));
    m.push_up(0, b"I: boot\r\nW: lo");
    m.push_up(1, &[0xAA, 0x00]);
    let mut buf = [0u8; 64];
    assert_eq!(t.read(&mut buf).unwrap(), 2);
    assert_eq!(&buf[..2], &[0xAA, 0x00]);
    assert_eq!(t.take_text(), ["I: boot"]);
    m.push_up(0, b"w battery\n");
    assert_eq!(t.read(&mut buf).unwrap(), 0);
    assert_eq!(t.take_text(), ["W: low battery"]);
    // A text channel equal to the data channel is ignored.
    let t2 = RttTransport::new(Box::new(m.clone()), &opts(0, 0, Some(0)));
    assert!(t2.text.is_none());
    // Over-long lines are cut.
    m.push_up(0, &[b'x'; MAX_LINE + 10]);
    for _ in 0..8 {
        t.read(&mut buf).unwrap();
    }
    let lines = t.take_text();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].len() > MAX_LINE);
}

#[test]
fn writes_wait_for_room_and_time_out() {
    let m = MemTarget::default();
    m.0.lock().room = Some(3);
    let mut t = RttTransport::new(Box::new(m.clone()), &opts(1, 2, None));
    t.write_all(b"0123456789").unwrap();
    assert_eq!(m.down(2), b"0123456789");
    m.0.lock().room = Some(0);
    let e = t.write_all(b"x").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::TimedOut);
    assert!(e.to_string().contains("down channel 2 full"), "{e}");
}

#[test]
fn session_reconnects_when_the_probe_goes_away() {
    let p = Project::open(&crate::schema::tests::demo_dir());
    let targets: Arc<Mutex<VecDeque<MemTarget>>> = Arc::default();
    let first = MemTarget::default();
    let second = MemTarget::default();
    targets.lock().extend([first.clone(), second.clone()]);
    let o = opts(1, 1, Some(0));
    let t2 = targets.clone();
    let connector: crate::transport::Connector = Box::new(move || {
        let t = t2
            .lock()
            .pop_front()
            .ok_or_else(|| CoreError::Transport("no probe".into()))?;
        Ok(Box::new(RttTransport::new(Box::new(t), &o)) as Box<dyn Transport>)
    });
    let profile = p.config.profile("sim").unwrap().clone();
    let params = SessionParams::new(&p, &profile).unwrap();
    let s = Session::start(params, connector, SystemClock::shared()).unwrap();
    assert_eq!(
        s.wait_connected(Duration::from_secs(2)),
        ConnState::Connected
    );
    let ev = s.subscribe();
    first.push_up(0, b"I: first\n");
    // Unplug: the session reconnects to the next "probe".
    std::thread::sleep(Duration::from_millis(50));
    first.0.lock().unplugged = true;
    second.push_up(0, b"I: second\n");
    let mut lines = Vec::new();
    let end = Instant::now() + Duration::from_secs(5);
    while lines.len() < 2 && Instant::now() < end {
        if let Ok(SessionEvent::Text { line, .. }) = ev.recv_timeout(Duration::from_millis(50)) {
            lines.push(line);
        }
    }
    assert_eq!(lines, ["I: first", "I: second"]);
    assert!(s.shared().counters.snapshot().reconnects >= 1 || s.state().is_connected());
}

#[test]
fn probe_selection() {
    let probes = vec![
        ProbeInfo {
            identifier: "J-Link".into(),
            vid: 0x1366,
            pid: 0x0105,
            serial_number: Some("000683000001".into()),
        },
        ProbeInfo {
            identifier: "STLink V3".into(),
            vid: 0x0483,
            pid: 0x374e,
            serial_number: Some("ABC123".into()),
        },
        ProbeInfo {
            identifier: "STLink V3".into(),
            vid: 0x0483,
            pid: 0x374e,
            serial_number: Some("DEF456".into()),
        },
    ];
    assert_eq!(
        select_probe(&probes, Some("1366:0105")).unwrap().identifier,
        "J-Link"
    );
    assert_eq!(
        select_probe(&probes, Some("0483:374e:def456"))
            .unwrap()
            .serial_number
            .as_deref(),
        Some("DEF456")
    );
    assert_eq!(
        select_probe(&probes, Some("ABC123")).unwrap().selector(),
        "0483:374e:ABC123"
    );
    let e = select_probe(&probes, Some("0483:374e")).unwrap_err();
    assert!(e.starts_with("2 debug probes match `0483:374e`"), "{e}");
    let e = select_probe(&probes, None).unwrap_err();
    assert!(e.contains("3 debug probes match"), "{e}");
    let e = select_probe(&probes, Some("nope")).unwrap_err();
    assert!(
        e.contains("no debug probe matches `nope`") && e.contains("J-Link (1366:0105"),
        "{e}"
    );
    assert_eq!(
        select_probe(&probes[..1], None).unwrap().identifier,
        "J-Link"
    );
    assert_eq!(
        select_probe(&[], None).unwrap_err(),
        "no debug probes are attached"
    );
}
