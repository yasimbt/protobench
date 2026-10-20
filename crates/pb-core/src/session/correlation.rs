//! Request/response correlation by sequence id, with timeouts and retries.
//!
//! Pure logic driven by explicit timestamps, so it is tested without
//! threads or sleeping. The session calls [`Correlator::register`] when it
//! sends, [`Correlator::on_message`] for every received message and
//! [`Correlator::tick`] periodically.

use std::sync::Arc;

use crate::codec::{Codec, DecodedMessage, MessageTarget};

/// Final result of a request.
#[derive(Debug, Clone)]
pub enum RequestOutcome {
    /// Fire-and-forget request was written.
    Sent,
    /// The expected response arrived.
    Ok {
        /// The response.
        response: Arc<DecodedMessage>,
        /// Time from the last (re)send to the response.
        latency_ns: u64,
        /// Number of times the request was sent.
        attempts: u32,
    },
    /// No response within the timeout, after all retries.
    Timeout {
        /// Number of times the request was sent.
        attempts: u32,
    },
    /// The request failed (not connected, encode error, wrong reply type).
    Error(String),
}

impl RequestOutcome {
    /// True for `Ok` and `Sent`.
    pub fn is_ok(&self) -> bool {
        matches!(self, RequestOutcome::Ok { .. } | RequestOutcome::Sent)
    }

    /// One-word status for badges and reports.
    pub fn label(&self) -> &'static str {
        match self {
            RequestOutcome::Sent => "sent",
            RequestOutcome::Ok { .. } => "ok",
            RequestOutcome::Timeout { .. } => "timeout",
            RequestOutcome::Error(_) => "error",
        }
    }

    /// The response, if any.
    pub fn response(&self) -> Option<&Arc<DecodedMessage>> {
        match self {
            RequestOutcome::Ok { response, .. } => Some(response),
            _ => None,
        }
    }
}

/// A request awaiting its response.
#[derive(Debug)]
pub struct Pending<T> {
    /// Session-wide request id.
    pub id: u64,
    /// Sequence id written into the request (0 if correlation is off).
    pub seq: u32,
    /// Command name.
    pub name: String,
    /// Expected reply.
    pub expect: MessageTarget,
    /// Wire bytes, for retries.
    pub wire: Vec<u8>,
    /// Timeout per attempt.
    pub timeout_ns: u64,
    /// Retries left.
    pub retries_left: u32,
    /// Attempts so far.
    pub attempts: u32,
    /// Time of the last (re)send.
    pub sent_ns: u64,
    /// Caller data (e.g. a reply channel).
    pub token: T,
}

/// Something the session must do after `tick`.
#[derive(Debug)]
pub enum TickAction {
    /// Resend these bytes (retry).
    Resend {
        /// Request id.
        id: u64,
        /// Bytes to write.
        wire: Vec<u8>,
    },
}

/// A resolved request.
#[derive(Debug)]
pub struct Resolved<T> {
    /// Request id.
    pub id: u64,
    /// Command name.
    pub name: String,
    /// Outcome.
    pub outcome: RequestOutcome,
    /// Caller data.
    pub token: T,
}

/// Tracks outstanding requests.
#[derive(Debug)]
pub struct Correlator<T> {
    next_seq: u32,
    pending: Vec<Pending<T>>,
    by_seq: bool,
}

/// Largest sequence id before wrapping (fits int32 fields).
pub const MAX_SEQ: u32 = 0x7FFF_FFFF;

impl<T> Correlator<T> {
    /// `by_seq`: match responses by sequence id; otherwise the oldest
    /// pending request expecting the received type wins.
    pub fn new(by_seq: bool) -> Self {
        Self {
            next_seq: 1,
            pending: Vec::new(),
            by_seq,
        }
    }

    /// Next sequence id (never 0, wraps at [`MAX_SEQ`]).
    pub fn next_seq(&mut self) -> u32 {
        let s = self.next_seq;
        self.next_seq = if s >= MAX_SEQ { 1 } else { s + 1 };
        s
    }

