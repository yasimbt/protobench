use super::*;
use crate::config::{CrcKind, Endian, FramingConfig, FramingType};
use proptest::prelude::*;

fn cfg(kind: FramingType, crc: CrcKind) -> FramingConfig {
    FramingConfig {
        kind,
        crc,
        max_frame_len: 512,
        ..Default::default()
    }
}

/// Every framing variant the tests cover.
fn all_configs() -> Vec<FramingConfig> {
    let mut v = Vec::new();
    for crc in [
        CrcKind::None,
        CrcKind::Crc8,
        CrcKind::Crc16CcittFalse,
        CrcKind::Crc16Modbus,
        CrcKind::Crc32,
    ] {
        v.push(cfg(FramingType::Cobs, crc));
        v.push(cfg(FramingType::Slip, crc));
        v.push(cfg(FramingType::VarintLen, crc));
        for len_bytes in [1, 2, 4] {
            for endian in [Endian::Little, Endian::Big] {
                let mut c = cfg(FramingType::FixedLen, crc);
                c.len_bytes = len_bytes;
                c.endian = endian;
                if len_bytes == 1 {
                    c.max_frame_len = 200;
                }
                v.push(c.clone());
                c.sync_word = vec![0xAA, 0x55];
                v.push(c);
            }
        }
        let mut c = cfg(FramingType::VarintLen, crc);
        c.sync_word = vec![0xAA, 0x55];
        v.push(c);
    }
    v
}

/// Configs that can detect garbage and resynchronise.
fn resync_configs() -> Vec<FramingConfig> {
    all_configs()
        .into_iter()
        .filter(|c| match c.kind {
            FramingType::Cobs | FramingType::Slip => true,
            _ => c.crc != CrcKind::None && c.crc != CrcKind::Crc8,
        })
        .collect()
}

fn frames(events: &[FrameEvent]) -> Vec<Vec<u8>> {
    events
        .iter()
        .filter_map(|e| match e {
            FrameEvent::Frame(f) => Some(f.clone()),
            _ => None,
        })
        .collect()
}

fn payloads() -> impl Strategy<Value = Vec<Vec<u8>>> {
    prop::collection::vec(prop::collection::vec(any::<u8>(), 1..150), 1..6)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn round_trip_random_payloads(ps in payloads()) {
        for c in all_configs() {
            let mut f = build_framer(&c);
            let mut stream = Vec::new();
            for p in &ps { stream.extend(f.encode(p)); }
            let mut ev = Vec::new();
            f.push(&stream, &mut ev);
            prop_assert_eq!(frames(&ev), ps.clone(), "{:?}", c);
            prop_assert_eq!(ev.len(), ps.len(), "{:?} {:?}", c, ev);
        }
    }

    #[test]
    fn resync_after_garbage(ps in payloads(), garbage in prop::collection::vec(any::<u8>(), 1..40)) {
        for c in resync_configs() {
            let mut f = build_framer(&c);
            let mut stream = garbage.clone();
            for p in &ps { stream.extend(f.encode(p)); }
            // Trailing frames that must be recovered.
            let tail = vec![vec![1u8, 2, 3, 4, 5, 6, 7, 8], vec![9u8; 20], vec![0x42u8; 33]];
            for p in &tail { stream.extend(f.encode(p)); }
            // Keep the link busy: a length framer may wait for a bogus length
            // read from the garbage before it slides on.
            let filler = vec![0x77u8; 100];
            for _ in 0..8 { stream.extend(f.encode(&filler)); }
            let mut ev = Vec::new();
            f.push(&stream, &mut ev);
            let got = frames(&ev);
            let found = got.windows(tail.len()).any(|w| w == &tail[..]);
            prop_assert!(found, "{:?}: {:?}", c, got.len());
        }
    }
}

