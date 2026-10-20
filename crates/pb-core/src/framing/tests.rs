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

fn texts(events: &[FrameEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            FrameEvent::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

/// A frame whose COBS code byte is printable and whose payload starts with
/// tag 0x0A (field 1, length-delimited) used to be split into a one-character
/// text line and a framing error.
#[test]
fn frame_starting_with_newline_tag_is_not_text() {
    let mut payload = vec![0x0A, 40];
    payload.extend((0..40).map(|i| b'a' + (i % 26) as u8));
    for kind in [FramingType::Cobs, FramingType::Slip] {
        for crc in [CrcKind::None, CrcKind::Crc16CcittFalse] {
            let mut c = cfg(kind, crc);
            c.text_passthrough = true;
            let mut f = build_framer(&c);
            let wire = f.encode(&payload);
            if kind == FramingType::Cobs {
                assert!((0x21..0x7F).contains(&wire[0]), "code byte {:#x}", wire[0]);
            }
            let mut ev = Vec::new();
            f.push(&wire, &mut ev);
            f.idle(&mut ev);
            assert_eq!(ev, vec![FrameEvent::Frame(payload.clone())], "{c:?}");
        }
    }
}

#[test]
fn text_without_frames_is_flushed_when_idle() {
    for kind in [FramingType::Cobs, FramingType::Slip] {
        let mut c = cfg(kind, CrcKind::Crc16CcittFalse);
        c.text_passthrough = true;
        let mut f = build_framer(&c);
        let mut ev = Vec::new();
        f.push(b"I: boot ok\r\nW: lo", &mut ev);
        assert!(ev.is_empty(), "held until idle or a delimiter");
        f.idle(&mut ev);
        assert_eq!(texts(&ev), vec!["I: boot ok".to_string()]);
        // The partial line stays and completes later.
        f.push(b"w battery\n", &mut ev);
        f.idle(&mut ev);
        assert_eq!(
            texts(&ev),
            vec!["I: boot ok".to_string(), "W: low battery".to_string()]
        );
    }
}

#[test]
fn decoded_size_limit_is_max_frame_len_plus_crc() {
    let mut c = cfg(FramingType::Cobs, CrcKind::Crc16CcittFalse);
    c.max_frame_len = 300;
    let enc = build_framer(&c);
    let ok = vec![0x11u8; 300];
    let mut too_big = enc.encode(&vec![0x11u8; 301]);
    too_big.extend(enc.encode(&ok));
    let mut f = build_framer(&c);
    let mut ev = Vec::new();
    f.push(&too_big, &mut ev);
    assert!(
        matches!(&ev[0], FrameEvent::Error { reason, .. } if reason.contains("max_frame_len")),
        "{ev:?}"
    );
    assert_eq!(frames(&ev), vec![ok]);
}

fn log_line() -> impl Strategy<Value = String> {
    ("[!-~]", "[ -~]{0,60}").prop_map(|(a, b)| format!("{a}{b}").trim_end().to_string())
}

#[derive(Debug, Clone)]
enum Item {
    Frame(Vec<u8>),
    Line(String),
}

fn items() -> impl Strategy<Value = Vec<Item>> {
    prop::collection::vec(
        prop_oneof![
            prop::collection::vec(any::<u8>(), 1..200).prop_map(Item::Frame),
            log_line().prop_map(Item::Line),
        ],
        1..12,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// With text passthrough on, random frames are never lost or split, at
    /// any read boundary.
    #[test]
    fn text_passthrough_never_loses_frames(
        ps in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..300), 1..8),
        cut in any::<prop::sample::Index>(),
    ) {
        for kind in [FramingType::Cobs, FramingType::Slip] {
            for crc in [CrcKind::Crc16CcittFalse, CrcKind::Crc32] {
                let mut c = cfg(kind, crc);
                c.text_passthrough = true;
                let enc = build_framer(&c);
                let stream: Vec<u8> = ps.iter().flat_map(|p| enc.encode(p)).collect();
                let k = cut.index(stream.len() + 1);
                let mut f = build_framer(&c);
                let mut ev = Vec::new();
                f.push(&stream[..k], &mut ev);
                f.push(&stream[k..], &mut ev);
                f.idle(&mut ev);
                prop_assert_eq!(&ev, &ps.iter().cloned().map(FrameEvent::Frame).collect::<Vec<_>>(), "{:?}", c);
            }
        }
    }

    /// Log lines and frames interleaved on one link come out in order.
    #[test]
    fn text_and_frames_interleaved(items in items(), cut in any::<prop::sample::Index>()) {
        for kind in [FramingType::Cobs, FramingType::Slip] {
            let mut c = cfg(kind, CrcKind::Crc32);
            c.text_passthrough = true;
            let enc = build_framer(&c);
            let mut stream = Vec::new();
            let mut want = Vec::new();
            for it in &items {
                match it {
                    Item::Frame(p) => {
                        stream.extend(enc.encode(p));
                        want.push(FrameEvent::Frame(p.clone()));
                    }
                    Item::Line(l) => {
                        stream.extend(l.as_bytes());
                        stream.push(b'\n');
                        want.push(FrameEvent::Text(l.clone()));
                    }
                }
            }
            let k = cut.index(stream.len() + 1);
            let mut f = build_framer(&c);
            let mut ev = Vec::new();
            f.push(&stream[..k], &mut ev);
            f.push(&stream[k..], &mut ev);
            f.idle(&mut ev);
            prop_assert_eq!(&ev, &want, "{:?}", c);
        }
    }
}
