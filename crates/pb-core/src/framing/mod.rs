//! Framing: turning a byte stream into frames and back.
//!
//! Protobuf is not self-delimiting, so every frame is wrapped by a framer:
//! COBS (0x00 delimiter), SLIP, a varint or fixed-width length prefix
//! (optionally after a sync word), or none (one frame per transport read).
//! An optional CRC is appended to the payload before framing:
//!
//! ```text
//! cobs:       COBS(payload || crc) || 0x00
//! slip:       END || SLIP(payload || crc) || END
//! varint_len: [sync] || varint(len) || payload || crc     (len = payload + crc)
//! fixed_len:  [sync] || uN(len)     || payload || crc
//! ```
//!
//! Decoders resynchronise after garbage and, when `text_passthrough` is on,
//! hand printable lines found between frames to the text console.

pub mod cobs;
mod crc;
pub mod slip;

pub use self::crc::Crc;

use crate::codec::write_varint;
use crate::config::{Endian, FramingConfig, FramingType};
use crate::schema::wire::read_varint;

/// Kinds of framing failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameErrorKind {
    /// Malformed encoding, bad length or overflow.
    Framing,
    /// CRC mismatch.
    Crc,
}

/// Output of a framer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameEvent {
    /// A good frame: the payload with the CRC removed.
    Frame(Vec<u8>),
    /// A printable line outside frames (without the line ending).
    Text(String),
    /// A bad frame with up to [`BAD_FRAME_KEEP`] of its bytes.
    Error {
        /// What went wrong.
        kind: FrameErrorKind,
        /// Short explanation.
        reason: String,
        /// The offending bytes (truncated).
        bytes: Vec<u8>,
    },
}

/// Bytes of a bad frame kept for inspection.
pub const BAD_FRAME_KEEP: usize = 256;

/// Encoder and incremental decoder for one framing scheme.
pub trait Framer: Send {
    /// Wraps a payload (CRC is added here).
    fn encode(&self, payload: &[u8]) -> Vec<u8>;
    /// Feeds received bytes; complete frames, text lines and errors are
    /// appended to `out`. Bytes may arrive split at any boundary.
    fn push(&mut self, data: &[u8], out: &mut Vec<FrameEvent>);
    /// Drops any partial state (e.g. after reconnecting).
    fn reset(&mut self);
}

/// Builds the framer for a config.
pub fn build_framer(cfg: &FramingConfig) -> Box<dyn Framer> {
    let crc = Crc::new(cfg.crc, cfg.crc_endian);
    let max = cfg.max_frame_len.max(1);
    match cfg.kind {
        FramingType::Cobs => Box::new(Delimited::new(
            Stuffing::Cobs,
            crc,
            max,
            cfg.text_passthrough,
        )),
        FramingType::Slip => Box::new(Delimited::new(
            Stuffing::Slip,
            crc,
            max,
            cfg.text_passthrough,
        )),
        FramingType::VarintLen => Box::new(LengthPrefixed::new(
            LenFormat::Varint,
            cfg.sync_word.clone(),
            crc,
            max,
            cfg.text_passthrough,
        )),
        FramingType::FixedLen => {
            let fmt = match cfg.len_bytes {
                1 => LenFormat::U8,
                4 => LenFormat::U32(cfg.endian),
                _ => LenFormat::U16(cfg.endian),
            };
            Box::new(LengthPrefixed::new(
                fmt,
                cfg.sync_word.clone(),
                crc,
                max,
                cfg.text_passthrough,
            ))
        }
        FramingType::None => Box::new(Unframed { crc, max }),
    }
}

fn truncate(b: &[u8]) -> Vec<u8> {
    b[..b.len().min(BAD_FRAME_KEEP)].to_vec()
}

fn crc_event(crc: &Crc, frame: &[u8], out: &mut Vec<FrameEvent>) -> bool {
    match crc.check(frame) {
        Ok(p) => {
            out.push(FrameEvent::Frame(p.to_vec()));
            true
        }
        Err((exp, found)) => {
            out.push(FrameEvent::Error {
                kind: FrameErrorKind::Crc,
                reason: if frame.len() < crc.len() {
                    "frame shorter than CRC".into()
                } else {
                    format!("CRC mismatch: expected {exp:#x}, found {found:#x}")
                },
                bytes: truncate(frame),
            });
            false
        }
    }
}

