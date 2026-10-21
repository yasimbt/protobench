//! Replay transport: plays back the received frames and text lines of a
//! `.pbsession` recording through the normal framing pipeline, at 1×, N× or
//! maximum speed. Frames are re-framed with the project's framer; text
//! lines come back through [`Transport::take_text`], whatever the framing.
//! With `framing.type = "none"` each frame is one read.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use super::{Transport, READ_TIMEOUT};
use crate::config::FramingConfig;
use crate::error::CoreError;
use crate::framing::build_framer;
use crate::recorder::SessionReader;
use crate::session::RecordKind;

enum Chunk {
    Bytes(Vec<u8>),
    Text(String),
}

/// Plays back a recording.
pub struct ReplayTransport {
    chunks: Vec<(u64, Chunk)>,
    next: usize,
    start: Instant,
    t0: u64,
    speed: f64,
    name: String,
    pending: Vec<u8>,
    text: Vec<String>,
    messages: bool,
}

impl ReplayTransport {
    /// Opens a recording. `speed`: 1.0 = real time, 0 = as fast as possible.
    pub fn open(path: &Path, speed: f64, framing: &FramingConfig) -> Result<Self, CoreError> {
        Self::open_device(path, speed, framing, crate::config::MAIN_DEVICE)
    }

    /// Opens a recording and plays the records of `device` (any device of
    /// a single-device recording).
    pub fn open_device(
        path: &Path,
        speed: f64,
        framing: &FramingConfig,
        device: &str,
    ) -> Result<Self, CoreError> {
        let mut r = SessionReader::open(path)?;
        let index = r.device_index(device).ok_or_else(|| {
            CoreError::Transport(format!(
                "{} has no device `{device}` (it has {})",
                path.display(),
                r.header
                    .devices
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        let framer = build_framer(framing);
        let mut chunks = Vec::new();
        while let Some(rec) = r.next_record()? {
            if rec.device != index {
                continue;
            }
            match rec.kind {
                RecordKind::RxFrame => {
                    chunks.push((rec.host_ts_ns, Chunk::Bytes(framer.encode(&rec.bytes))))
                }
                RecordKind::TextLine => chunks.push((
                    rec.host_ts_ns,
                    Chunk::Text(String::from_utf8_lossy(&rec.bytes).into_owned()),
                )),
                _ => {}
            }
        }
        let t0 = chunks.first().map(|c| c.0).unwrap_or(0);
        Ok(ReplayTransport {
            chunks,
            next: 0,
            start: Instant::now(),
            t0,
            speed: speed.max(0.0),
            name: format!(
                "replay {} ({}×)",
                path.display(),
                if speed == 0.0 {
                    "max".to_string()
                } else {
                    speed.to_string()
                }
            ),
            pending: Vec::new(),
            text: Vec::new(),
            messages: framing.kind == crate::config::FramingType::None,
        })
    }

    /// Number of chunks in the recording.
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// True if the recording has nothing to play.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    fn due(&self, i: usize) -> Duration {
        if self.speed == 0.0 {
            return Duration::ZERO;
        }
        let rel = self.chunks[i].0.saturating_sub(self.t0) as f64 / 1e9 / self.speed;
        Duration::from_secs_f64(rel)
    }
}

impl Transport for ReplayTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            let now = self.start.elapsed();
            while self.next < self.chunks.len()
                && self.due(self.next) <= now
                && self.pending.len() < buf.len()
                // One frame per read when frames carry no framing.
                && (!self.messages || self.pending.is_empty())
            {
                match std::mem::replace(&mut self.chunks[self.next].1, Chunk::Bytes(Vec::new())) {
                    Chunk::Bytes(b) => self.pending.extend_from_slice(&b),
                    Chunk::Text(t) => self.text.push(t),
                }
                self.next += 1;
            }
            if self.pending.is_empty() {
                if self.next < self.chunks.len() && self.text.is_empty() {
                    let wait = self.due(self.next).saturating_sub(now).min(READ_TIMEOUT);
                    std::thread::sleep(wait);
                }
                return Ok(0);
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }

    fn write_all(&mut self, _data: &[u8]) -> io::Result<()> {
        // A recording cannot answer; writes are dropped.
        Ok(())
    }

    fn describe(&self) -> String {
        self.name.clone()
    }

    fn finished(&self) -> bool {
        self.next >= self.chunks.len() && self.pending.is_empty() && self.text.is_empty()
    }

    fn take_text(&mut self) -> Vec<String> {
        std::mem::take(&mut self.text)
    }

    fn message_based(&self) -> bool {
        self.messages
    }
}
