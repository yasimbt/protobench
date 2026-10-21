//! MQTT 3.1.1 transport: a small client on a blocking socket (no async
//! runtime), with optional TLS.
//!
//! Every PUBLISH received on a subscribed topic is one frame (use
//! `framing.type = "none"`); every frame the session writes is one PUBLISH
//! to the `publish` topic. QoS 0 and 1 are supported. Unacknowledged QoS 1
//! messages survive a reconnect: the connector keeps them and the next
//! connection sends them again with the DUP flag, so nothing is lost when
//! the broker or the network drops (it may arrive twice).

mod packet;
mod tls;

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

pub use packet::{
    filter_error, topic_error, topic_matches, Connect, Packet, PacketError, Publish, MAX_REMAINING,
};
pub use tls::{client_config, Stream};

use super::{Transport, READ_TIMEOUT, WRITE_TIMEOUT};
use crate::config::ConnectionProfile;
use crate::error::CoreError;

/// How long connecting, CONNACK and SUBACK may take.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Most QoS 1 messages kept for redelivery.
pub const OUTBOX_MAX: usize = 10_000;

/// Connection settings, resolved from a profile.
#[derive(Clone)]
pub struct MqttOptions {
    /// `host:port`.
    pub broker: String,
    /// Host name (TLS server name).
    pub host: String,
    /// Client id.
    pub client_id: String,
    /// User name.
    pub username: Option<String>,
    /// Password.
    pub password: Option<Vec<u8>>,
    /// TLS configuration, if TLS is on.
    pub tls: Option<Arc<rustls::ClientConfig>>,
    /// Keep-alive in seconds (0 = off).
    pub keep_alive: u16,
    /// Topic filters to subscribe to.
    pub subscribe: Vec<String>,
    /// Topic to publish to.
    pub publish: Option<String>,
    /// QoS 0 or 1.
    pub qos: u8,
    /// Largest packet accepted.
    pub max_packet: usize,
}

impl MqttOptions {
    /// Resolves a profile: default port, password from the environment,
    /// TLS files relative to `project_dir`.
    pub fn from_profile(p: &ConnectionProfile, project_dir: &Path) -> Result<Self, CoreError> {
        let broker = p.broker.trim();
        if broker.is_empty() {
            return Err(CoreError::Transport("mqtt profile has no `broker`".into()));
        }
        let default_port = if p.tls { 8883 } else { 1883 };
        let (host, broker) = match broker.rsplit_once(':') {
            Some((h, port)) if port.parse::<u16>().is_ok() && !h.is_empty() => {
                (h.trim_matches(['[', ']']).to_string(), broker.to_string())
            }
            _ => (broker.to_string(), format!("{broker}:{default_port}")),
        };
        let password = match &p.password_env {
            Some(var) => Some(
                std::env::var(var)
                    .map_err(|_| {
                        CoreError::Transport(format!(
                            "mqtt: environment variable `{var}` (password_env) is not set"
                        ))
                    })?
                    .into_bytes(),
            ),
            None => p.password.clone().map(String::into_bytes),
        };
        let tls = if p.tls {
            let file = |f: &Option<String>| f.as_ref().map(|x| project_dir.join(x));
            Some(client_config(
                file(&p.tls_ca).as_deref(),
                file(&p.tls_cert).as_deref(),
                file(&p.tls_key).as_deref(),
            )?)
        } else {
            None
        };
        let client_id = if p.client_id.trim().is_empty() {
            // Stable for the connector's life, so a persistent session
            // (QoS 1) survives reconnects.
            format!("protobench-{:08x}", random_u32())
        } else {
            p.client_id.trim().to_string()
        };
        Ok(MqttOptions {
            broker,
            host,
            client_id,
            username: p.username.clone(),
            password,
            tls,
            keep_alive: p.keep_alive_s,
            subscribe: p.subscribe.clone(),
            publish: p.publish.clone(),
            qos: p.qos.min(1),
            max_packet: MAX_REMAINING,
        })
    }
}

fn random_u32() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (t as u32) ^ ((t >> 32) as u32) ^ std::process::id().rotate_left(16)
}

/// State that outlives one connection: QoS 1 messages not yet acknowledged
/// and the packet-id counter.
#[derive(Default)]
pub struct MqttState {
    outbox: VecDeque<Publish>,
    next_pkid: u16,
}

impl MqttState {
    fn pkid(&mut self) -> u16 {
        self.next_pkid = self.next_pkid.wrapping_add(1).max(1);
        self.next_pkid
    }

    /// QoS 1 messages waiting for their PUBACK.
    pub fn unacked(&self) -> usize {
        self.outbox.len()
    }
}

/// Shared [`MqttState`].
pub type SharedMqttState = Arc<Mutex<MqttState>>;

/// A connected MQTT client.
pub struct MqttTransport {
    stream: Stream,
    opts: MqttOptions,
    state: SharedMqttState,
    buf: Vec<u8>,
    inbox: VecDeque<Vec<u8>>,
    last_sent: Instant,
    last_rcvd: Instant,
    ping_out: Option<Instant>,
}