/// Collects printable text between frames.
#[derive(Debug, Default)]
struct TextLine {
    buf: Vec<u8>,
}

/// True for bytes allowed in a text line.
fn is_text_byte(b: u8) -> bool {
    (0x20..0x7F).contains(&b) || b == b'\t' || b == b'\r'
}

/// True if `buf` looks like one line of firmware log text.
fn looks_like_text(buf: &[u8]) -> bool {
    match buf.first() {
        Some(&f) if (0x21..0x7F).contains(&f) => buf.iter().all(|&b| is_text_byte(b)),
        _ => false,
    }
}

fn line_string(buf: &[u8]) -> String {
    String::from_utf8_lossy(buf)
        .trim_end_matches('\r')
        .to_string()
}

impl TextLine {
    /// Feeds one byte; returns a completed line.
    fn push(&mut self, b: u8, max: usize) -> Option<Result<String, Vec<u8>>> {
        if b == b'\n' {
            let line = std::mem::take(&mut self.buf);
            if line.is_empty() {
                return None;
            }
            return Some(if looks_like_text(&line) {
                Ok(line_string(&line))
            } else {
                Err(line)
            });
        }
        self.buf.push(b);
        if self.buf.len() > max {
            return Some(Err(std::mem::take(&mut self.buf)));
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stuffing {
    Cobs,
    Slip,
}

/// COBS and SLIP: frames end at a delimiter byte.
struct Delimited {
    stuffing: Stuffing,
    crc: Crc,
    max_encoded: usize,
    text: bool,
    buf: Vec<u8>,
    overflow: bool,
}

impl Delimited {
    fn new(stuffing: Stuffing, crc: Crc, max: usize, text: bool) -> Self {
        let max_encoded = match stuffing {
            Stuffing::Cobs => cobs::max_encoded_len(max + crc.len()),
            Stuffing::Slip => 2 * (max + crc.len()),
        };
        Self {
            stuffing,
            crc,
            max_encoded,
            text,
            buf: Vec::new(),
            overflow: false,
        }
    }

    fn delimiter(&self) -> u8 {
        match self.stuffing {
            Stuffing::Cobs => 0x00,
            Stuffing::Slip => slip::END,
        }
    }

    fn finish(&mut self, out: &mut Vec<FrameEvent>) {
        if self.overflow {
            out.push(FrameEvent::Error {
                kind: FrameErrorKind::Framing,
                reason: "frame exceeds max_frame_len".into(),
                bytes: truncate(&self.buf),
            });
        } else if !self.buf.is_empty() {
            let decoded = match self.stuffing {
                Stuffing::Cobs => cobs::decode(&self.buf),
                Stuffing::Slip => slip::decode(&self.buf),
            };
            match decoded {
                Ok(frame) => {
                    if frame.len() > self.max_encoded {
                        out.push(FrameEvent::Error {
                            kind: FrameErrorKind::Framing,
                            reason: "frame exceeds max_frame_len".into(),
                            bytes: truncate(&frame),
                        });
                    } else {
                        crc_event(&self.crc, &frame, out);
                    }
                }
                Err(reason) => out.push(FrameEvent::Error {
                    kind: FrameErrorKind::Framing,
                    reason: reason.into(),
                    bytes: truncate(&self.buf),
                }),
            }
        }
        self.buf.clear();
        self.overflow = false;
    }
}

impl Framer for Delimited {
    fn encode(&self, payload: &[u8]) -> Vec<u8> {
        let mut body = payload.to_vec();
        self.crc.append(&mut body, 0);
        let mut out = Vec::with_capacity(body.len() + body.len() / 128 + 4);
        match self.stuffing {
            Stuffing::Cobs => {
                cobs::encode(&body, &mut out);
                out.push(0x00);
            }
            Stuffing::Slip => slip::encode(&body, &mut out),
        }
        out
    }

    fn push(&mut self, data: &[u8], out: &mut Vec<FrameEvent>) {
        let delim = self.delimiter();
        for &b in data {
            if b == delim {
                self.finish(out);
            } else if self.text && b == b'\n' && !self.overflow && looks_like_text(&self.buf) {
                out.push(FrameEvent::Text(line_string(&self.buf)));
                self.buf.clear();
            } else if self.overflow {
                // Drop bytes until the next delimiter.
                continue;
            } else {
                self.buf.push(b);
                if self.buf.len() > self.max_encoded {
                    self.overflow = true;
                }
            }
        }
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.overflow = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LenFormat {
    Varint,
    U8,
    U16(Endian),
    U32(Endian),
}

impl LenFormat {
    fn write(self, out: &mut Vec<u8>, len: usize) {
        match self {
            LenFormat::Varint => write_varint(out, len as u64),
            LenFormat::U8 => out.push(len as u8),
            LenFormat::U16(Endian::Little) => out.extend_from_slice(&(len as u16).to_le_bytes()),
            LenFormat::U16(Endian::Big) => out.extend_from_slice(&(len as u16).to_be_bytes()),
            LenFormat::U32(Endian::Little) => out.extend_from_slice(&(len as u32).to_le_bytes()),
            LenFormat::U32(Endian::Big) => out.extend_from_slice(&(len as u32).to_be_bytes()),
        }
    }

    /// Parses a length at the start of `b`: Ok(Some((len, header_bytes))),
    /// Ok(None) if more bytes are needed, Err if malformed.
    fn read(self, b: &[u8]) -> Result<Option<(usize, usize)>, ()> {
        let need = |n: usize| b.len() >= n;
        Ok(match self {
            LenFormat::Varint => match read_varint(b, 0) {
                Some((v, n)) => Some((usize::try_from(v).map_err(|_| ())?, n)),
                None if b.len() >= 10 => return Err(()),
                None => None,
            },
            LenFormat::U8 => b.first().map(|&x| (x as usize, 1)),
            LenFormat::U16(e) => need(2).then(|| {
                let a = [b[0], b[1]];
                let v = match e {
                    Endian::Little => u16::from_le_bytes(a),
                    Endian::Big => u16::from_be_bytes(a),
                };
                (v as usize, 2)
            }),
            LenFormat::U32(e) => need(4).then(|| {
                let a = [b[0], b[1], b[2], b[3]];
                let v = match e {
                    Endian::Little => u32::from_le_bytes(a),
                    Endian::Big => u32::from_be_bytes(a),
                };
                (v as usize, 4)
            }),
        })
    }
}

/// Length-prefixed frames, optionally after a sync word.
struct LengthPrefixed {
    fmt: LenFormat,
    sync: Vec<u8>,
    crc: Crc,
    max: usize,
    text: bool,
    buf: Vec<u8>,
    textline: TextLine,
    /// Suppresses repeated errors while sliding to find the next frame.
    resyncing: bool,
    garbage: Vec<u8>,
}

impl LengthPrefixed {
    fn new(fmt: LenFormat, sync: Vec<u8>, crc: Crc, max: usize, text: bool) -> Self {
        Self {
            fmt,
            sync,
            crc,
            max,
            text,
            buf: Vec::new(),
            textline: TextLine::default(),
            resyncing: false,
            garbage: Vec::new(),
        }
    }

    fn error(
        &mut self,
        kind: FrameErrorKind,
        reason: &str,
        bytes: &[u8],
        out: &mut Vec<FrameEvent>,
    ) {
        if !self.resyncing {
            out.push(FrameEvent::Error {
                kind,
                reason: reason.into(),
                bytes: truncate(bytes),
            });
        }
        self.resyncing = true;
    }

    /// Handles bytes skipped while hunting for the sync word.
    fn skipped(&mut self, bytes: &[u8], out: &mut Vec<FrameEvent>) {
        for &b in bytes {
            if self.text {
                match self.textline.push(b, self.max) {
                    Some(Ok(line)) => out.push(FrameEvent::Text(line)),
                    Some(Err(junk)) => self.garbage.extend_from_slice(&junk),
                    None => {}
                }
            } else {
                self.garbage.push(b);
            }
        }
        self.flush_garbage(out);
    }

    fn flush_garbage(&mut self, out: &mut Vec<FrameEvent>) {
        if !self.garbage.is_empty() {
            let g = std::mem::take(&mut self.garbage);
            self.error(FrameErrorKind::Framing, "bytes outside a frame", &g, out);
        }
    }

    fn drain_sync(&mut self, out: &mut Vec<FrameEvent>) -> bool {
        if self.sync.is_empty() {
            return true;
        }
        let n = self.sync.len();
        if let Some(pos) = self.buf.windows(n).position(|w| w == self.sync.as_slice()) {
            if pos > 0 {
                let skipped: Vec<u8> = self.buf.drain(..pos).collect();
                self.skipped(&skipped, out);
            }
            true
        } else {
            // Keep the longest suffix that could start the sync word.
            let len = self.buf.len();
            let keep = (1..n.min(len + 1))
                .rev()
                .find(|&k| self.buf[len - k..] == self.sync[..k])
                .unwrap_or(0);
            let cut_at = len - keep;
            let skipped: Vec<u8> = self.buf.drain(..cut_at).collect();
            self.skipped(&skipped, out);
            false
        }
    }
}

impl Framer for LengthPrefixed {
    fn encode(&self, payload: &[u8]) -> Vec<u8> {
        let mut body = payload.to_vec();
        self.crc.append(&mut body, 0);
        let mut out = Vec::with_capacity(body.len() + self.sync.len() + 5);
        out.extend_from_slice(&self.sync);
        self.fmt.write(&mut out, body.len());
        out.extend_from_slice(&body);
        out
    }

    fn push(&mut self, data: &[u8], out: &mut Vec<FrameEvent>) {
        self.buf.extend_from_slice(data);
        loop {
            if !self.drain_sync(out) {
                return;
            }
            let s = self.sync.len();
            let (len, hdr) = match self.fmt.read(&self.buf[s..]) {
                Ok(Some(x)) => x,
                Ok(None) => return,
                Err(()) => {
                    let b = self.buf[..s + 1].to_vec();
                    self.error(FrameErrorKind::Framing, "malformed length prefix", &b, out);
                    self.buf.drain(..1);
                    continue;
                }
            };
            let total = len;
            if total == 0 || total > self.max + self.crc.len() || total < self.crc.len() {
                let b = self.buf[..(s + hdr).min(self.buf.len())].to_vec();
                self.error(
                    FrameErrorKind::Framing,
                    &format!("bad frame length {total}"),
                    &b,
                    out,
                );
                self.buf.drain(..1);
                continue;
            }
            let start = s + hdr;
            if self.buf.len() < start + total {
                return;
            }
            let frame = self.buf[start..start + total].to_vec();
            match self.crc.check(&frame) {
                Ok(p) => {
                    out.push(FrameEvent::Frame(p.to_vec()));
                    self.buf.drain(..start + total);
                    self.resyncing = false;
                }
                Err((exp, found)) => {
                    self.error(
                        FrameErrorKind::Crc,
                        &format!("CRC mismatch: expected {exp:#x}, found {found:#x}"),
                        &frame,
                        out,
                    );
                    self.buf.drain(..1);
                }
            }
        }
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.textline = TextLine::default();
        self.resyncing = false;
        self.garbage.clear();
    }
}

/// No framing: each `push` call (one transport read or datagram) is a frame.
struct Unframed {
    crc: Crc,
    max: usize,
}

impl Framer for Unframed {
    fn encode(&self, payload: &[u8]) -> Vec<u8> {
        let mut body = payload.to_vec();
        self.crc.append(&mut body, 0);
        body
    }

    fn push(&mut self, data: &[u8], out: &mut Vec<FrameEvent>) {
        if data.is_empty() {
            return;
        }
        if data.len() > self.max + self.crc.len() {
            out.push(FrameEvent::Error {
                kind: FrameErrorKind::Framing,
                reason: "frame exceeds max_frame_len".into(),
                bytes: truncate(data),
            });
            return;
        }
        crc_event(&self.crc, data, out);
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests;
