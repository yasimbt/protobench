//! In-process loopback: two connected ends, used for the built-in simulator.
//!
//! In stream mode a read returns whatever is queued, like a serial port. In
//! message mode each write arrives as exactly one read, like a datagram, so
//! projects with `framing.type = "none"` work with the simulator too.

use std::io;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use super::{Transport, READ_TIMEOUT};

/// One end of a loopback pair.
pub struct LoopbackEnd {
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    name: &'static str,
    messages: bool,
}

/// Creates two connected stream ends: `(host, device)`.
pub fn pair() -> (LoopbackEnd, LoopbackEnd) {
    pair_with_mode(false)
}

/// Creates two connected ends that keep message boundaries when `messages`
/// is true.
pub fn pair_with_mode(messages: bool) -> (LoopbackEnd, LoopbackEnd) {
    let (a_tx, a_rx) = crossbeam_channel::unbounded();
    let (b_tx, b_rx) = crossbeam_channel::unbounded();
    (
        LoopbackEnd {
            tx: a_tx,
            rx: b_rx,
            pending: Vec::new(),
            name: "loopback (host end)",
            messages,
        },
        LoopbackEnd {
            tx: b_tx,
            rx: a_rx,
            pending: Vec::new(),
            name: "loopback (device end)",
            messages,
        },
    )
}

impl LoopbackEnd {
    fn fill(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        n
    }

    /// Number of chunks waiting to be read (used to check for backlog).
    pub fn backlog(&self) -> usize {
        self.rx.len()
    }
}

impl Transport for LoopbackEnd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.pending.is_empty() {
            return Ok(self.fill(buf));
        }
        match self.rx.recv_timeout(READ_TIMEOUT) {
            Ok(chunk) => {
                self.pending = chunk;
                // Coalesce whatever else is queued, up to the buffer size.
                while !self.messages && self.pending.len() < buf.len() {
                    match self.rx.try_recv() {
                        Ok(more) => self.pending.extend_from_slice(&more),
                        Err(_) => break,
                    }
                }
                Ok(self.fill(buf))
            }
            Err(RecvTimeoutError::Timeout) => Ok(0),
            Err(RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "loopback peer closed",
            )),
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.tx
            .send(data.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "loopback peer closed"))
    }

    fn describe(&self) -> String {
        self.name.to_string()
    }

    fn message_based(&self) -> bool {
        self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_flow_both_ways_and_close_is_detected() {
        let (mut a, mut b) = pair();
        a.write_all(b"hello").unwrap();
        a.write_all(b" world").unwrap();
        let mut buf = [0u8; 64];
        let n = b.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello world");
        assert_eq!(b.read(&mut buf).unwrap(), 0);
        b.write_all(b"x").unwrap();
        let mut small = [0u8; 1];
        assert_eq!(a.read(&mut small).unwrap(), 1);
        drop(b);
        assert!(a.read(&mut buf).is_err());
    }

    #[test]
    fn message_mode_keeps_boundaries() {
        let (mut a, mut b) = pair_with_mode(true);
        assert!(a.message_based());
        a.write_all(b"one").unwrap();
        a.write_all(b"two").unwrap();
        let mut buf = [0u8; 64];
        let n = b.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"one");
        let n = b.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"two");
        assert!(!pair().0.message_based());
    }
}
