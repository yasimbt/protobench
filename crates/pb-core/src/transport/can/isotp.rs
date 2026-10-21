//! ISO-TP (ISO 15765-2) segmentation: messages longer than one CAN frame.
//!
//! ```text
//! single frame       0x0L data…              (L = length, up to 7)
//!                    0x00 LL data…           (CAN FD, length 8–62)
//! first frame        0x1L LL data…           (12-bit length, up to 4095)
//!                    0x10 00 LLLLLLLL data…  (32-bit length)
//! consecutive frame  0x2N data…              (N = sequence number 1–15, 0, 1, …)
//! flow control       0x3S BS ST              (S: 0 continue, 1 wait, 2 overflow;
//!                                             BS block size; ST minimum separation)
//! ```
//!
//! [`Receiver`] reassembles incoming frames and says when to send flow
//! control; the sending side ([`single_frame`], [`first_frame`],
//! [`consecutive_frame`], [`FlowControl`]) is driven by the transport,
//! which waits for flow control on the bus.

use std::time::{Duration, Instant};

use super::frame::fd_len;

/// Segmentation settings for one direction pair.
#[derive(Debug, Clone, PartialEq)]
pub struct IsoTpParams {
    /// Frame data length: 8 for classic CAN, 64 for CAN FD.
    pub frame_len: usize,
    /// Pad frames to their full length with this byte (FD frames longer
    /// than 8 bytes are always padded to a valid length, with 0xCC if None).
    pub padding: Option<u8>,
    /// Block size asked for when receiving (0 = no further flow control).
    pub block_size: u8,
    /// Minimum separation asked for when receiving (the raw STmin byte).
    pub st_min: u8,
    /// N_Bs and N_Cr: how long to wait for flow control or the next
    /// consecutive frame.
    pub timeout: Duration,
    /// Largest message accepted.
    pub max_len: usize,
}

impl Default for IsoTpParams {
    fn default() -> Self {
        IsoTpParams {
            frame_len: 8,
            padding: None,
            block_size: 0,
            st_min: 0,
            timeout: Duration::from_millis(1000),
            max_len: 4095,
        }
    }
}

/// Encodes a separation time in µs as the STmin byte (rounded up).
pub fn st_min_from_us(us: u32) -> u8 {
    match us {
        0 => 0,
        1..=900 => 0xF0 + us.div_ceil(100) as u8,
        _ => us.div_ceil(1000).min(127) as u8,
    }
}

/// Decodes an STmin byte; reserved values mean the maximum, 127 ms.
pub fn st_min_duration(b: u8) -> Duration {
    match b {
        0x00..=0x7F => Duration::from_millis(u64::from(b)),
        0xF1..=0xF9 => Duration::from_micros(u64::from(b - 0xF0) * 100),
        _ => Duration::from_millis(127),
    }
}

/// Pads a frame per `params`: to the full classic length when padding is
/// set, and always to a valid CAN FD length above 8 bytes.
fn finish(mut f: Vec<u8>, p: &IsoTpParams) -> Vec<u8> {
    let target = match p.padding {
        Some(_) if p.frame_len <= 8 => 8,
        Some(_) => fd_len(f.len()).max(8),
        None if f.len() > 8 => fd_len(f.len()),
        None => f.len(),
    };
    f.resize(target, p.padding.unwrap_or(0xCC));
    f
}

/// The single frame for `data`, if it fits in one.
pub fn single_frame(data: &[u8], p: &IsoTpParams) -> Option<Vec<u8>> {
    if data.len() <= 7 {
        let mut f = vec![data.len() as u8];
        f.extend_from_slice(data);
        return Some(finish(f, p));
    }
    if p.frame_len > 8 && data.len() <= p.frame_len - 2 {
        let mut f = vec![0x00, data.len() as u8];
        f.extend_from_slice(data);
        return Some(finish(f, p));
    }
    None
}

/// The first frame of a multi-frame message and how many data bytes it
/// carries.
pub fn first_frame(data: &[u8], p: &IsoTpParams) -> (Vec<u8>, usize) {
    let mut f = if data.len() <= 4095 {
        vec![0x10 | (data.len() >> 8) as u8, data.len() as u8]
    } else {
        let mut f = vec![0x10, 0x00];
        f.extend_from_slice(&(data.len() as u32).to_be_bytes());
        f
    };
    let n = (p.frame_len - f.len()).min(data.len());
    f.extend_from_slice(&data[..n]);
    (finish(f, p), n)
}

/// A consecutive frame with sequence number `sn` and the next bytes of
/// `rest`; returns it and how many bytes it carries.
pub fn consecutive_frame(sn: u8, rest: &[u8], p: &IsoTpParams) -> (Vec<u8>, usize) {
    let n = (p.frame_len - 1).min(rest.len());
    let mut f = vec![0x20 | (sn & 0x0F)];
    f.extend_from_slice(&rest[..n]);
    (finish(f, p), n)
}

