//! MQTT 3.1.1 control packets: the subset a client and a small test broker
//! need (QoS 0 and 1; no QoS 2, no wills).

use std::fmt;

/// Largest remaining length MQTT allows (four length bytes).
pub const MAX_REMAINING: usize = 268_435_455;

/// A malformed packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketError(pub String);

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MQTT: {}", self.0)
    }
}

impl std::error::Error for PacketError {}

fn err<T>(m: impl Into<String>) -> Result<T, PacketError> {
    Err(PacketError(m.into()))
}

/// CONNECT.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Connect {
    /// Client identifier.
    pub client_id: String,
    /// Clean session flag.
    pub clean_session: bool,
    /// Keep-alive in seconds (0 = off).
    pub keep_alive: u16,
    /// User name.
    pub username: Option<String>,
    /// Password.
    pub password: Option<Vec<u8>>,
}

/// PUBLISH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publish {
    /// Topic name.
    pub topic: String,
    /// QoS 0 or 1.
    pub qos: u8,
    /// Redelivery flag.
    pub dup: bool,
    /// Retain flag.
    pub retain: bool,
    /// Packet identifier (QoS 1).
    pub pkid: u16,
    /// Payload.
    pub payload: Vec<u8>,
}

/// A control packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// Client → broker: open a session.
    Connect(Connect),
    /// Broker → client: session present, return code (0 = accepted).
    ConnAck {
        /// The broker kept a previous session.
        session_present: bool,
        /// 0 accepted; 1–5 refused.
        code: u8,
    },
    /// A message, either way.
    Publish(Publish),
    /// QoS 1 acknowledgement.
    PubAck(u16),
    /// Client → broker: subscribe to (filter, max QoS) pairs.
    Subscribe {
        /// Packet identifier.
        pkid: u16,
        /// Topic filters with their requested QoS.
        filters: Vec<(String, u8)>,
    },
    /// Broker → client: granted QoS per filter (0x80 = refused).
    SubAck {
        /// Packet identifier.
        pkid: u16,
        /// Return codes.
        codes: Vec<u8>,
    },
    /// Keep-alive ping.
    PingReq,
    /// Keep-alive answer.
    PingResp,
    /// Client → broker: closing.
    Disconnect,
}

fn put_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s);
}

fn put_remaining(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let mut b = (n % 128) as u8;
        n /= 128;
        if n > 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
}

/// Reads the remaining-length field at `buf[1..]`: Ok(None) if incomplete,
/// else (length, bytes it took).
fn remaining(buf: &[u8]) -> Result<Option<(usize, usize)>, PacketError> {
    let mut n = 0usize;
    let mut mult = 1usize;
    for i in 0..4 {
        let Some(&b) = buf.get(1 + i) else {
            return Ok(None);
        };
        n += usize::from(b & 0x7F) * mult;
        if b & 0x80 == 0 {
            return Ok(Some((n, i + 1)));
        }
        mult *= 128;
    }
    err("remaining length longer than 4 bytes")
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, PacketError> {
        let v = *self
            .b
            .get(self.at)
            .ok_or_else(|| PacketError("packet too short".into()))?;
        self.at += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, PacketError> {
        Ok(u16::from_be_bytes([self.u8()?, self.u8()?]))
    }
    fn bytes(&mut self) -> Result<&'a [u8], PacketError> {
        let n = usize::from(self.u16()?);
        let s = self
            .b
            .get(self.at..self.at + n)
            .ok_or_else(|| PacketError("string runs past the packet".into()))?;
        self.at += n;
        Ok(s)
    }
    fn string(&mut self) -> Result<String, PacketError> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| PacketError("invalid UTF-8".into()))
    }
    fn rest(&self) -> &'a [u8] {
        &self.b[self.at.min(self.b.len())..]
    }
}

