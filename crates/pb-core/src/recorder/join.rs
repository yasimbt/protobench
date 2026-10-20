//! Channel selection and the as-of join shared by CSV and Parquet export.

use std::sync::Arc;

use crate::error::CoreError;
use crate::telemetry::{Channel, TelemetryStore};

/// Selected channels and, for each, its `(time, value)` samples.
pub(crate) type Selection = (Vec<Arc<Channel>>, Vec<Vec<(f64, f64)>>);

/// Channels to export (all when `names` is empty) and their samples within
/// `range` (seconds, inclusive).
pub(crate) fn select(
    store: &TelemetryStore,
    names: &[String],
    range: Option<(f64, f64)>,
) -> Result<Selection, CoreError> {
    let chans: Vec<Arc<Channel>> = if names.is_empty() {
        store.channels()
    } else {
        names
            .iter()
            .map(|n| {
                store
                    .get(n)
                    .ok_or_else(|| CoreError::Recorder(format!("no channel `{n}`")))
            })
            .collect::<Result<_, _>>()?
    };
    let (t0, t1) = range.unwrap_or((f64::NEG_INFINITY, f64::INFINITY));
    let samples = chans
        .iter()
        .map(|c| c.data.read().ring.range(t0, t1).collect())
        .collect();
    Ok((chans, samples))
}

/// As-of join on the union of timestamps: each row holds every channel's
/// latest value at or before the row's time (`None` before its first
/// sample).
pub(crate) struct AsOf<'a> {
    samples: &'a [Vec<(f64, f64)>],
    idx: Vec<usize>,
    cur: Vec<Option<f64>>,
}

impl<'a> AsOf<'a> {
    pub(crate) fn new(samples: &'a [Vec<(f64, f64)>]) -> Self {
        Self {
            samples,
            idx: vec![0; samples.len()],
            cur: vec![None; samples.len()],
        }
    }

    /// Advances to the next timestamp; the row's values are in
    /// [`AsOf::values`].
    pub(crate) fn next_row(&mut self) -> Option<f64> {
        let next = self
            .samples
            .iter()
            .zip(&self.idx)
            .filter_map(|(s, &i)| s.get(i).map(|x| x.0))
            .fold(f64::INFINITY, f64::min);
        if !next.is_finite() {
            return None;
        }
        for (k, s) in self.samples.iter().enumerate() {
            while let Some(&(t, v)) = s.get(self.idx[k]) {
                if t > next {
                    break;
                }
                self.cur[k] = Some(v);
                self.idx[k] += 1;
            }
        }
        Some(next)
    }

    /// Values of the current row.
    pub(crate) fn values(&self) -> &[Option<f64>] {
        &self.cur
    }
}