    /// Number of outstanding requests.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// True if nothing is outstanding.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Starts tracking a sent request.
    pub fn register(&mut self, p: Pending<T>) {
        self.pending.push(p);
    }

    /// Offers a received message. `seq` is the value of the response field
    /// (when correlation by seq is on).
    pub fn on_message(
        &mut self,
        codec: &Codec,
        msg: &Arc<DecodedMessage>,
        seq: Option<u64>,
        now_ns: u64,
    ) -> Option<Resolved<T>> {
        // Requests that carry a seq are matched by it; requests without one
        // (seq 0, e.g. a type with no seq field) match the oldest of the type.
        let by_seq = match seq {
            Some(s) if self.by_seq && s != 0 => {
                self.pending.iter().position(|p| u64::from(p.seq) == s)
            }
            _ => None,
        };
        let idx = by_seq.or_else(|| {
            self.pending
                .iter()
                .position(|p| (!self.by_seq || p.seq == 0) && codec.matches(msg, &p.expect))
        })?;
        let p = self.pending.remove(idx);
        let outcome = if codec.matches(msg, &p.expect) {
            RequestOutcome::Ok {
                response: msg.clone(),
                latency_ns: now_ns.saturating_sub(p.sent_ns),
                attempts: p.attempts,
            }
        } else {
            RequestOutcome::Error(format!(
                "expected `{}`, got `{}`",
                p.expect.name(),
                msg.kind()
            ))
        };
        Some(Resolved {
            id: p.id,
            name: p.name,
            outcome,
            token: p.token,
        })
    }

