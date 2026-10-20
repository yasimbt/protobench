//! Device timestamp handling: unit conversion, counter wrap, and mapping
//! onto the host time base.

use crate::config::TimeUnit;

/// Converts raw device timestamps into seconds on the host time base.
///
/// The first device timestamp is anchored to its host receive time; later
/// samples keep the device's own spacing. A backwards jump of more than one
/// second (a device reset) re-anchors.
#[derive(Debug, Clone)]
pub struct DeviceTimeBase {
    unit_s: f64,
    wrap: Option<u64>,
    last_raw: Option<u64>,
    wraps: u64,
    last_dev_s: f64,
    anchor: Option<(f64, f64)>,
}

impl DeviceTimeBase {
    /// Creates a time base. `wrap_bits = 0` disables wrap handling.
    pub fn new(unit: TimeUnit, wrap_bits: u32) -> Self {
        Self {
            unit_s: unit.seconds(),
            wrap: (wrap_bits > 0 && wrap_bits < 64).then(|| 1u64 << wrap_bits),
            last_raw: None,
            wraps: 0,
            last_dev_s: 0.0,
            anchor: None,
        }
    }

    /// Maps a raw timestamp received at `host_s` to host-base seconds.
    pub fn map(&mut self, raw: u64, host_s: f64) -> f64 {
        let raw = match self.wrap {
            Some(m) => raw % m,
            None => raw,
        };
        if let (Some(m), Some(last)) = (self.wrap, self.last_raw) {
            if raw < last && last - raw > m / 2 {
                self.wraps += 1;
            }
        }
        self.last_raw = Some(raw);
        let ext = match self.wrap {
            Some(m) => self.wraps.saturating_mul(m).saturating_add(raw),
            None => raw,
        };
        let dev_s = ext as f64 * self.unit_s;
        let reset = self.anchor.is_some() && dev_s < self.last_dev_s - 1.0;
        if self.anchor.is_none() || reset {
            if reset {
                self.wraps = 0;
            }
            self.anchor = Some((dev_s, host_s));
        }
        self.last_dev_s = dev_s;
        let (d0, h0) = self.anchor.unwrap_or((dev_s, host_s));
        h0 + (dev_s - d0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_anchor_and_wrap() {
        let mut tb = DeviceTimeBase::new(TimeUnit::Us, 32);
        assert_eq!(tb.map(1_000_000, 10.0), 10.0);
        assert!((tb.map(1_500_000, 10.7) - 10.5).abs() < 1e-9);
        // Wrap of the 32-bit counter keeps time moving forward.
        let mut tb = DeviceTimeBase::new(TimeUnit::Us, 32);
        let near = (1u64 << 32) - 1000;
        let a = tb.map(near, 0.0);
        let b = tb.map(500, 0.0);
        assert!((b - a - 0.0015).abs() < 1e-9, "{a} {b}");
        // Values above the wrap width are reduced.
        let mut tb = DeviceTimeBase::new(TimeUnit::Ms, 0);
        assert_eq!(tb.map(5000, 1.0), 1.0);
        assert!((tb.map(6000, 2.5) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn reset_reanchors() {
        let mut tb = DeviceTimeBase::new(TimeUnit::S, 0);
        tb.map(100, 1.0);
        tb.map(101, 2.0);
        let t = tb.map(0, 50.0);
        assert_eq!(t, 50.0);
    }
}