/// A flow-control frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowControl {
    /// Continue to send `block_size` frames (0 = all), `st_min` apart.
    Continue {
        /// Frames before the next flow control (0 = no limit).
        block_size: u8,
        /// Raw STmin byte.
        st_min: u8,
    },
    /// Wait for another flow-control frame.
    Wait,
    /// The receiver cannot take a message this long.
    Overflow,
}

impl FlowControl {
    /// Parses a flow-control frame.
    pub fn parse(f: &[u8]) -> Option<FlowControl> {
        if f.first().map(|b| b >> 4) != Some(3) || f.len() < 3 {
            return None;
        }
        Some(match f[0] & 0x0F {
            0 => FlowControl::Continue {
                block_size: f[1],
                st_min: f[2],
            },
            1 => FlowControl::Wait,
            _ => FlowControl::Overflow,
        })
    }

    /// Encodes it.
    pub fn encode(&self, p: &IsoTpParams) -> Vec<u8> {
        let f = match *self {
            FlowControl::Continue { block_size, st_min } => vec![0x30, block_size, st_min],
            FlowControl::Wait => vec![0x31, 0, 0],
            FlowControl::Overflow => vec![0x32, 0, 0],
        };
        finish(f, p)
    }
}

/// What the receiver wants done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RxEvent {
    /// A complete message.
    Message(Vec<u8>),
    /// Send this flow-control frame.
    Send(Vec<u8>),
    /// A reception was abandoned.
    Error(String),
}

struct Rx {
    total: usize,
    data: Vec<u8>,
    next_sn: u8,
    in_block: u8,
    deadline: Instant,
}

/// Reassembles incoming ISO-TP frames.
pub struct Receiver {
    p: IsoTpParams,
    rx: Option<Rx>,
}

impl Receiver {
    /// A receiver with `params`.
    pub fn new(p: IsoTpParams) -> Receiver {
        Receiver { p, rx: None }
    }

    /// Handles one frame's data (flow-control frames are not for the
    /// receiver and are ignored).
    pub fn on_frame(&mut self, f: &[u8], now: Instant) -> Vec<RxEvent> {
        let mut out = Vec::new();
        let Some(&pci) = f.first() else {
            return out;
        };
        match pci >> 4 {
            0 => {
                let (len, start) = if pci & 0x0F != 0 {
                    (usize::from(pci & 0x0F), 1)
                } else if f.len() > 8 && f.len() >= 2 {
                    (usize::from(f[1]), 2)
                } else {
                    return out; // empty single frame
                };
                if self.rx.take().is_some() {
                    out.push(RxEvent::Error(
                        "single frame in the middle of a message".into(),
                    ));
                }
                match f.get(start..start + len) {
                    Some(d) => out.push(RxEvent::Message(d.to_vec())),
                    None => out.push(RxEvent::Error(
                        "single frame shorter than its length".into(),
                    )),
                }
            }
            1 => {
                let (total, start) = match ((usize::from(pci & 0x0F)) << 8)
                    | usize::from(*f.get(1).unwrap_or(&0))
                {
                    0 if f.len() >= 6 => (u32::from_be_bytes([f[2], f[3], f[4], f[5]]) as usize, 6),
                    0 => return out,
                    n => (n, 2),
                };
                if self.rx.take().is_some() {
                    out.push(RxEvent::Error(
                        "new first frame in the middle of a message".into(),
                    ));
                }
                if total > self.p.max_len {
                    out.push(RxEvent::Send(FlowControl::Overflow.encode(&self.p)));
                    out.push(RxEvent::Error(format!(
                        "message of {total} bytes exceeds the limit of {}",
                        self.p.max_len
                    )));
                    return out;
                }
                let mut data = Vec::with_capacity(total);
                data.extend_from_slice(&f[start.min(f.len())..f.len().min(start + total)]);
                self.rx = Some(Rx {
                    total,
                    data,
                    next_sn: 1,
                    in_block: 0,
                    deadline: now + self.p.timeout,
                });
                out.push(RxEvent::Send(
                    FlowControl::Continue {
                        block_size: self.p.block_size,
                        st_min: self.p.st_min,
                    }
                    .encode(&self.p),
                ));
            }
            2 => {
                let Some(rx) = self.rx.as_mut() else {
                    return out; // not for a message we follow
                };
                if pci & 0x0F != rx.next_sn {
                    let exp = rx.next_sn;
                    self.rx = None;
                    out.push(RxEvent::Error(format!(
                        "consecutive frame {} where {exp} was due",
                        pci & 0x0F
                    )));
                    return out;
                }
                let want = rx.total - rx.data.len();
                rx.data.extend_from_slice(&f[1..f.len().min(1 + want)]);
                rx.next_sn = (rx.next_sn + 1) & 0x0F;
                rx.deadline = now + self.p.timeout;
                if rx.data.len() >= rx.total {
                    let rx = self.rx.take().expect("receiving");
                    out.push(RxEvent::Message(rx.data));
                    return out;
                }
                // Block size 0: no further flow control, nothing to count.
                if self.p.block_size == 0 {
                    return out;
                }
                rx.in_block += 1;
                if rx.in_block == self.p.block_size {
                    rx.in_block = 0;
                    out.push(RxEvent::Send(
                        FlowControl::Continue {
                            block_size: self.p.block_size,
                            st_min: self.p.st_min,
                        }
                        .encode(&self.p),
                    ));
                }
            }
            _ => {}
        }
        out
    }

