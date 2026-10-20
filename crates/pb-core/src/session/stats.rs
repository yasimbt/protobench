//! Link statistics, rates, bad-frame capture and request latency.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

/// Lock-free counters updated by the I/O thread.
#[derive(Debug, Default)]
pub struct LinkCounters {
    /// Bytes received.
    pub rx_bytes: AtomicU64,
    /// Bytes sent.
    pub tx_bytes: AtomicU64,
    /// Good frames received.
    pub rx_frames: AtomicU64,
    /// Frames sent.
    pub tx_frames: AtomicU64,
    /// Messages decoded.
    pub rx_msgs: AtomicU64,
    /// Framing errors (bad encoding, bad length, overflow, garbage).
    pub framing_errors: AtomicU64,
    /// CRC mismatches.
    pub crc_errors: AtomicU64,
    /// Frames that did not decode as the expected type.
    pub decode_errors: AtomicU64,
    /// Unknown message IDs (id_prefix mode).
    pub unknown_ids: AtomicU64,
    /// Text lines received.
    pub text_lines: AtomicU64,
    /// Events dropped because a subscriber was too slow.
    pub dropped_events: AtomicU64,
    /// Telemetry samples written.
    pub samples: AtomicU64,
    /// Requests that timed out.
    pub timeouts: AtomicU64,
    /// Retries sent.
    pub retries: AtomicU64,
    /// Reconnect attempts.
    pub reconnects: AtomicU64,
}

/// Plain copy of the counters plus rates.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct LinkSnapshot {
    /// Bytes received.
    pub rx_bytes: u64,
    /// Bytes sent.
    pub tx_bytes: u64,
    /// Good frames received.
    pub rx_frames: u64,
    /// Frames sent.
    pub tx_frames: u64,
    /// Messages decoded.
    pub rx_msgs: u64,
    /// Framing errors.
    pub framing_errors: u64,
    /// CRC errors.
    pub crc_errors: u64,
    /// Decode errors.
    pub decode_errors: u64,
    /// Unknown message IDs.
    pub unknown_ids: u64,
    /// Text lines.
    pub text_lines: u64,
    /// Dropped events.
    pub dropped_events: u64,
    /// Telemetry samples.
    pub samples: u64,
    /// Request timeouts.
    pub timeouts: u64,
    /// Retries.
    pub retries: u64,
    /// Reconnect attempts.
    pub reconnects: u64,
    /// Received bytes per second.
    pub rx_bps: f64,
    /// Sent bytes per second.
    pub tx_bps: f64,
    /// Received messages per second.
    pub rx_mps: f64,
    /// Sent frames per second.
    pub tx_fps: f64,
}

impl LinkSnapshot {
    /// Sum of all error counters.
    pub fn errors(&self) -> u64 {
        self.framing_errors + self.crc_errors + self.decode_errors + self.unknown_ids
    }
}

impl LinkCounters {
    /// Adds to a counter.
    pub fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }

    /// Copies all counters.
    pub fn snapshot(&self) -> LinkSnapshot {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        LinkSnapshot {
            rx_bytes: g(&self.rx_bytes),
            tx_bytes: g(&self.tx_bytes),
            rx_frames: g(&self.rx_frames),
            tx_frames: g(&self.tx_frames),
            rx_msgs: g(&self.rx_msgs),
            framing_errors: g(&self.framing_errors),
            crc_errors: g(&self.crc_errors),
            decode_errors: g(&self.decode_errors),
            unknown_ids: g(&self.unknown_ids),
            text_lines: g(&self.text_lines),
            dropped_events: g(&self.dropped_events),
            samples: g(&self.samples),
            timeouts: g(&self.timeouts),
            retries: g(&self.retries),
            reconnects: g(&self.reconnects),
            ..Default::default()
        }
    }

    /// Resets every counter.
    pub fn reset(&self) {
        for c in [
            &self.rx_bytes,
            &self.tx_bytes,
            &self.rx_frames,
            &self.tx_frames,
            &self.rx_msgs,
            &self.framing_errors,
            &self.crc_errors,
            &self.decode_errors,
            &self.unknown_ids,
            &self.text_lines,
            &self.dropped_events,
            &self.samples,
            &self.timeouts,
            &self.retries,
            &self.reconnects,
        ] {
            c.store(0, Ordering::Relaxed);
        }
    }
}

/// Computes rates from successive snapshots.
#[derive(Debug, Default)]
pub struct RateMeter {
    last: Option<(u64, LinkSnapshot)>,
    rates: (f64, f64, f64, f64),
}

impl RateMeter {
    /// Updates with a snapshot taken at `now_ns`; recomputes at most every
    /// 250 ms. Returns the snapshot with rates filled in.
    pub fn update(&mut self, now_ns: u64, mut s: LinkSnapshot) -> LinkSnapshot {
        match self.last {
            None => self.last = Some((now_ns, s)),
            Some((t0, p)) => {
                let dt = (now_ns.saturating_sub(t0)) as f64 / 1e9;
                if dt >= 0.25 {
                    let r = |a: u64, b: u64| a.saturating_sub(b) as f64 / dt;
                    self.rates = (
                        r(s.rx_bytes, p.rx_bytes),
                        r(s.tx_bytes, p.tx_bytes),
                        r(s.rx_msgs, p.rx_msgs),
                        r(s.tx_frames, p.tx_frames),
                    );
                    self.last = Some((now_ns, s));
                }
            }
        }
        (s.rx_bps, s.tx_bps, s.rx_mps, s.tx_fps) = self.rates;
        s
    }
}

