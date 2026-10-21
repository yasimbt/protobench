//! Command history with resend.

use std::collections::VecDeque;

/// One sent command.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    /// Host time in ns.
    pub host_ts_ns: u64,
    /// Device it was sent to (`main` for the project's own).
    pub device: String,
    /// Command name or message name.
    pub name: String,
    /// Message (or oneof member) that was sent.
    pub message: String,
    /// Payload as Protobuf JSON of the inner message.
    pub payload: serde_json::Value,
    /// Expected reply, if any.
    pub expect: Option<String>,
}

/// Bounded list of recently sent commands, newest last.
#[derive(Debug, Clone)]
pub struct CommandHistory {
    entries: VecDeque<HistoryEntry>,
    cap: usize,
}

impl Default for CommandHistory {
    fn default() -> Self {
        Self::new(200)
    }
}

impl CommandHistory {
    /// Creates a history keeping at most `cap` entries.
    pub fn new(cap: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Adds an entry, dropping the oldest when full.
    pub fn push(&mut self, e: HistoryEntry) {
        if self.entries.len() == self.cap {
            self.entries.pop_front();
        }
        self.entries.push_back(e);
    }

    /// Entries, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &HistoryEntry> {
        self.entries.iter()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Removes all entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded() {
        let mut h = CommandHistory::new(2);
        for i in 0..3 {
            h.push(HistoryEntry {
                device: "main".into(),
                host_ts_ns: i,
                name: format!("c{i}"),
                message: "ping".into(),
                payload: serde_json::json!({}),
                expect: None,
            });
        }
        assert_eq!(h.len(), 2);
        assert_eq!(h.iter().next().unwrap().name, "c1");
    }
}