impl Packet {
    /// Encodes the packet.
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        let first = match self {
            Packet::Connect(c) => {
                put_str(&mut body, b"MQTT");
                body.push(4); // protocol level 3.1.1
                let mut flags = 0u8;
                if c.clean_session {
                    flags |= 0x02;
                }
                if c.username.is_some() {
                    flags |= 0x80;
                }
                if c.password.is_some() {
                    flags |= 0x40;
                }
                body.push(flags);
                body.extend_from_slice(&c.keep_alive.to_be_bytes());
                put_str(&mut body, c.client_id.as_bytes());
                if let Some(u) = &c.username {
                    put_str(&mut body, u.as_bytes());
                }
                if let Some(p) = &c.password {
                    put_str(&mut body, p);
                }
                0x10
            }
            Packet::ConnAck {
                session_present,
                code,
            } => {
                body.push(u8::from(*session_present));
                body.push(*code);
                0x20
            }
            Packet::Publish(p) => {
                put_str(&mut body, p.topic.as_bytes());
                if p.qos > 0 {
                    body.extend_from_slice(&p.pkid.to_be_bytes());
                }
                body.extend_from_slice(&p.payload);
                0x30 | (u8::from(p.dup) << 3) | ((p.qos & 3) << 1) | u8::from(p.retain)
            }
            Packet::PubAck(id) => {
                body.extend_from_slice(&id.to_be_bytes());
                0x40
            }
            Packet::Subscribe { pkid, filters } => {
                body.extend_from_slice(&pkid.to_be_bytes());
                for (f, q) in filters {
                    put_str(&mut body, f.as_bytes());
                    body.push(*q);
                }
                0x82
            }
            Packet::SubAck { pkid, codes } => {
                body.extend_from_slice(&pkid.to_be_bytes());
                body.extend_from_slice(codes);
                0x90
            }
            Packet::PingReq => 0xC0,
            Packet::PingResp => 0xD0,
            Packet::Disconnect => 0xE0,
        };
        let mut out = Vec::with_capacity(body.len() + 5);
        out.push(first);
        put_remaining(&mut out, body.len());
        out.extend_from_slice(&body);
        out
    }

    /// Decodes one packet from the start of `buf`. Ok(None) means more
    /// bytes are needed; otherwise the packet and the bytes it used.
    pub fn parse(buf: &[u8], max_len: usize) -> Result<Option<(Packet, usize)>, PacketError> {
        let Some(&first) = buf.first() else {
            return Ok(None);
        };
        let Some((len, n)) = remaining(buf)? else {
            return Ok(None);
        };
        if len > max_len {
            return err(format!(
                "packet of {len} bytes exceeds the limit of {max_len}"
            ));
        }
        let total = 1 + n + len;
        if buf.len() < total {
            return Ok(None);
        }
        let body = &buf[1 + n..total];
        let mut r = Reader { b: body, at: 0 };
        let p = match first >> 4 {
            1 => {
                let proto = r.bytes()?;
                let level = r.u8()?;
                if proto != b"MQTT" || level != 4 {
                    return err("only MQTT 3.1.1 is supported");
                }
                let flags = r.u8()?;
                if flags & 0x04 != 0 {
                    return err("wills are not supported");
                }
                let keep_alive = r.u16()?;
                let client_id = r.string()?;
                let username = (flags & 0x80 != 0).then(|| r.string()).transpose()?;
                let password = (flags & 0x40 != 0)
                    .then(|| r.bytes().map(<[u8]>::to_vec))
                    .transpose()?;
                Packet::Connect(Connect {
                    client_id,
                    clean_session: flags & 0x02 != 0,
                    keep_alive,
                    username,
                    password,
                })
            }
            2 => Packet::ConnAck {
                session_present: r.u8()? & 1 != 0,
                code: r.u8()?,
            },
            3 => {
                let qos = (first >> 1) & 3;
                if qos > 1 {
                    return err("QoS 2 is not supported");
                }
                let topic = r.string()?;
                let pkid = if qos > 0 { r.u16()? } else { 0 };
                Packet::Publish(Publish {
                    topic,
                    qos,
                    dup: first & 0x08 != 0,
                    retain: first & 0x01 != 0,
                    pkid,
                    payload: r.rest().to_vec(),
                })
            }
            4 => Packet::PubAck(r.u16()?),
            8 => {
                let pkid = r.u16()?;
                let mut filters = Vec::new();
                while !r.rest().is_empty() {
                    let f = r.string()?;
                    let q = r.u8()?;
                    filters.push((f, q));
                }
                Packet::Subscribe { pkid, filters }
            }
            9 => Packet::SubAck {
                pkid: r.u16()?,
                codes: r.rest().to_vec(),
            },
            12 => Packet::PingReq,
            13 => Packet::PingResp,
            14 => Packet::Disconnect,
            t => return err(format!("unsupported packet type {t}")),
        };
        Ok(Some((p, total)))
    }
}

