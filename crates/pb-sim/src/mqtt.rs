//! MQTT for the simulator: [`MiniBroker`], a small in-process MQTT 3.1.1
//! broker (QoS 0 and 1, persistent sessions, optional TLS, fault
//! injection) for tests and demos, and [`spawn_mqtt_device`], which runs a
//! project's simulator as an MQTT device.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;

use pb_core::clock::SystemClock;
use pb_core::config::{ConnectionProfile, TransportKind};
use pb_core::error::CoreError;
use pb_core::project::Project;
use pb_core::transport::mqtt::{
    topic_matches, MqttOptions, MqttTransport, Packet, Publish, SharedMqttState, MAX_REMAINING,
};
use pb_core::transport::Transport;

use crate::Simulator;

/// What a client is subscribed to: filter and granted QoS.
type Subs = Vec<(String, u8)>;

struct Client {
    id: String,
    subs: Subs,
    tx: Sender<Packet>,
    next_pkid: u16,
}

/// A disconnected persistent session (clean_session = false).
#[derive(Default)]
struct Saved {
    subs: Subs,
    queued: VecDeque<Publish>,
}

#[derive(Default)]
struct Inner {
    stop: AtomicBool,
    clients: Mutex<Vec<Client>>,
    saved: Mutex<HashMap<String, Saved>>,
    /// Incoming PUBLISHes to drop (no PUBACK, connection closed).
    drop_publishes: AtomicUsize,
    /// While set, the broker answers nothing (no PINGRESP, no CONNACK).
    mute: AtomicBool,
    publishes: AtomicU64,
    pings: AtomicU64,
    connections: AtomicU64,
}

/// A small MQTT broker on `127.0.0.1`. Dropping it stops it.
pub struct MiniBroker {
    addr: SocketAddr,
    inner: Arc<Inner>,
    accept: Option<JoinHandle<()>>,
}

impl MiniBroker {
    /// Starts a plain broker on a free port.
    pub fn start() -> io::Result<MiniBroker> {
        Self::start_with(None)
    }

    /// Starts a broker that speaks TLS with `config`.
    pub fn start_tls(config: Arc<rustls::ServerConfig>) -> io::Result<MiniBroker> {
        Self::start_with(Some(config))
    }

    fn start_with(tls: Option<Arc<rustls::ServerConfig>>) -> io::Result<MiniBroker> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let inner = Arc::new(Inner::default());
        let i2 = inner.clone();
        let accept = std::thread::Builder::new()
            .name("minibroker".into())
            .spawn(move || accept_loop(listener, i2, tls))?;
        Ok(MiniBroker {
            addr,
            inner,
            accept: Some(accept),
        })
    }

    /// `127.0.0.1:port`.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Drops the next `n` PUBLISH packets clients send (no PUBACK) and
    /// closes their connection, as a network failure would.
    pub fn drop_next_publishes(&self, n: usize) {
        self.inner.drop_publishes.store(n, Ordering::Relaxed);
    }

    /// Stops answering anything while `on`.
    pub fn mute(&self, on: bool) {
        self.inner.mute.store(on, Ordering::Relaxed);
    }

    /// PUBLISH packets received (and routed).
    pub fn publishes(&self) -> u64 {
        self.inner.publishes.load(Ordering::Relaxed)
    }

    /// PINGREQ packets received.
    pub fn pings(&self) -> u64 {
        self.inner.pings.load(Ordering::Relaxed)
    }

    /// Connections accepted (CONNECT packets).
    pub fn connections(&self) -> u64 {
        self.inner.connections.load(Ordering::Relaxed)
    }
}

