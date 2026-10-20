//! Replay transport: plays back the received frames and text lines of a
//! `.pbsession` recording through the normal framing pipeline, at 1×, N× or
//! maximum speed.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use super::{Transport, READ_TIMEOUT};
use crate::config::FramingConfig;
use crate::error::CoreError;
use crate::framing::build_framer;
use crate::recorder::SessionReader;
use crate::session::RecordKind;

/// Plays back a recording.
pub struct ReplayTransport {
    chunks: Vec<(u64, Vec<u8>)>,
    next: usize,
    start: Instant,
    t0: u64,
    speed: f64,
    name: String,
    pending: Vec<u8>,
}

impl ReplayTransport {
    /// Opens a recording. `speed`: 1.0 = real time, 0 = as fast as possible.
    pub fn open(path: &Path, speed: f64, framing: &FramingConfig) -> Result<Self, CoreError> {
        let mut r = SessionReader::open(path)?;
        let framer = build_framer(framing);
        let mut chunks = Vec::new();
        while let Some(rec) = r.next_record()? {
            match rec.kind {
                RecordKind::RxFrame => chunks.push((rec.host_ts_ns, framer.encode(&rec.bytes))),
                RecordKind::TextLine if framing.text_passthrough => {
                    let mut b = rec.bytes;
                    b.push(b'\n');
                    chunks.push((rec.host_ts_ns, b));
                }
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
            {
                let c = std::mem::take(&mut self.chunks[self.next].1);
                self.pending.extend_from_slice(&c);
                self.next += 1;
            }
            if self.pending.is_empty() {
                if self.next < self.chunks.len() {
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
        self.next >= self.chunks.len() && self.pending.is_empty()
    }
}