/// A frame that failed framing, CRC or decoding.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BadFrame {
    /// Host time in ns.
    pub host_ts_ns: u64,
    /// `framing`, `crc`, `decode` or `unknown_id`.
    pub kind: &'static str,
    /// Explanation.
    pub reason: String,
    /// Bytes (truncated).
    pub bytes: Vec<u8>,
}

/// Keeps the last N bad frames.
#[derive(Debug)]
pub struct BadFrames {
    items: Mutex<VecDeque<BadFrame>>,
    cap: usize,
}

impl BadFrames {
    /// Keeps at most `cap` frames.
    pub fn new(cap: usize) -> Self {
        Self {
            items: Mutex::new(VecDeque::new()),
            cap: cap.max(1),
        }
    }

    /// Adds a frame.
    pub fn push(&self, f: BadFrame) {
        let mut g = self.items.lock();
        if g.len() == self.cap {
            g.pop_front();
        }
        g.push_back(f);
    }

    /// Copies the frames, oldest first.
    pub fn list(&self) -> Vec<BadFrame> {
        self.items.lock().iter().cloned().collect()
    }

    /// Removes all.
    pub fn clear(&self) {
        self.items.lock().clear();
    }
}

/// Latency statistics for one command.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct LatencyStats {
    /// Responses counted.
    pub count: u64,
    /// Minimum in ms.
    pub min_ms: f64,
    /// Mean in ms.
    pub avg_ms: f64,
    /// Maximum in ms.
    pub max_ms: f64,
    /// 95th percentile in ms (over the last 1000 responses).
    pub p95_ms: f64,
    /// Timeouts.
    pub timeouts: u64,
}

#[derive(Debug, Default)]
struct LatencyAcc {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
    recent: VecDeque<f64>,
    timeouts: u64,
}

/// Per-command latency tracker.
#[derive(Debug, Default)]
pub struct Latency {
    by_name: Mutex<HashMap<String, LatencyAcc>>,
}

impl Latency {
    /// Records a response latency.
    pub fn record(&self, name: &str, latency_ns: u64) {
        let ms = latency_ns as f64 / 1e6;
        let mut g = self.by_name.lock();
        let a = g.entry(name.to_string()).or_default();
        if a.count == 0 {
            a.min = ms;
            a.max = ms;
        } else {
            a.min = a.min.min(ms);
            a.max = a.max.max(ms);
        }
        a.count += 1;
        a.sum += ms;
        if a.recent.len() == 1000 {
            a.recent.pop_front();
        }
        a.recent.push_back(ms);
    }

    /// Records a timeout.
    pub fn record_timeout(&self, name: &str) {
        self.by_name
            .lock()
            .entry(name.to_string())
            .or_default()
            .timeouts += 1;
    }

    /// Statistics per command name, sorted by name.
    pub fn list(&self) -> Vec<(String, LatencyStats)> {
        let g = self.by_name.lock();
        let mut v: Vec<_> = g
            .iter()
            .map(|(k, a)| {
                let mut sorted: Vec<f64> = a.recent.iter().copied().collect();
                sorted.sort_by(f64::total_cmp);
                let p95 = if sorted.is_empty() {
                    0.0
                } else {
                    let idx =
                        ((sorted.len() as f64 * 0.95).ceil() as usize).clamp(1, sorted.len()) - 1;
                    sorted[idx]
                };
                (
                    k.clone(),
                    LatencyStats {
                        count: a.count,
                        min_ms: a.min,
                        avg_ms: if a.count > 0 {
                            a.sum / a.count as f64
                        } else {
                            0.0
                        },
                        max_ms: a.max,
                        p95_ms: p95,
                        timeouts: a.timeouts,
                    },
                )
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Clears all statistics.
    pub fn clear(&self) {
        self.by_name.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_percentiles() {
        let l = Latency::default();
        for i in 1..=100 {
            l.record("ping", i * 1_000_000);
        }
        l.record_timeout("ping");
        let (name, s) = &l.list()[0];
        assert_eq!(name, "ping");
        assert_eq!(s.count, 100);
        assert_eq!(s.min_ms, 1.0);
        assert_eq!(s.max_ms, 100.0);
        assert!((s.avg_ms - 50.5).abs() < 1e-9);
        assert_eq!(s.p95_ms, 95.0);
        assert_eq!(s.timeouts, 1);
    }

    #[test]
    fn rates_and_bad_frames() {
        let c = LinkCounters::default();
        let mut m = RateMeter::default();
        m.update(0, c.snapshot());
        LinkCounters::add(&c.rx_bytes, 1000);
        LinkCounters::add(&c.rx_msgs, 10);
        let s = m.update(500_000_000, c.snapshot());
        assert_eq!(s.rx_bps, 2000.0);
        assert_eq!(s.rx_mps, 20.0);
        let b = BadFrames::new(2);
        for i in 0..3 {
            b.push(BadFrame {
                host_ts_ns: i,
                kind: "crc",
                reason: String::new(),
                bytes: vec![],
            });
        }
        assert_eq!(b.list().len(), 2);
        assert_eq!(b.list()[0].host_ts_ns, 1);
        c.reset();
        assert_eq!(c.snapshot().rx_bytes, 0);
    }
}