    /// Abandons a reception whose next consecutive frame is overdue.
    pub fn poll(&mut self, now: Instant) -> Option<RxEvent> {
        if self.rx.as_ref().is_some_and(|r| now > r.deadline) {
            let rx = self.rx.take()?;
            return Some(RxEvent::Error(format!(
                "timed out after {} of {} bytes (no consecutive frame within {:?})",
                rx.data.len(),
                rx.total,
                self.p.timeout
            )));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(frame_len: usize) -> IsoTpParams {
        IsoTpParams {
            frame_len,
            max_len: 1 << 20,
            ..Default::default()
        }
    }

    #[test]
    fn frame_layouts() {
        let c = p(8);
        assert_eq!(single_frame(b"abc", &c).unwrap(), b"\x03abc");
        assert!(single_frame(&[0; 8], &c).is_none());
        let fd = p(64);
        let sf = single_frame(&[7; 20], &fd).unwrap();
        assert_eq!(&sf[..2], &[0x00, 20]);
        assert_eq!(sf.len(), 24, "padded to a valid FD length");
        let (ff, n) = first_frame(&[1; 100], &c);
        assert_eq!((&ff[..2], n), (&[0x10, 100][..], 6));
        let (ff, n) = first_frame(&vec![1; 5000], &c);
        assert_eq!(&ff[..6], &[0x10, 0, 0, 0, 0x13, 0x88]);
        assert_eq!(n, 2);
        let (cf, n) = consecutive_frame(15, &[9; 3], &c);
        assert_eq!((cf, n), (vec![0x2F, 9, 9, 9], 3));
        let padded = IsoTpParams {
            padding: Some(0xAA),
            ..c.clone()
        };
        assert_eq!(
            single_frame(b"a", &padded).unwrap(),
            b"\x01a\xAA\xAA\xAA\xAA\xAA\xAA"
        );
        assert_eq!(
            FlowControl::parse(
                &FlowControl::Continue {
                    block_size: 4,
                    st_min: 0xF3
                }
                .encode(&c)
            ),
            Some(FlowControl::Continue {
                block_size: 4,
                st_min: 0xF3
            })
        );
        assert_eq!(st_min_from_us(250), 0xF3);
        assert_eq!(st_min_duration(0xF3), Duration::from_micros(300));
        assert_eq!(st_min_from_us(5000), 5);
        assert_eq!(st_min_duration(0x90), Duration::from_millis(127));
    }

    #[test]
    fn receiver_reassembles_and_checks_sequence() {
        let c = IsoTpParams {
            block_size: 2,
            ..p(8)
        };
        let mut r = Receiver::new(c.clone());
        let now = Instant::now();
        let msg: Vec<u8> = (0..20).collect();
        let (ff, mut off) = first_frame(&msg, &c);
        let ev = r.on_frame(&ff, now);
        assert_eq!(ev.len(), 1);
        assert!(matches!(&ev[0], RxEvent::Send(f) if f[..3] == [0x30, 2, 0]));
        let mut sn = 1;
        let mut got = None;
        while got.is_none() {
            let (cf, n) = consecutive_frame(sn, &msg[off..], &c);
            off += n;
            sn = (sn + 1) & 0x0F;
            for e in r.on_frame(&cf, now) {
                if let RxEvent::Message(m) = e {
                    got = Some(m);
                }
            }
        }
        assert_eq!(got.unwrap(), msg);
        // A skipped sequence number abandons the message.
        r.on_frame(&first_frame(&msg, &c).0, now);
        let ev = r.on_frame(&consecutive_frame(2, &msg[6..], &c).0, now);
        assert!(matches!(&ev[0], RxEvent::Error(e) if e.contains("where 1 was due")));
        // An overdue consecutive frame times out.
        r.on_frame(&first_frame(&msg, &c).0, now);
        assert!(r.poll(now).is_none());
        assert!(matches!(
            r.poll(now + Duration::from_secs(2)),
            Some(RxEvent::Error(_))
        ));
        // Too long: overflow.
        let small = IsoTpParams { max_len: 10, ..c };
        let mut r = Receiver::new(small.clone());
        let ev = r.on_frame(&first_frame(&msg, &small).0, now);
        assert!(matches!(&ev[0], RxEvent::Send(f) if f[0] == 0x32));
    }
}
