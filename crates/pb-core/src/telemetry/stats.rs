//! Running statistics per channel.

/// Welford running statistics plus a rate estimate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunningStats {
    /// Samples since the last reset.
    pub count: u64,
    mean: f64,
    m2: f64,
    /// Minimum.
    pub min: f64,
    /// Maximum.
    pub max: f64,
    /// Last value.
    pub last: f64,
    first_t: f64,
    last_t: f64,
}

/// A snapshot of channel statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct StatsSnapshot {
    /// Samples counted.
    pub count: u64,
    /// Minimum.
    pub min: f64,
    /// Maximum.
    pub max: f64,
    /// Mean.
    pub mean: f64,
    /// Population standard deviation.
    pub std: f64,
    /// Last value.
    pub last: f64,
    /// Average sample rate over the counted interval.
    pub rate_hz: f64,
}

impl StatsSnapshot {
    /// Looks up a statistic by name (`mean`, `min`, `max`, `std`, `rate_hz`,
    /// `count`, `last`).
    pub fn get(&self, stat: &str) -> Option<f64> {
        Some(match stat {
            "mean" => self.mean,
            "min" => self.min,
            "max" => self.max,
            "std" => self.std,
            "rate_hz" | "rate" => self.rate_hz,
            "count" => self.count as f64,
            "last" => self.last,
            _ => return None,
        })
    }
}

impl RunningStats {
    /// Adds a sample at time `t` (seconds).
    pub fn push(&mut self, t: f64, v: f64) {
        if !v.is_finite() {
            return;
        }
        if self.count == 0 {
            self.min = v;
            self.max = v;
            self.first_t = t;
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
        self.count += 1;
        let d = v - self.mean;
        self.mean += d / self.count as f64;
        self.m2 += d * (v - self.mean);
        self.last = v;
        self.last_t = t;
    }

    /// Current values.
    pub fn snapshot(&self) -> StatsSnapshot {
        let span = self.last_t - self.first_t;
        StatsSnapshot {
            count: self.count,
            min: self.min,
            max: self.max,
            mean: self.mean,
            std: if self.count > 0 {
                (self.m2 / self.count as f64).sqrt()
            } else {
                0.0
            },
            last: self.last,
            rate_hz: if self.count > 1 && span > 0.0 {
                (self.count - 1) as f64 / span
            } else {
                0.0
            },
        }
    }

    /// Starts over.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Statistics over a slice of samples; `duration_s` (when known) sets the
/// rate as `count / duration_s`.
pub fn stats_of(samples: &[(f64, f64)], duration_s: Option<f64>) -> StatsSnapshot {
    let mut s = RunningStats::default();
    for (t, v) in samples {
        s.push(*t, *v);
    }
    let mut snap = s.snapshot();
    if let Some(d) = duration_s.filter(|d| *d > 0.0) {
        snap.rate_hz = snap.count as f64 / d;
    }
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welford_matches_direct_computation() {
        let xs = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let mut s = RunningStats::default();
        for (i, x) in xs.iter().enumerate() {
            s.push(i as f64 * 0.1, *x);
        }
        let snap = s.snapshot();
        assert_eq!(snap.count, 8);
        assert!((snap.mean - 5.0).abs() < 1e-12);
        assert!((snap.std - 2.0).abs() < 1e-12);
        assert_eq!(snap.min, 2.0);
        assert_eq!(snap.max, 9.0);
        assert_eq!(snap.last, 9.0);
        assert!((snap.rate_hz - 10.0).abs() < 1e-9);
        assert_eq!(snap.get("std"), Some(snap.std));
        assert_eq!(snap.get("count"), Some(8.0));
        assert_eq!(snap.get("bogus"), None);
        s.reset();
        assert_eq!(s.snapshot().count, 0);
    }

    #[test]
    fn stats_of_with_duration() {
        let samples: Vec<_> = (0..100).map(|i| (i as f64 * 0.01, 1.0)).collect();
        let s = stats_of(&samples, Some(2.0));
        assert_eq!(s.rate_hz, 50.0);
        assert_eq!(s.std, 0.0);
    }
}
