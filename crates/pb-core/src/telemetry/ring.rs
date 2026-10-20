//! Fixed-capacity ring buffer of (time, value) samples.

/// Ring buffer holding the newest `capacity` samples. Times are expected to
/// be non-decreasing.
#[derive(Debug, Clone)]
pub struct RingBuffer {
    t: Vec<f64>,
    v: Vec<f64>,
    cap: usize,
    head: usize,
    len: usize,
    total: u64,
}

impl RingBuffer {
    /// Creates an empty buffer. Memory grows as samples arrive, up to
    /// `capacity`.
    pub fn new(capacity: usize) -> Self {
        Self {
            t: Vec::new(),
            v: Vec::new(),
            cap: capacity.max(1),
            head: 0,
            len: 0,
            total: 0,
        }
    }

    /// Capacity.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Number of samples held.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Samples ever pushed (including overwritten ones).
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Appends a sample, overwriting the oldest when full.
    pub fn push(&mut self, t: f64, v: f64) {
        if self.t.len() < self.cap {
            self.t.push(t);
            self.v.push(v);
            self.len += 1;
            self.head = self.t.len() % self.cap;
        } else {
            self.t[self.head] = t;
            self.v[self.head] = v;
            self.head = (self.head + 1) % self.cap;
            self.len = self.cap;
        }
        self.total += 1;
    }

    fn phys(&self, i: usize) -> usize {
        let start = if self.t.len() < self.cap {
            0
        } else {
            self.head
        };
        (start + i) % self.cap
    }

    /// Sample `i` in chronological order (0 = oldest).
    pub fn get(&self, i: usize) -> Option<(f64, f64)> {
        (i < self.len).then(|| {
            let p = self.phys(i);
            (self.t[p], self.v[p])
        })
    }

    /// Time of sample `i`.
    pub fn time(&self, i: usize) -> f64 {
        self.t[self.phys(i)]
    }

    /// Newest sample.
    pub fn last(&self) -> Option<(f64, f64)> {
        self.len.checked_sub(1).and_then(|i| self.get(i))
    }

    /// Oldest sample.
    pub fn first(&self) -> Option<(f64, f64)> {
        self.get(0)
    }

    /// Iterates oldest to newest.
    pub fn iter(&self) -> impl Iterator<Item = (f64, f64)> + '_ {
        (0..self.len).map(move |i| {
            let p = self.phys(i);
            (self.t[p], self.v[p])
        })
    }

    /// Index of the first sample with time >= `t` (binary search).
    pub fn lower_bound(&self, t: f64) -> usize {
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.time(mid) < t {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Samples with `t0 <= t <= t1`.
    pub fn range(&self, t0: f64, t1: f64) -> impl Iterator<Item = (f64, f64)> + '_ {
        let start = self.lower_bound(t0);
        (start..self.len)
            .map(move |i| self.get(i).unwrap_or((f64::NAN, f64::NAN)))
            .take_while(move |(t, _)| *t <= t1)
    }

    /// Samples pushed after the running total was `mark`. Returns `None` if
    /// some of them were already overwritten.
    pub fn since(&self, mark: u64) -> Option<Vec<(f64, f64)>> {
        let n = self.total.checked_sub(mark)? as usize;
        if n > self.len {
            return None;
        }
        Some(
            (self.len - n..self.len)
                .filter_map(|i| self.get(i))
                .collect(),
        )
    }

    /// Removes all samples.
    pub fn clear(&mut self) {
        self.t.clear();
        self.v.clear();
        self.head = 0;
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_around_keeping_newest() {
        let mut r = RingBuffer::new(4);
        for i in 0..10 {
            r.push(i as f64, (i * 10) as f64);
        }
        assert_eq!(r.len(), 4);
        assert_eq!(r.total(), 10);
        let v: Vec<_> = r.iter().collect();
        assert_eq!(v, vec![(6.0, 60.0), (7.0, 70.0), (8.0, 80.0), (9.0, 90.0)]);
        assert_eq!(r.first(), Some((6.0, 60.0)));
        assert_eq!(r.last(), Some((9.0, 90.0)));
        assert_eq!(r.get(4), None);
        assert_eq!(r.lower_bound(7.5), 2);
        let range: Vec<_> = r.range(7.0, 8.0).collect();
        assert_eq!(range, vec![(7.0, 70.0), (8.0, 80.0)]);
        assert_eq!(r.since(8).unwrap(), vec![(8.0, 80.0), (9.0, 90.0)]);
        assert!(r.since(2).is_none());
        assert_eq!(r.since(10).unwrap(), vec![]);
    }

    #[test]
    fn partial_fill() {
        let mut r = RingBuffer::new(100);
        r.push(1.0, 1.0);
        r.push(2.0, 2.0);
        assert_eq!(r.iter().count(), 2);
        assert_eq!(r.lower_bound(0.0), 0);
        assert_eq!(r.lower_bound(9.0), 2);
        r.clear();
        assert!(r.is_empty());
        assert_eq!(r.last(), None);
    }
}
