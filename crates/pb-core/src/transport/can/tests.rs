use std::sync::Arc;

use crossbeam_channel::{Receiver as Rx, Sender};
use proptest::prelude::*;

use super::*;
use crate::transport::Transport;

/// One end of an in-memory CAN bus: what one end sends, the other gets.
struct MemBus {
    tx: Sender<CanFrame>,
    rx: Rx<CanFrame>,
    sent: Arc<std::sync::atomic::AtomicUsize>,
}

fn mem_pair() -> (MemBus, MemBus) {
    let (a_tx, a_rx) = crossbeam_channel::unbounded();
    let (b_tx, b_rx) = crossbeam_channel::unbounded();
    (
        MemBus {
            tx: a_tx,
            rx: b_rx,
            sent: Default::default(),
        },
        MemBus {
            tx: b_tx,
            rx: a_rx,
            sent: Default::default(),
        },
    )
}

impl CanBus for MemBus {
    fn recv(&mut self, timeout: Duration) -> io::Result<Option<CanFrame>> {
        match self.rx.recv_timeout(timeout) {
            Ok(f) => Ok(Some(f)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Ok(None),
            Err(_) => Err(io::Error::new(io::ErrorKind::BrokenPipe, "bus closed")),
        }
    }
    fn send(&mut self, f: &CanFrame) -> io::Result<()> {
        let max = if f.fd { 64 } else { 8 };
        assert!(f.data.len() <= max, "{} bytes in one frame", f.data.len());
        if f.fd && f.data.len() > 8 {
            assert!(
                FD_LENS.contains(&f.data.len()),
                "invalid FD length {}",
                f.data.len()
            );
        }
        self.sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.tx
            .send(f.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "bus closed"))
    }
    fn describe(&self) -> String {
        "memory bus".into()
    }
}

fn opts(fd: bool, isotp: Option<IsoTpParams>) -> CanOptions {
    CanOptions {
        tx_id: 0x7E0,
        rx_id: 0x7E8,
        extended: false,
        fd,
        brs: false,
        isotp,
    }
}

/// Sends `data` from one end and returns what the other end reads.
fn exchange(a: &mut CanTransport, b: CanTransport, data: &[u8]) -> Vec<u8> {
    let reader = std::thread::spawn(move || {
        let mut b = b;
        let mut buf = vec![0u8; 1 << 16];
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            let n = b.read(&mut buf).unwrap();
            if n > 0 || buf.is_empty() {
                return buf[..n].to_vec();
            }
        }
        panic!("nothing received");
    });
    a.write_all(data).unwrap();
    reader.join().unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn isotp_round_trips(
        len in prop_oneof![0usize..=64, 0usize..=4095],
        fd in any::<bool>(),
        block_size in 0u8..=8,
        st in prop_oneof![Just(0u8), Just(0xF1u8), Just(1u8)],
        pad in proptest::option::of(any::<u8>()),
        reverse in any::<bool>(),
        seed in any::<u8>(),
    ) {
        let data: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(31) ^ seed).collect();
        let p = IsoTpParams {
            frame_len: if fd { 64 } else { 8 },
            padding: pad,
            block_size,
            st_min: st,
            timeout: Duration::from_secs(2),
            max_len: MAX_MESSAGE,
        };
        let (x, y) = mem_pair();
        let o = opts(fd, Some(p));
        let (mut a, b) = if reverse {
            (CanTransport::new(Box::new(y), o.peer()), CanTransport::new(Box::new(x), o))
        } else {
            (CanTransport::new(Box::new(x), o.clone()), CanTransport::new(Box::new(y), o.peer()))
        };
        // An empty message is one single frame, which reads as 0 bytes.
        let got = if data.is_empty() {
            a.write_all(&data).unwrap();
            Vec::new()
        } else {
            exchange(&mut a, b, &data)
        };
        prop_assert_eq!(got, data);
    }
}

#[test]
fn long_messages_use_the_32_bit_first_frame() {
    let p = IsoTpParams {
        frame_len: 64,
        max_len: MAX_MESSAGE,
        ..Default::default()
    };
    let (x, y) = mem_pair();
    let o = opts(true, Some(p));
    let mut a = CanTransport::new(Box::new(x), o.clone());
    let b = CanTransport::new(Box::new(y), o.peer());
    let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7) as u8).collect();
    assert_eq!(exchange(&mut a, b, &data), data);
}