#[test]
fn split_reads_at_every_boundary() {
    let ps: Vec<Vec<u8>> = vec![
        vec![0x00, 0x01, 0x00],
        vec![0xC0, 0xDB, 0x0A, 0x55, 0xAA],
        (0..=255u8).collect(),
    ];
    for c in all_configs() {
        let mut c = c;
        c.max_frame_len = 300;
        if c.len_bytes == 1 && c.kind == FramingType::FixedLen {
            continue;
        }
        let enc = build_framer(&c);
        let mut stream = Vec::new();
        for p in &ps {
            stream.extend(enc.encode(p));
        }
        for k in 0..=stream.len() {
            let mut f = build_framer(&c);
            let mut ev = Vec::new();
            f.push(&stream[..k], &mut ev);
            f.push(&stream[k..], &mut ev);
            assert_eq!(frames(&ev), ps, "{c:?} split at {k}");
        }
        // Byte by byte.
        let mut f = build_framer(&c);
        let mut ev = Vec::new();
        for b in &stream {
            f.push(std::slice::from_ref(b), &mut ev);
        }
        assert_eq!(frames(&ev), ps, "{c:?} byte by byte");
    }
}

#[test]
fn cobs_wire_format_matches_docs() {
    // COBS( payload || crc16_ccitt_false_le ) || 0x00
    let c = FramingConfig {
        kind: FramingType::Cobs,
        crc: CrcKind::Crc16CcittFalse,
        crc_endian: Endian::Little,
        ..Default::default()
    };
    let f = build_framer(&c);
    let payload = [0x08, 0x01, 0x12, 0x00];
    let mut body = payload.to_vec();
    let crc = Crc::new(CrcKind::Crc16CcittFalse, Endian::Little).compute(&payload) as u16;
    body.extend_from_slice(&crc.to_le_bytes());
    let mut expect = Vec::new();
    cobs::encode(&body, &mut expect);
    expect.push(0);
    assert_eq!(f.encode(&payload), expect);
    assert_eq!(*f.encode(&payload).last().unwrap(), 0x00);
}

#[test]
fn text_lines_between_cobs_frames() {
    let mut c = cfg(FramingType::Cobs, CrcKind::Crc16CcittFalse);
    c.text_passthrough = true;
    let mut f = build_framer(&c);
    let mut stream = b"I: boot ok\r\n".to_vec();
    stream.extend(f.encode(&[0x08, 0x0A, 0x10, 0x0A]));
    stream.extend(b"W: low battery\n");
    stream.extend(f.encode(&[0x01]));
    let mut ev = Vec::new();
    f.push(&stream, &mut ev);
    assert_eq!(
        ev,
        vec![
            FrameEvent::Text("I: boot ok".into()),
            FrameEvent::Frame(vec![0x08, 0x0A, 0x10, 0x0A]),
            FrameEvent::Text("W: low battery".into()),
            FrameEvent::Frame(vec![0x01]),
        ]
    );
}

#[test]
fn text_lines_around_sync_frames() {
    let mut c = cfg(FramingType::FixedLen, CrcKind::Crc16CcittFalse);
    c.sync_word = vec![0xAA, 0x55];
    c.text_passthrough = true;
    let mut f = build_framer(&c);
    let mut stream = b"[INFO] hello\n".to_vec();
    stream.extend(f.encode(b"abc"));
    stream.extend(b"E: oops\n");
    let mut ev = Vec::new();
    f.push(&stream, &mut ev);
    assert_eq!(
        ev,
        vec![
            FrameEvent::Text("[INFO] hello".into()),
            FrameEvent::Frame(b"abc".to_vec()),
            FrameEvent::Text("E: oops".into()),
        ]
    );
}

#[test]
fn crc_error_and_overflow_are_reported() {
    let c = cfg(FramingType::Cobs, CrcKind::Crc16CcittFalse);
    let mut f = build_framer(&c);
    let mut bad = f.encode(b"hello");
    bad[2] ^= 0x01;
    let mut ev = Vec::new();
    f.push(&bad, &mut ev);
    assert!(matches!(
        ev[0],
        FrameEvent::Error {
            kind: FrameErrorKind::Crc,
            ..
        }
    ));
    let mut ev = Vec::new();
    f.push(&vec![0x11; 2000], &mut ev);
    f.push(&[0x00], &mut ev);
    assert!(
        matches!(
            ev[0],
            FrameEvent::Error {
                kind: FrameErrorKind::Framing,
                ..
            }
        ),
        "{ev:?}"
    );
    // Next frame still decodes.
    let good = f.encode(b"ok");
    let mut ev = Vec::new();
    f.push(&good, &mut ev);
    assert_eq!(frames(&ev), vec![b"ok".to_vec()]);
}