impl Drop for MiniBroker {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

fn accept_loop(listener: TcpListener, inner: Arc<Inner>, tls: Option<Arc<rustls::ServerConfig>>) {
    while !inner.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((s, _)) => {
                let i = inner.clone();
                let tls = tls.clone();
                let _ = std::thread::Builder::new()
                    .name("minibroker-client".into())
                    .spawn(move || {
                        if let Err(e) = serve(s, &i, tls) {
                            tracing::debug!("minibroker client: {e}");
                        }
                    });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(b),
            Conn::Tls(s) => s.read(b),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(b),
            Conn::Tls(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

fn send(conn: &mut Conn, p: &Packet) -> io::Result<()> {
    conn.write_all(&p.encode())?;
    conn.flush()
}

/// Delivers a message to every connected client with a matching
/// subscription, and queues it for disconnected persistent sessions.
fn route(inner: &Inner, p: &Publish) {
    for c in inner.clients.lock().iter_mut() {
        let Some(q) = c
            .subs
            .iter()
            .filter(|(f, _)| topic_matches(f, &p.topic))
            .map(|(_, q)| *q)
            .max()
        else {
            continue;
        };
        let qos = q.min(p.qos);
        let pkid = if qos == 1 {
            c.next_pkid = c.next_pkid.wrapping_add(1).max(1);
            c.next_pkid
        } else {
            0
        };
        let _ = c.tx.send(Packet::Publish(Publish {
            topic: p.topic.clone(),
            qos,
            dup: false,
            retain: false,
            pkid,
            payload: p.payload.clone(),
        }));
    }
    for s in inner.saved.lock().values_mut() {
        if p.qos == 1
            && s.subs
                .iter()
                .any(|(f, q)| *q == 1 && topic_matches(f, &p.topic))
        {
            s.queued.push_back(p.clone());
        }
    }
}

fn serve(tcp: TcpStream, inner: &Inner, tls: Option<Arc<rustls::ServerConfig>>) -> io::Result<()> {
    tcp.set_nodelay(true)?;
    tcp.set_nonblocking(false)?;
    tcp.set_read_timeout(Some(Duration::from_millis(5)))?;
    let mut conn = match tls {
        Some(cfg) => {
            let sc = rustls::ServerConnection::new(cfg).map_err(io::Error::other)?;
            Conn::Tls(Box::new(rustls::StreamOwned::new(sc, tcp)))
        }
        None => Conn::Plain(tcp),
    };
    let (tx, rx): (Sender<Packet>, Receiver<Packet>) = crossbeam_channel::unbounded();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 16 * 1024];
    let mut me: Option<(String, bool)> = None; // (client id, clean session)
    let result = (|| -> io::Result<()> {
        loop {
            if inner.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            while let Ok(p) = rx.try_recv() {
                send(&mut conn, &p)?;
            }
            match conn.read(&mut tmp) {
                Ok(0) => return Ok(()),
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e),
            }
            while let Some((p, n)) = Packet::parse(&buf, MAX_REMAINING)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
            {
                buf.drain(..n);
                if inner.mute.load(Ordering::Relaxed) {
                    continue;
                }
                match p {
                    Packet::Connect(c) => {
                        inner.connections.fetch_add(1, Ordering::Relaxed);
                        let saved = if c.clean_session {
                            inner.saved.lock().remove(&c.client_id);
                            None
                        } else {
                            inner.saved.lock().remove(&c.client_id)
                        };
                        send(
                            &mut conn,
                            &Packet::ConnAck {
                                session_present: saved.is_some(),
                                code: 0,
                            },
                        )?;
                        let saved = saved.unwrap_or_default();
                        // Drop a stale connection with the same id.
                        inner.clients.lock().retain(|x| x.id != c.client_id);
                        inner.clients.lock().push(Client {
                            id: c.client_id.clone(),
                            subs: saved.subs,
                            tx: tx.clone(),
                            next_pkid: 0,
                        });
                        for mut q in saved.queued {
                            q.dup = false;
                            send(&mut conn, &Packet::Publish(q))?;
                        }
                        me = Some((c.client_id, c.clean_session));
                    }
                    Packet::Subscribe { pkid, filters } => {
                        let codes: Vec<u8> = filters.iter().map(|(_, q)| (*q).min(1)).collect();
                        if let Some((id, _)) = &me {
                            for cl in inner.clients.lock().iter_mut().filter(|c| &c.id == id) {
                                for (f, q) in &filters {
                                    cl.subs.retain(|(x, _)| x != f);
                                    cl.subs.push((f.clone(), (*q).min(1)));
                                }
                            }
                        }
                        send(&mut conn, &Packet::SubAck { pkid, codes })?;
                    }
                    Packet::Publish(m) => {
                        let drop = inner
                            .drop_publishes
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                                n.checked_sub(1)
                            })
                            .is_ok();
                        if drop {
                            return Ok(()); // lost before the PUBACK
                        }
                        inner.publishes.fetch_add(1, Ordering::Relaxed);
                        if m.qos == 1 {
                            send(&mut conn, &Packet::PubAck(m.pkid))?;
                        }
                        route(inner, &m);
                    }
                    Packet::PingReq => {
                        inner.pings.fetch_add(1, Ordering::Relaxed);
                        send(&mut conn, &Packet::PingResp)?;
                    }
                    Packet::Disconnect => return Ok(()),
                    _ => {}
                }
            }
        }
    })();
    if let Some((id, clean)) = me {
        let mut clients = inner.clients.lock();
        if let Some(i) = clients
            .iter()
            .position(|c| c.id == id && c.tx.same_channel(&tx))
        {
            let c = clients.remove(i);
            if !clean {
                inner.saved.lock().insert(
                    id,
                    Saved {
                        subs: c.subs,
                        queued: VecDeque::new(),
                    },
                );
            }
        }
    }
    result
}

