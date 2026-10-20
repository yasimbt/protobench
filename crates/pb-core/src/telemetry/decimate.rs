//! Min/max decimation for plotting millions of points.

use super::RingBuffer;

/// Extremes of one pixel column.
struct Bucket {
    col: i64,
    min: (f64, f64),
    max: (f64, f64),
    imin: usize,
    imax: usize,
}

/// Returns the points of `ring` in `[t0, t1]` reduced to at most two points
/// (min and max, in time order) per pixel column. One sample on each side of
/// the window is included so lines reach the plot edges. When the window
/// holds fewer than `2 * columns` samples, they are returned unchanged.
pub fn decimate_minmax(ring: &RingBuffer, t0: f64, t1: f64, columns: usize) -> Vec<[f64; 2]> {
    let columns = columns.max(1);
    if ring.is_empty() || t1 < t0 {
        return Vec::new();
    }
    let start = ring.lower_bound(t0).saturating_sub(1);
    let mut end = ring.lower_bound(t1);
    while end < ring.len() && ring.time(end) <= t1 {
        end += 1;
    }
    end = (end + 1).min(ring.len());
    let n = end.saturating_sub(start);
    if n <= 2 * columns {
        return (start..end)
            .filter_map(|i| ring.get(i))
            .map(|(t, v)| [t, v])
            .collect();
    }
    let span = (t1 - t0).max(f64::MIN_POSITIVE);
    let width = span / columns as f64;
    let mut out = Vec::with_capacity(2 * columns + 4);
    let mut bucket: Option<Bucket> = None;
    let flush = |out: &mut Vec<[f64; 2]>, b: Bucket| {
        let Bucket {
            min,
            max,
            imin,
            imax,
            ..
        } = b;
        if imin == imax {
            out.push([min.0, min.1]);
        } else if imin < imax {
            out.push([min.0, min.1]);
            out.push([max.0, max.1]);
        } else {
            out.push([max.0, max.1]);
            out.push([min.0, min.1]);
        }
    };
    for i in start..end {
        let Some((t, v)) = ring.get(i) else { continue };
        let col = ((t - t0) / width).floor() as i64;
        match &mut bucket {
            Some(b) if b.col == col => {
                if v < b.min.1 {
                    b.min = (t, v);
                    b.imin = i;
                }
                if v > b.max.1 {
                    b.max = (t, v);
                    b.imax = i;
                }
            }
            _ => {
                if let Some(b) = bucket.take() {
                    flush(&mut out, b);
                }
                bucket = Some(Bucket {
                    col,
                    min: (t, v),
                    max: (t, v),
                    imin: i,
                    imax: i,
                });
            }
        }
    }
    if let Some(b) = bucket {
        flush(&mut out, b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_extremes_per_column() {
        let mut r = RingBuffer::new(10_000);
        for i in 0..10_000 {
            let t = i as f64 / 1000.0;
            let v = if i == 4321 {
                100.0
            } else if i == 7777 {
                -100.0
            } else {
                (i % 7) as f64
            };
            r.push(t, v);
        }
        let pts = decimate_minmax(&r, 0.0, 10.0, 100);
        assert!(pts.len() <= 2 * 100 + 4, "{}", pts.len());
        assert!(pts.iter().any(|p| p[1] == 100.0));
        assert!(pts.iter().any(|p| p[1] == -100.0));
        assert!(pts.windows(2).all(|w| w[0][0] <= w[1][0]), "time order");
    }

    #[test]
    fn small_windows_pass_through() {
        let mut r = RingBuffer::new(100);
        for i in 0..10 {
            r.push(i as f64, i as f64);
        }
        let pts = decimate_minmax(&r, 2.0, 5.0, 100);
        assert_eq!(pts.first(), Some(&[1.0, 1.0]));
        assert_eq!(pts.last(), Some(&[6.0, 6.0]));
        assert!(decimate_minmax(&RingBuffer::new(4), 0.0, 1.0, 10).is_empty());
    }

    #[test]
    fn million_points_is_fast() {
        let mut r = RingBuffer::new(1_000_000);
        for i in 0..1_000_000 {
            r.push(i as f64 * 1e-3, (i as f64 * 0.01).sin());
        }
        let start = std::time::Instant::now();
        for _ in 0..10 {
            let pts = decimate_minmax(&r, 0.0, 1000.0, 1920);
            assert!(pts.len() <= 2 * 1920 + 4);
        }
        let per_frame = start.elapsed() / 10;
        // Generous bound for debug builds; release is ~10x faster.
        assert!(per_frame.as_millis() < 400, "{per_frame:?}");
    }
}