    /// Handles timeouts: returns resends and requests that timed out.
    pub fn tick(&mut self, now_ns: u64) -> (Vec<TickAction>, Vec<Resolved<T>>) {
        let mut resend = Vec::new();
        let mut done = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            let p = &mut self.pending[i];
            if now_ns.saturating_sub(p.sent_ns) < p.timeout_ns {
                i += 1;
                continue;
            }
            if p.retries_left > 0 {
                p.retries_left -= 1;
                p.attempts += 1;
                p.sent_ns = now_ns;
                resend.push(TickAction::Resend {
                    id: p.id,
                    wire: p.wire.clone(),
                });
                i += 1;
            } else {
                let p = self.pending.remove(i);
                done.push(Resolved {
                    id: p.id,
                    name: p.name,
                    outcome: RequestOutcome::Timeout {
                        attempts: p.attempts,
                    },
                    token: p.token,
                });
            }
        }
        (resend, done)
    }

    /// Fails every outstanding request (e.g. on disconnect).
    pub fn fail_all(&mut self, reason: &str) -> Vec<Resolved<T>> {
        self.pending
            .drain(..)
            .map(|p| Resolved {
                id: p.id,
                name: p.name,
                outcome: RequestOutcome::Error(reason.to_string()),
                token: p.token,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Role;
    use crate::project::Project;

    fn codecs() -> (Codec, Codec) {
        let p = Project::open(&crate::schema::tests::demo_dir());
        let s = p.schema.clone().unwrap();
        (
            Codec::new(&p.config, &s, Role::Host).unwrap(),
            Codec::new(&p.config, &s, Role::Device).unwrap(),
        )
    }

    fn reply(dev: &Codec, host: &Codec, member: &str, seq: u32) -> Arc<DecodedMessage> {
        let t = dev.resolve_tx(member).unwrap();
        let mut m = dev.build(&t, serde_json::json!({})).unwrap();
        crate::codec::set_int_at_path(
            &mut m,
            &crate::schema::FieldPath::parse("seq").unwrap(),
            u64::from(seq),
        );
        Arc::new(host.decode(&dev.encode(&m).unwrap(), 0).unwrap())
    }

    fn pending(
        c: &mut Correlator<()>,
        host: &Codec,
        id: u64,
        expect: &str,
        retries: u32,
        now: u64,
    ) -> u32 {
        let seq = c.next_seq();
        c.register(Pending {
            id,
            seq,
            name: format!("cmd{id}"),
            expect: host.resolve_rx(expect).unwrap(),
            wire: vec![id as u8],
            timeout_ns: 100,
            retries_left: retries,
            attempts: 1,
            sent_ns: now,
            token: (),
        });
        seq
    }

    #[test]
    fn matched_by_seq_out_of_order() {
        let (host, dev) = codecs();
        let mut c = Correlator::new(true);
        let s1 = pending(&mut c, &host, 1, "pong", 0, 0);
        let s2 = pending(&mut c, &host, 2, "ack", 0, 0);
        assert_ne!(s1, s2);
        // Reply to the second request first.
        let r = c
            .on_message(
                &host,
                &reply(&dev, &host, "ack", s2),
                Some(u64::from(s2)),
                30,
            )
            .unwrap();
        assert_eq!(r.id, 2);
        assert!(matches!(
            r.outcome,
            RequestOutcome::Ok {
                latency_ns: 30,
                attempts: 1,
                ..
            }
        ));
        let r = c
            .on_message(
                &host,
                &reply(&dev, &host, "pong", s1),
                Some(u64::from(s1)),
                40,
            )
            .unwrap();
        assert_eq!(r.id, 1);
        assert!(r.outcome.is_ok());
        assert!(c.is_empty());
        // Unsolicited (seq 0) and unknown seq are ignored.
        assert!(c
            .on_message(&host, &reply(&dev, &host, "pong", 0), Some(0), 0)
            .is_none());
        assert!(c
            .on_message(&host, &reply(&dev, &host, "pong", 99), Some(99), 0)
            .is_none());
    }

    #[test]
    fn wrong_type_is_error() {
        let (host, dev) = codecs();
        let mut c = Correlator::new(true);
        let s = pending(&mut c, &host, 1, "pong", 0, 0);
        let r = c
            .on_message(&host, &reply(&dev, &host, "ack", s), Some(u64::from(s)), 1)
            .unwrap();
        assert!(matches!(r.outcome, RequestOutcome::Error(ref m) if m.contains("expected `pong`")));
    }

    #[test]
    fn timeout_after_retries() {
        let (host, _) = codecs();
        let mut c = Correlator::new(true);
        pending(&mut c, &host, 1, "pong", 2, 0);
        let (r, d) = c.tick(50);
        assert!(r.is_empty() && d.is_empty());
        let (r, d) = c.tick(100);
        assert_eq!(r.len(), 1);
        assert!(d.is_empty());
        let (r, _) = c.tick(200);
        assert_eq!(r.len(), 1);
        let (r, d) = c.tick(300);
        assert!(r.is_empty());
        assert!(matches!(
            d[0].outcome,
            RequestOutcome::Timeout { attempts: 3 }
        ));
    }

    #[test]
    fn retry_then_success_reports_attempts() {
        let (host, dev) = codecs();
        let mut c = Correlator::new(true);
        let s = pending(&mut c, &host, 1, "pong", 1, 0);
        let (r, _) = c.tick(150);
        assert!(matches!(&r[0], TickAction::Resend { wire, .. } if wire == &vec![1u8]));
        let r = c
            .on_message(
                &host,
                &reply(&dev, &host, "pong", s),
                Some(u64::from(s)),
                170,
            )
            .unwrap();
        assert!(matches!(
            r.outcome,
            RequestOutcome::Ok {
                attempts: 2,
                latency_ns: 20,
                ..
            }
        ));
    }

    #[test]
    fn fifo_matching_without_seq() {
        let (host, dev) = codecs();
        let mut c = Correlator::new(false);
        pending(&mut c, &host, 1, "pong", 0, 0);
        pending(&mut c, &host, 2, "pong", 0, 0);
        let r = c
            .on_message(&host, &reply(&dev, &host, "pong", 0), None, 1)
            .unwrap();
        assert_eq!(r.id, 1);
        assert_eq!(c.fail_all("gone").len(), 1);
    }

    #[test]
    fn seq_wraps_and_skips_zero() {
        let mut c: Correlator<()> = Correlator::new(true);
        c.next_seq = MAX_SEQ;
        assert_eq!(c.next_seq(), MAX_SEQ);
        assert_eq!(c.next_seq(), 1);
    }
}