#[test]
fn length_framer_rejects_huge_lengths_without_allocating() {
    let mut c = cfg(FramingType::FixedLen, CrcKind::Crc16CcittFalse);
    c.len_bytes = 4;
    let mut f = build_framer(&c);
    let mut ev = Vec::new();
    f.push(&[0xFF, 0xFF, 0xFF, 0x7F, 0x01], &mut ev);
    assert!(ev.iter().any(|e| matches!(
        e,
        FrameEvent::Error {
            kind: FrameErrorKind::Framing,
            ..
        }
    )));
    let good = f.encode(b"x");
    f.push(&good, &mut ev);
    assert_eq!(frames(&ev), vec![b"x".to_vec()]);
}

#[test]
fn unframed_each_push_is_a_frame() {
    let c = cfg(FramingType::None, CrcKind::Crc32);
    let mut f = build_framer(&c);
    let a = f.encode(b"one");
    let mut ev = Vec::new();
    f.push(&a, &mut ev);
    f.push(&a[..2], &mut ev);
    assert_eq!(frames(&ev), vec![b"one".to_vec()]);
    assert!(matches!(
        ev[1],
        FrameEvent::Error {
            kind: FrameErrorKind::Crc,
            ..
        }
    ));
}

#[test]
fn documented_hex_examples() {
    // docs/FIRMWARE_INTEGRATION.md, "Worked examples".
    let cobs = build_framer(&FramingConfig {
        kind: FramingType::Cobs,
        crc: CrcKind::Crc16CcittFalse,
        ..Default::default()
    });
    let hex = |b: &[u8]| crate::util::hex(b);
    assert_eq!(
        hex(&cobs.encode(&[0x08, 0x01, 0x12, 0x00])),
        "04 08 01 12 03 22 53 00"
    );
    assert_eq!(
        hex(&cobs.encode(&[0x08, 0x02, 0x1A, 0x03, 0x08, 0xC8, 0x01])),
        "0A 08 02 1A 03 08 C8 01 76 16 00"
    );
    assert_eq!(
        hex(&cobs.encode(&[0x08, 0x02, 0x1A, 0x02, 0x08, 0x01])),
        "09 08 02 1A 02 08 01 35 C3 00"
    );
    assert_eq!(
        hex(&cobs.encode(&[0x01, 0x10, 0xAC, 0x02])),
        "07 01 10 AC 02 46 C9 00"
    );
    let fixed = build_framer(&FramingConfig {
        kind: FramingType::FixedLen,
        len_bytes: 2,
        sync_word: vec![0xAA, 0x55],
        crc: CrcKind::Crc16CcittFalse,
        ..Default::default()
    });
    assert_eq!(
        hex(&fixed.encode(&[0x08, 0x01, 0x12, 0x00])),
        "AA 55 06 00 08 01 12 00 22 53"
    );
    let slip = build_framer(&FramingConfig {
        kind: FramingType::Slip,
        crc: CrcKind::None,
        ..Default::default()
    });
    assert_eq!(
        hex(&slip.encode(&[0x01, 0xC0, 0xDB])),
        "C0 01 DB DC DB DD C0"
    );
    let varint = build_framer(&FramingConfig {
        kind: FramingType::VarintLen,
        crc: CrcKind::Crc8,
        ..Default::default()
    });
    let crc8 = Crc::new(CrcKind::Crc8, Endian::Little).compute(&[0x08, 0x01, 0x12, 0x00]) as u8;
    assert_eq!(
        varint.encode(&[0x08, 0x01, 0x12, 0x00]),
        vec![0x05, 0x08, 0x01, 0x12, 0x00, crc8]
    );
}