/// The device side of an MQTT host profile: it subscribes to what the host
/// publishes and publishes where the host listens (`+` and `#` in the
/// host's first filter become `sim`).
pub fn device_profile(host: &ConnectionProfile) -> ConnectionProfile {
    let up = host
        .subscribe
        .first()
        .map(|f| {
            f.split('/')
                .map(|l| if l == "+" || l == "#" { "sim" } else { l })
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_else(|| "protobench/sim/up".into());
    ConnectionProfile {
        name: format!("{}-device", host.name),
        transport: TransportKind::Mqtt,
        client_id: String::new(),
        subscribe: host.publish.iter().cloned().collect(),
        publish: Some(up),
        ..host.clone()
    }
}

/// The first MQTT profile of a project, or the one named `name`.
pub fn mqtt_profile(project: &Project, name: Option<&str>) -> Result<ConnectionProfile, CoreError> {
    let profiles = &project.config.connection.profiles;
    let p = match name {
        Some(n) => profiles.iter().find(|p| p.name == n),
        None => profiles.iter().find(|p| p.transport == TransportKind::Mqtt),
    };
    match p {
        Some(p) if p.transport == TransportKind::Mqtt => Ok(p.clone()),
        Some(p) => Err(CoreError::Config(format!(
            "profile `{}` is not an mqtt profile",
            p.name
        ))),
        None => Err(CoreError::Config(
            "the project has no mqtt connection profile".into(),
        )),
    }
}

/// Runs the project's simulator as an MQTT device for the host profile
/// `host` (topics swapped, see [`device_profile`]) on `broker`, until
/// `stop`. It reconnects when the broker goes away.
pub fn spawn_mqtt_device(
    project: &Project,
    host: &ConnectionProfile,
    broker: &str,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, CoreError> {
    let schema = project.require_schema()?;
    let config = project.config.clone();
    let mut dev = device_profile(host);
    dev.broker = broker.to_string();
    let opts = MqttOptions::from_profile(&dev, &project.dir)?;
    let state = SharedMqttState::default();
    // Fail early when the broker cannot be reached at all.
    let first = MqttTransport::connect(opts.clone(), state.clone())?;
    Simulator::new(&config, &schema, SystemClock::shared())?;
    std::thread::Builder::new()
        .name("protobench-sim-mqtt".into())
        .spawn(move || {
            let mut transport: Option<Box<dyn Transport>> = Some(Box::new(first));
            while !stop.load(Ordering::Relaxed) {
                let t = match transport.take() {
                    Some(t) => t,
                    None => match MqttTransport::connect(opts.clone(), state.clone()) {
                        Ok(t) => Box::new(t),
                        Err(e) => {
                            tracing::warn!("simulator: {e}");
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                    },
                };
                match Simulator::new(&config, &schema, SystemClock::shared()) {
                    Ok(sim) => sim.run(t, stop.clone()),
                    Err(e) => {
                        tracing::warn!("simulator: {e}");
                        return;
                    }
                }
            }
        })
        .map_err(CoreError::IoBare)
}