fn io_err(kind: io::ErrorKind, m: impl Into<String>) -> io::Error {
    io::Error::new(kind, m.into())
}

fn refused(code: u8) -> &'static str {
    match code {
        1 => "unacceptable protocol version",
        2 => "client id rejected",
        3 => "server unavailable",
        4 => "bad user name or password",
        5 => "not authorized",
        _ => "refused",
    }
}

impl MqttTransport {
    /// Connects, sends CONNECT, subscribes and re-sends unacknowledged QoS 1
    /// messages from `state`.
    pub fn connect(opts: MqttOptions, state: SharedMqttState) -> Result<MqttTransport, CoreError> {
        let broker = opts.broker.clone();
        let t = |m: String| CoreError::Transport(format!("mqtt {broker}: {m}"));
        let addrs: Vec<_> = opts
            .broker
            .to_socket_addrs()
            .map_err(|e| t(e.to_string()))?
            .collect();
        let mut last = None;
        let mut tcp = None;
        for a in addrs {
            match TcpStream::connect_timeout(&a, SETUP_TIMEOUT) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let tcp = tcp.ok_or_else(|| {
            t(last
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no address".into()))
        })?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(Duration::from_millis(50)))?;
        tcp.set_write_timeout(Some(SETUP_TIMEOUT))?;
        let stream = match &opts.tls {
            Some(cfg) => Stream::tls(tcp, cfg.clone(), &opts.host).map_err(|e| t(e.to_string()))?,
            None => Stream::Plain(tcp),
        };
        let now = Instant::now();
        let mut me = MqttTransport {
            stream,
            opts,
            state,
            buf: Vec::new(),
            inbox: VecDeque::new(),
            last_sent: now,
            last_rcvd: now,
            ping_out: None,
        };
        // QoS 1 keeps a persistent session so the broker queues messages
        // while the link is down; QoS 0 starts clean.
        let connect = Packet::Connect(Connect {
            client_id: me.opts.client_id.clone(),
            clean_session: me.opts.qos == 0,
            keep_alive: me.opts.keep_alive,
            username: me.opts.username.clone(),
            password: me.opts.password.clone(),
        });
        me.send(&connect).map_err(|e| t(e.to_string()))?;
        let deadline = Instant::now() + SETUP_TIMEOUT;
        match me.next_packet(deadline).map_err(|e| t(e.to_string()))? {
            Packet::ConnAck { code: 0, .. } => {}
            Packet::ConnAck { code, .. } => {
                return Err(t(format!(
                    "broker refused the connection: {} (code {code})",
                    refused(code)
                )))
            }
            other => return Err(t(format!("expected CONNACK, got {other:?}"))),
        }
        if !me.opts.subscribe.is_empty() {
            let pkid = me.state.lock().pkid();
            let filters = me
                .opts
                .subscribe
                .iter()
                .map(|f| (f.clone(), me.opts.qos))
                .collect();
            me.send(&Packet::Subscribe { pkid, filters })
                .map_err(|e| t(e.to_string()))?;
            loop {
                match me.next_packet(deadline).map_err(|e| t(e.to_string()))? {
                    Packet::SubAck { pkid: id, codes } if id == pkid => {
                        if let Some(i) = codes.iter().position(|c| *c == 0x80) {
                            let f = me.opts.subscribe.get(i).cloned().unwrap_or_default();
                            return Err(t(format!("broker refused the subscription to `{f}`")));
                        }
                        break;
                    }
                    other => me.handle(other).map_err(|e| t(e.to_string()))?,
                }
            }
        }
        // Messages the last connection could not get acknowledged.
        let pending: Vec<Publish> = me.state.lock().outbox.iter().cloned().collect();
        for mut p in pending {
            p.dup = true;
            me.send(&Packet::Publish(p)).map_err(|e| t(e.to_string()))?;
        }
        me.stream.tcp().set_read_timeout(Some(READ_TIMEOUT))?;
        me.stream.tcp().set_write_timeout(Some(WRITE_TIMEOUT))?;
        tracing::info!(
            "mqtt: connected to {} as {}",
            me.opts.broker,
            me.opts.client_id
        );
        Ok(me)
    }

    /// The shared state (for tests: unacknowledged messages).
    pub fn state(&self) -> &SharedMqttState {
        &self.state
    }

    fn send(&mut self, p: &Packet) -> io::Result<()> {
        // A write that times out may have sent half a packet: the stream is
        // unusable, so it counts as a lost link.
        self.stream.write_all(&p.encode()).map_err(|e| {
            if matches!(
                e.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) {
                io_err(
                    io::ErrorKind::ConnectionAborted,
                    format!("broker stopped reading (write blocked for {WRITE_TIMEOUT:?})"),
                )
            } else {
                e
            }
        })?;
        self.stream.flush()?;
        self.last_sent = Instant::now();
        Ok(())
    }

    /// Reads from the socket once; false if nothing arrived.
    fn fill(&mut self) -> io::Result<bool> {
        let mut tmp = [0u8; 16 * 1024];
        match self.stream.read(&mut tmp) {
            Ok(0) => Err(io_err(
                io::ErrorKind::ConnectionAborted,
                "broker closed the connection",
            )),
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                self.last_rcvd = Instant::now();
                Ok(true)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    fn parse(&mut self) -> io::Result<Option<Packet>> {
        match Packet::parse(&self.buf, self.opts.max_packet) {
            Ok(Some((p, n))) => {
                self.buf.drain(..n);
                Ok(Some(p))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(io_err(io::ErrorKind::InvalidData, e.to_string())),
        }
    }

    /// Waits for the next packet until `deadline` (setup only).
    fn next_packet(&mut self, deadline: Instant) -> io::Result<Packet> {
        loop {
            if let Some(p) = self.parse()? {
                return Ok(p);
            }
            if Instant::now() >= deadline {
                return Err(io_err(io::ErrorKind::TimedOut, "no answer from the broker"));
            }
            self.fill()?;
        }
    }

    fn handle(&mut self, p: Packet) -> io::Result<()> {
        match p {
            Packet::Publish(m) => {
                if m.qos == 1 {
                    self.send(&Packet::PubAck(m.pkid))?;
                }
                self.inbox.push_back(m.payload);
            }
            Packet::PubAck(id) => self.state.lock().outbox.retain(|p| p.pkid != id),
            Packet::PingResp => self.ping_out = None,
            _ => {}
        }
        Ok(())
    }

    /// Sends PINGREQ when idle; fails when the broker stopped answering.
    fn keep_alive(&mut self) -> io::Result<()> {
        if self.opts.keep_alive == 0 {
            return Ok(());
        }
        let ka = Duration::from_secs(u64::from(self.opts.keep_alive));
        if let Some(sent) = self.ping_out {
            if sent.elapsed() > ka / 2 + Duration::from_secs(1) {
                return Err(io_err(
                    io::ErrorKind::TimedOut,
                    format!("broker did not answer a ping within {:?}", ka / 2),
                ));
            }
        } else if self.last_sent.elapsed() >= ka * 3 / 4 {
            self.send(&Packet::PingReq)?;
            self.ping_out = Some(Instant::now());
        }
        Ok(())
    }
}

impl Transport for MqttTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.inbox.is_empty() {
            self.fill()?;
            while let Some(p) = self.parse()? {
                self.handle(p)?;
            }
            self.keep_alive()?;
        }
        let Some(m) = self.inbox.pop_front() else {
            return Ok(0);
        };
        if m.len() > buf.len() {
            tracing::warn!(
                "mqtt: dropped a {}-byte message (larger than max_frame_len)",
                m.len()
            );
            return Ok(0);
        }
        buf[..m.len()].copy_from_slice(&m);
        Ok(m.len())
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let Some(topic) = self.opts.publish.clone() else {
            return Err(io_err(
                io::ErrorKind::NotConnected,
                "this MQTT profile has no `publish` topic; it only receives",
            ));
        };
        let qos = self.opts.qos;
        let pkid = if qos == 1 {
            self.state.lock().pkid()
        } else {
            0
        };
        let p = Publish {
            topic,
            qos,
            dup: false,
            retain: false,
            pkid,
            payload: data.to_vec(),
        };
        if qos == 1 {
            let mut st = self.state.lock();
            if st.outbox.len() >= OUTBOX_MAX {
                st.outbox.pop_front();
            }
            st.outbox.push_back(p.clone());
        }
        self.send(&Packet::Publish(p))
    }

    fn describe(&self) -> String {
        format!(
            "mqtt{}://{} (pub {}, sub {})",
            if self.opts.tls.is_some() { "s" } else { "" },
            self.opts.broker,
            self.opts.publish.as_deref().unwrap_or("-"),
            if self.opts.subscribe.is_empty() {
                "-".to_string()
            } else {
                self.opts.subscribe.join(", ")
            }
        )
    }

    fn message_based(&self) -> bool {
        true
    }
}

impl Drop for MqttTransport {
    fn drop(&mut self) {
        let _ = self.stream.write_all(&Packet::Disconnect.encode());
        let _ = self.stream.flush();
    }
}

/// A connector for an MQTT profile. The QoS 1 outbox and the client id
/// are shared by every connection it makes.
pub fn connector(
    profile: &ConnectionProfile,
    project_dir: &Path,
) -> Result<super::Connector, CoreError> {
    let opts = MqttOptions::from_profile(profile, project_dir)?;
    let state: SharedMqttState = Arc::default();
    Ok(Box::new(move || {
        Ok(Box::new(MqttTransport::connect(opts.clone(), state.clone())?) as Box<dyn Transport>)
    }))
}