/// True if `topic` matches the subscription `filter` (`+` one level, `#`
/// the rest).
pub fn topic_matches(filter: &str, topic: &str) -> bool {
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Why a subscription filter is invalid, if it is.
pub fn filter_error(filter: &str) -> Option<String> {
    if filter.is_empty() {
        return Some("empty topic filter".into());
    }
    let levels: Vec<&str> = filter.split('/').collect();
    for (i, l) in levels.iter().enumerate() {
        if l.contains('#') && (*l != "#" || i + 1 != levels.len()) {
            return Some(format!("`{filter}`: `#` must be a whole, last level"));
        }
        if l.contains('+') && *l != "+" {
            return Some(format!("`{filter}`: `+` must be a whole level"));
        }
    }
    None
}

/// Why a topic name for publishing is invalid, if it is.
pub fn topic_error(topic: &str) -> Option<String> {
    if topic.is_empty() {
        Some("empty topic".into())
    } else if topic.contains(['+', '#']) {
        Some(format!("`{topic}`: a publish topic cannot contain + or #"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(p: Packet) {
        let b = p.encode();
        // Every split point: nothing until complete, then the packet.
        for cut in 0..b.len() {
            assert_eq!(
                Packet::parse(&b[..cut], MAX_REMAINING).unwrap(),
                None,
                "{p:?} at {cut}"
            );
        }
        let mut more = b.clone();
        more.extend_from_slice(&[0xC0, 0x00]);
        let (q, n) = Packet::parse(&more, MAX_REMAINING).unwrap().unwrap();
        assert_eq!(q, p);
        assert_eq!(n, b.len());
    }

    #[test]
    fn every_packet_round_trips() {
        round_trip(Packet::Connect(Connect {
            client_id: "bench-1".into(),
            clean_session: true,
            keep_alive: 30,
            username: Some("ops".into()),
            password: Some(b"s3cret".to_vec()),
        }));
        round_trip(Packet::Connect(Connect {
            client_id: String::new(),
            clean_session: false,
            keep_alive: 0,
            username: None,
            password: None,
        }));
        round_trip(Packet::ConnAck {
            session_present: true,
            code: 0,
        });
        for qos in [0, 1] {
            round_trip(Packet::Publish(Publish {
                topic: "dev/1/up".into(),
                qos,
                dup: qos == 1,
                retain: false,
                pkid: if qos == 1 { 513 } else { 0 },
                payload: vec![0, 1, 2, 0xFF],
            }));
        }
        round_trip(Packet::PubAck(7));
        round_trip(Packet::Subscribe {
            pkid: 3,
            filters: vec![("dev/+/up".into(), 1), ("log/#".into(), 0)],
        });
        round_trip(Packet::SubAck {
            pkid: 3,
            codes: vec![1, 0x80],
        });
        round_trip(Packet::PingReq);
        round_trip(Packet::PingResp);
        round_trip(Packet::Disconnect);
    }

    #[test]
    fn remaining_length_boundaries() {
        for len in [0usize, 127, 128, 16_383, 16_384, 2_097_151, 2_097_152] {
            let p = Packet::Publish(Publish {
                topic: "t".into(),
                qos: 0,
                dup: false,
                retain: false,
                pkid: 0,
                payload: vec![0xAB; len.saturating_sub(3)],
            });
            let b = p.encode();
            let (q, n) = Packet::parse(&b, MAX_REMAINING).unwrap().unwrap();
            assert_eq!((q, n), (p, b.len()), "{len}");
        }
        let big = Packet::Publish(Publish {
            topic: "t".into(),
            qos: 0,
            dup: false,
            retain: false,
            pkid: 0,
            payload: vec![0; 1000],
        });
        assert!(Packet::parse(&big.encode(), 100).is_err());
        assert!(Packet::parse(&[0x30, 0xFF, 0xFF, 0xFF, 0xFF, 0x01], MAX_REMAINING).is_err());
        assert!(
            Packet::parse(&[0x34, 0x03, 0, 1, b'a'], MAX_REMAINING).is_err(),
            "QoS 2"
        );
    }

    #[test]
    fn topics() {
        assert!(topic_matches("dev/+/up", "dev/7/up"));
        assert!(!topic_matches("dev/+/up", "dev/7/down"));
        assert!(!topic_matches("dev/+", "dev/7/up"));
        assert!(topic_matches("dev/#", "dev/7/up"));
        assert!(topic_matches("dev/#", "dev"));
        assert!(topic_matches("#", "a/b"));
        assert!(topic_matches("a/b", "a/b"));
        assert!(!topic_matches("a/b", "a/b/c"));
        assert!(filter_error("a/#/b").is_some());
        assert_eq!(
            filter_error("a/x+"),
            Some("`a/x+`: `+` must be a whole level".into())
        );
        assert!(filter_error("dev/+/up").is_none());
        assert!(topic_error("dev/+/down").is_some());
        assert!(topic_error("dev/1/down").is_none());
    }
}
