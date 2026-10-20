//! Channels and the shared telemetry store.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use prost_reflect::EnumDescriptor;

use super::{RingBuffer, RunningStats, StatsSnapshot};
use crate::schema::FieldDisplay;

/// Default ring-buffer capacity per channel.
pub const DEFAULT_CAPACITY: usize = 1_000_000;

/// How a channel's values are interpreted.
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelKind {
    /// A number (scaled by the field metadata).
    Number,
    /// An enum; the value is the enum number.
    Enum(EnumDescriptor),
    /// A bool; the value is 0 or 1.
    Bool,
    /// A derived (computed) channel.
    Derived,
}

/// Samples and statistics of one channel.
#[derive(Debug)]
pub struct ChannelData {
    /// Engineering-unit samples.
    pub ring: RingBuffer,
    /// Running statistics since the last reset.
    pub stats: RunningStats,
}

/// One telemetry channel.
#[derive(Debug)]
pub struct Channel {
    /// Concrete name, e.g. `telemetry.adc[2]`.
    pub name: String,
    /// Display metadata.
    pub display: FieldDisplay,
    /// Value interpretation.
    pub kind: ChannelKind,
    /// Samples; lock briefly.
    pub data: RwLock<ChannelData>,
}

impl Channel {
    /// Label for UIs: display name and unit.
    pub fn label(&self) -> String {
        if self.display.unit.is_empty() {
            self.display.display.clone()
        } else {
            format!("{} [{}]", self.display.display, self.display.unit)
        }
    }

    /// Text for a value: the enum name, `true`/`false`, or a formatted number.
    pub fn format_value(&self, v: f64) -> String {
        match &self.kind {
            ChannelKind::Enum(e) => e
                .get_value(v as i32)
                .map(|ev| ev.name().to_string())
                .unwrap_or_else(|| format!("{}", v as i32)),
            ChannelKind::Bool => (v != 0.0).to_string(),
            _ => self.display.format(v),
        }
    }

    /// Latest (time, value).
    pub fn last(&self) -> Option<(f64, f64)> {
        self.data.read().ring.last()
    }

    /// Current statistics.
    pub fn stats(&self) -> StatsSnapshot {
        self.data.read().stats.snapshot()
    }

    /// Total samples pushed.
    pub fn total(&self) -> u64 {
        self.data.read().ring.total()
    }

    /// Appends an engineering-unit sample.
    pub fn push(&self, t: f64, v: f64) {
        let mut d = self.data.write();
        d.ring.push(t, v);
        d.stats.push(t, v);
    }
}

/// Channels in creation order plus a name index.
type ChannelIndex = (Vec<Arc<Channel>>, HashMap<String, usize>);

/// All channels of a session, shared between the I/O thread and readers.
#[derive(Debug)]
pub struct TelemetryStore {
    channels: RwLock<ChannelIndex>,
    capacity: AtomicUsize,
    generation: AtomicU64,
}

impl Default for TelemetryStore {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl TelemetryStore {
    /// Creates an empty store with the given per-channel capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            channels: RwLock::new((Vec::new(), HashMap::new())),
            capacity: AtomicUsize::new(capacity.max(1)),
            generation: AtomicU64::new(0),
        }
    }

    /// Per-channel capacity for new channels.
    pub fn capacity(&self) -> usize {
        self.capacity.load(Ordering::Relaxed)
    }

    /// Changes the capacity used for channels created from now on.
    pub fn set_capacity(&self, cap: usize) {
        self.capacity.store(cap.max(1), Ordering::Relaxed);
    }

    /// Bumped whenever a channel is added; lets UIs refresh lists cheaply.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Looks up a channel.
    pub fn get(&self, name: &str) -> Option<Arc<Channel>> {
        let g = self.channels.read();
        g.1.get(name).map(|&i| g.0[i].clone())
    }

    /// Returns the channel, creating it on first sight.
    pub fn get_or_create(
        &self,
        name: &str,
        make: impl FnOnce() -> (FieldDisplay, ChannelKind),
    ) -> Arc<Channel> {
        if let Some(c) = self.get(name) {
            return c;
        }
        let mut g = self.channels.write();
        if let Some(&i) = g.1.get(name) {
            return g.0[i].clone();
        }
        let (display, kind) = make();
        let ch = Arc::new(Channel {
            name: name.to_string(),
            display,
            kind,
            data: RwLock::new(ChannelData {
                ring: RingBuffer::new(self.capacity()),
                stats: RunningStats::default(),
            }),
        });
        let idx = g.0.len();
        g.0.push(ch.clone());
        g.1.insert(name.to_string(), idx);
        self.generation.fetch_add(1, Ordering::Relaxed);
        ch
    }

    /// All channels in creation order.
    pub fn channels(&self) -> Vec<Arc<Channel>> {
        self.channels.read().0.clone()
    }

    /// Channel names in creation order.
    pub fn names(&self) -> Vec<String> {
        self.channels
            .read()
            .0
            .iter()
            .map(|c| c.name.clone())
            .collect()
    }

    /// Resets statistics (not samples) of every channel.
    pub fn reset_stats(&self) {
        for c in self.channels() {
            c.data.write().stats.reset();
        }
    }

    /// Removes all samples and statistics, keeping the channels.
    pub fn clear(&self) {
        for c in self.channels() {
            let mut d = c.data.write();
            d.ring.clear();
            d.stats.reset();
        }
    }
}