#[test]
fn raw_frames_and_foreign_ids() {
    let (x, mut y) = mem_pair();
    let mut a = CanTransport::new(Box::new(x), opts(false, None));
    // Frames for other ids are ignored; ours arrive one per read.
    y.send(&CanFrame {
        id: 0x123,
        extended: false,
        fd: false,
        brs: false,
        data: vec![1],
    })
    .unwrap();
    y.send(&CanFrame {
        id: 0x7E8,
        extended: false,
        fd: false,
        brs: false,
        data: vec![2, 3],
    })
    .unwrap();
    let mut buf = [0u8; 64];
    let mut got = Vec::new();
    for _ in 0..5 {
        let n = a.read(&mut buf).unwrap();
        if n > 0 {
            got.push(buf[..n].to_vec());
        }
    }
    assert_eq!(got, vec![vec![2, 3]]);
    a.write_all(&[9; 8]).unwrap();
    assert_eq!(
        y.recv(Duration::from_secs(1)).unwrap().unwrap().data,
        vec![9; 8]
    );
    let e = a.write_all(&[0; 9]).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    assert!(e.to_string().contains("turn isotp on"), "{e}");
}

#[test]
fn sender_honours_wait_and_overflow() {
    let p = IsoTpParams::default();
    let (x, mut y) = mem_pair();
    let mut a = CanTransport::new(Box::new(x), opts(false, Some(p.clone())));
    let fc = |data: Vec<u8>| CanFrame {
        id: 0x7E8,
        extended: false,
        fd: false,
        brs: false,
        data,
    };
    let t = std::thread::spawn(move || {
        // First frame → WAIT, then CONTINUE with a block size of 2.
        let ff = y.recv(Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!(ff.data[0] >> 4, 1);
        y.send(&fc(FlowControl::Wait.encode(&IsoTpParams::default())))
            .unwrap();
        y.send(&fc(FlowControl::Continue {
            block_size: 2,
            st_min: 0,
        }
        .encode(&IsoTpParams::default())))
            .unwrap();
        let mut cfs = 0;
        while cfs < 2 {
            if y.recv(Duration::from_secs(2)).unwrap().is_some() {
                cfs += 1;
            }
        }
        // Then refuse the rest.
        y.send(&fc(FlowControl::Overflow.encode(&IsoTpParams::default())))
            .unwrap();
    });
    let e = a.write_all(&[5; 40]).unwrap_err();
    t.join().unwrap();
    assert!(e.to_string().contains("cannot take 40 bytes"), "{e}");
    // Silence: N_Bs times out.
    let (x, _y) = mem_pair();
    let quick = IsoTpParams {
        timeout: Duration::from_millis(50),
        ..p
    };
    let mut a = CanTransport::new(Box::new(x), opts(false, Some(quick)));
    let e = a.write_all(&[5; 40]).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::TimedOut);
}

#[test]
fn profile_options() {
    let p = ConnectionProfile {
        transport: crate::config::TransportKind::Can,
        tx_id: Some(0x18DA_F110),
        rx_id: Some(0x18DA_10F1),
        extended_id: true,
        fd: true,
        data_bitrate: Some(2_000_000),
        isotp_st_min_us: 300,
        isotp_padding: Some(0x55),
        ..Default::default()
    };
    let o = CanOptions::from_profile(&p).unwrap();
    assert!(o.extended && o.fd && o.brs);
    let i = o.isotp.clone().unwrap();
    assert_eq!((i.frame_len, i.st_min, i.padding), (64, 0xF3, Some(0x55)));
    assert_eq!((o.peer().tx_id, o.peer().rx_id), (0x18DA_10F1, 0x18DA_F110));
    let none = ConnectionProfile { tx_id: None, ..p };
    assert!(CanOptions::from_profile(&none).is_err());
}

/// Needs a `vcan0` interface (`ip link add dev vcan0 type vcan && ip link
/// set up vcan0`); the WSL2 kernel has none.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "needs a vcan0 interface"]
fn socketcan_isotp_over_vcan() {
    let p = IsoTpParams::default();
    let a = SocketCanBus::open("vcan0", false).unwrap();
    let b = SocketCanBus::open("vcan0", false).unwrap();
    let o = opts(false, Some(p));
    let mut ta = CanTransport::new(Box::new(a), o.clone());
    let tb = CanTransport::new(Box::new(b), o.peer());
    let data: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
    assert_eq!(exchange(&mut ta, tb, &data), data);
}
