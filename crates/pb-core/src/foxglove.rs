//! Live Foxglove WebSocket server (`foxglove.websocket.v1`).
//!
//! Foxglove (or any client of the protocol) connects, receives `serverInfo`
//! and an `advertise` of every topic, subscribes to channels and then gets
//! binary message frames:
//!
//! ```text
//! 0x01 | u32 LE subscription id | u64 LE log time (Unix ns) | payload
//! ```
//!
//! Topics are the MCAP export's (see [`crate::topics`]): protobuf channels
//! carry their device's `FileDescriptorSet` as their schema, `/text` and
//! `/markers` are JSON. Topics that appear later are advertised on the fly.
//! With several devices, each device's topics live under its name
//! (`/ref/rx/reading`) and carry its own schema.
//!
//! One thread accepts connections and one thread serves each client,
//! consistent with the rest of `pb-core` (no async runtime). A client that
//! falls behind loses messages (counted) rather than slowing the session.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use base64::Engine as _;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use parking_lot::Mutex;
use serde_json::{json, Value as Json};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::{Message, WebSocket};

use crate::error::CoreError;
use crate::session::SessionEvent;
use crate::topics::{self, TopicInfo, TopicMessage};

/// WebSocket subprotocol.
pub const SUBPROTOCOL: &str = "foxglove.websocket.v1";
/// Opcode of a binary message-data frame.
pub const OP_MESSAGE_DATA: u8 = 0x01;
/// Messages queued per client before new ones are dropped.
pub const CLIENT_QUEUE: usize = 20_000;

#[derive(Debug, Clone)]
struct ChannelDef {
    id: u32,
    topic: String,
    encoding: &'static str,
    schema_name: String,
    schema: String,
    schema_encoding: &'static str,
}

impl ChannelDef {
    fn to_json(&self) -> Json {
        json!({
            "id": self.id,
            "topic": self.topic,
            "encoding": self.encoding,
            "schemaName": self.schema_name,
            "schema": self.schema,
            "schemaEncoding": self.schema_encoding,
        })
    }
}

enum Out {
    Text(String),
    Binary(Vec<u8>),
}

struct Client {
    tx: Sender<Out>,
    /// Subscription id → channel id.
    subs: Mutex<HashMap<u32, u32>>,
}

#[derive(Default)]
struct Registry {
    channels: Vec<ChannelDef>,
    by_topic: HashMap<String, u32>,
    clients: Vec<Arc<Client>>,
}

/// A device served by [`FoxgloveServer::start_devices`].
#[derive(Debug, Clone)]
pub struct FoxgloveDevice {
    /// Device name (`main` keeps the plain topics).
    pub name: String,
    /// Encoded `FileDescriptorSet` of the device's schema.
    pub fds: Vec<u8>,
    /// Topics to advertise from the start
    /// ([`crate::topics::known_topics_on`]).
    pub topics: Vec<TopicInfo>,
}

struct Inner {
    name: String,
    /// Device name and base64 descriptor set, by device index.
    schemas: Vec<(String, String)>,
    registry: Mutex<Registry>,
    stop: AtomicBool,
    published: AtomicU64,
    dropped: AtomicU64,
}

impl Inner {
    fn channel_def(
        &self,
        id: u32,
        topic: &str,
        schema_name: Option<&str>,
        device: usize,
    ) -> ChannelDef {
        match schema_name {
            Some(n) => ChannelDef {
                id,
                topic: topic.to_string(),
                encoding: "protobuf",
                schema_name: n.to_string(),
                schema: self
                    .schemas
                    .get(device)
                    .map(|s| s.1.clone())
                    .unwrap_or_default(),
                schema_encoding: "protobuf",
            },
            None => {
                let (name, schema) = if topic == topics::MARKER_TOPIC {
                    ("protobench.Marker", topics::MARKER_SCHEMA)
                } else {
                    ("protobench.Text", topics::TEXT_SCHEMA)
                };
                ChannelDef {
                    id,
                    topic: topic.to_string(),
                    encoding: "json",
                    schema_name: name.into(),
                    schema: schema.into(),
                    schema_encoding: "jsonschema",
                }
            }
        }
    }

    /// Channel id for a topic of device `device`, advertising it to every
    /// client if new.
    fn channel_for(&self, topic: &str, schema_name: Option<&str>, device: usize) -> u32 {
        let mut reg = self.registry.lock();
        if let Some(id) = reg.by_topic.get(topic) {
            return *id;
        }
        let id = reg.channels.len() as u32 + 1;
        let def = self.channel_def(id, topic, schema_name, device);
        let msg = json!({ "op": "advertise", "channels": [def.to_json()] }).to_string();
        for c in &reg.clients {
            let _ = c.tx.try_send(Out::Text(msg.clone()));
        }
        reg.by_topic.insert(topic.to_string(), id);
        reg.channels.push(def);
        id
    }

    fn publish(&self, m: &TopicMessage, log_time_ns: u64, device: usize) {
        let ch = self.channel_for(&m.topic, m.schema_name.as_deref(), device);
        let clients = self.registry.lock().clients.clone();
        for c in clients {
            let subs: Vec<u32> = c
                .subs
                .lock()
                .iter()
                .filter(|(_, &cid)| cid == ch)
                .map(|(&sid, _)| sid)
                .collect();
            for sid in subs {
                let mut frame = Vec::with_capacity(13 + m.data.len());
                frame.push(OP_MESSAGE_DATA);
                frame.extend_from_slice(&sid.to_le_bytes());
                frame.extend_from_slice(&log_time_ns.to_le_bytes());
                frame.extend_from_slice(&m.data);
                match c.tx.try_send(Out::Binary(frame)) {
                    Ok(()) => {
                        self.published.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Full(_)) => {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => {}
                }
            }
        }
    }
}

/// A running Foxglove WebSocket server. Dropping it stops the server.
pub struct FoxgloveServer {
    addr: SocketAddr,
    inner: Arc<Inner>,
    accept: Option<JoinHandle<()>>,
}

impl FoxgloveServer {
    /// Binds `bind:port` (port 0 picks a free one) and advertises `topics`.
    /// `fds` is the encoded `FileDescriptorSet` used as the protobuf schema.
    pub fn start(
        bind: &str,
        port: u16,
        name: &str,
        fds: &[u8],
        topics: &[TopicInfo],
    ) -> Result<FoxgloveServer, CoreError> {
        Self::start_devices(
            bind,
            port,
            name,
            &[FoxgloveDevice {
                name: crate::config::MAIN_DEVICE.into(),
                fds: fds.to_vec(),
                topics: topics.to_vec(),
            }],
        )
    }

    /// Like [`FoxgloveServer::start`] for several devices, each with its
    /// own schema and topics. Attach each device's session with
    /// [`FoxgloveServer::attach_device`].
    pub fn start_devices(
        bind: &str,
        port: u16,
        name: &str,
        devices: &[FoxgloveDevice],
    ) -> Result<FoxgloveServer, CoreError> {
        let listener = TcpListener::bind((bind, port))
            .map_err(|e| CoreError::Transport(format!("foxglove: bind {bind}:{port}: {e}")))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let inner = Arc::new(Inner {
            name: name.to_string(),
            schemas: devices
                .iter()
                .map(|d| {
                    (
                        d.name.clone(),
                        base64::engine::general_purpose::STANDARD.encode(&d.fds),
                    )
                })
                .collect(),
            registry: Mutex::new(Registry::default()),
            stop: AtomicBool::new(false),
            published: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        for (i, d) in devices.iter().enumerate() {
            for t in &d.topics {
                inner.channel_for(&t.topic, t.schema_name.as_deref(), i);
            }
        }
        let i2 = inner.clone();
        let accept = std::thread::Builder::new()
            .name("foxglove-accept".into())
            .spawn(move || accept_loop(listener, i2))?;
        tracing::info!("foxglove server on ws://{addr}");
        Ok(FoxgloveServer {
            addr,
            inner,
            accept: Some(accept),
        })
    }

    /// The bound address.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Connected clients.
    pub fn client_count(&self) -> usize {
        self.inner.registry.lock().clients.len()
    }

    /// Message frames queued to clients, and frames dropped because a client
    /// fell behind.
    pub fn counters(&self) -> (u64, u64) {
        (
            self.inner.published.load(Ordering::Relaxed),
            self.inner.dropped.load(Ordering::Relaxed),
        )
    }

    /// Publishes one message of `main` at `log_time_ns` (Unix ns).
    pub fn publish(&self, m: &TopicMessage, log_time_ns: u64) {
        self.inner.publish(m, log_time_ns, 0);
    }

    /// Forwards session events (decoded messages, text lines, markers) until
    /// the subscription closes or the server stops. `wall_offset_ns` turns
    /// session timestamps into Unix time
    /// ([`crate::session::Session::wall_offset_ns`]).
    pub fn attach(&self, events: Receiver<SessionEvent>, wall_offset_ns: u64) -> JoinHandle<()> {
        self.attach_device(events, wall_offset_ns, crate::config::MAIN_DEVICE)
    }

    /// Like [`FoxgloveServer::attach`] for the session of `device`, whose
    /// topics live under its name.
    pub fn attach_device(
        &self,
        events: Receiver<SessionEvent>,
        wall_offset_ns: u64,
        device: &str,
    ) -> JoinHandle<()> {
        let inner = self.inner.clone();
        let index = inner
            .schemas
            .iter()
            .position(|(n, _)| n == device)
            .unwrap_or(0);
        let device = device.to_string();
        std::thread::spawn(move || loop {
            if inner.stop.load(Ordering::Relaxed) {
                return;
            }
            let ev = match events.recv_timeout(Duration::from_millis(50)) {
                Ok(ev) => ev,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            };
            let (m, ts) = match ev {
                SessionEvent::Message { dir, msg } => {
                    (topics::for_message_on(&device, &msg, dir), msg.host_ts_ns)
                }
                SessionEvent::Text { host_ts_ns, line } => {
                    (topics::for_text_on(&device, &line), host_ts_ns)
                }
                SessionEvent::Marker { host_ts_ns, text } => {
                    (topics::for_marker(&text), host_ts_ns)
                }
                _ => continue,
            };
            inner.publish(&m, wall_offset_ns.saturating_add(ts), index);
        })
    }
}

impl Drop for FoxgloveServer {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

fn accept_loop(listener: TcpListener, inner: Arc<Inner>) {
    while !inner.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => {
                let i = inner.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("foxglove-{addr}"))
                    .spawn(move || {
                        if let Err(e) = serve_client(stream, &i) {
                            tracing::info!("foxglove client {addr}: {e}");
                        }
                    });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                tracing::warn!("foxglove accept: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}

/// Selects the Foxglove subprotocol when the client offers it.
#[allow(clippy::result_large_err)]
fn negotiate(req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
    let offered = req
        .headers()
        .get_all("Sec-WebSocket-Protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|p| p.trim() == SUBPROTOCOL);
    if offered {
        if let Ok(v) = SUBPROTOCOL.parse() {
            resp.headers_mut().insert("Sec-WebSocket-Protocol", v);
        }
    }
    Ok(resp)
}

fn serve_client(stream: TcpStream, inner: &Inner) -> Result<(), String> {
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);
    let mut ws = tungstenite::accept_hdr(stream, negotiate).map_err(|e| e.to_string())?;
    ws.get_mut()
        .set_read_timeout(Some(Duration::from_millis(5)))
        .map_err(|e| e.to_string())?;

    let (tx, rx) = crossbeam_channel::bounded(CLIENT_QUEUE);
    let client = Arc::new(Client {
        tx,
        subs: Mutex::new(HashMap::new()),
    });
    // Register and take the channel list under one lock, so a topic added
    // meanwhile is either in this advertise or sent separately, never lost.
    let advertise = {
        let mut reg = inner.registry.lock();
        reg.clients.push(client.clone());
        json!({
            "op": "advertise",
            "channels": reg.channels.iter().map(ChannelDef::to_json).collect::<Vec<_>>(),
        })
        .to_string()
    };
    let info = json!({
        "op": "serverInfo",
        "name": inner.name,
        "capabilities": [],
        "supportedEncodings": [],
        "metadata": {},
        "sessionId": format!("{}", std::process::id()),
    })
    .to_string();
    let result = (|| {
        ws.send(Message::text(info)).map_err(|e| e.to_string())?;
        ws.send(Message::text(advertise))
            .map_err(|e| e.to_string())?;
        client_loop(&mut ws, &client, &rx, inner)
    })();
    inner
        .registry
        .lock()
        .clients
        .retain(|c| !Arc::ptr_eq(c, &client));
    let _ = ws.close(None);
    let _ = ws.flush();
    result
}

fn client_loop(
    ws: &mut WebSocket<TcpStream>,
    client: &Client,
    rx: &Receiver<Out>,
    inner: &Inner,
) -> Result<(), String> {
    loop {
        if inner.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut wrote = false;
        while let Ok(o) = rx.try_recv() {
            let m = match o {
                Out::Text(t) => Message::text(t),
                Out::Binary(b) => Message::binary(b),
            };
            ws.write(m).map_err(|e| e.to_string())?;
            wrote = true;
        }
        if wrote {
            ws.flush().map_err(|e| e.to_string())?;
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                if let Some(reply) = handle_op(t.as_str(), client, inner) {
                    ws.send(Message::text(reply)).map_err(|e| e.to_string())?;
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(e) if is_timeout(&e) => {}
            Err(tungstenite::Error::ConnectionClosed) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn status(level: u8, message: String) -> String {
    json!({ "op": "status", "level": level, "message": message }).to_string()
}

/// Handles a client operation; returns a status reply for problems.
fn handle_op(text: &str, client: &Client, inner: &Inner) -> Option<String> {
    let Ok(v) = serde_json::from_str::<Json>(text) else {
        return Some(status(2, "invalid JSON".into()));
    };
    match v["op"].as_str() {
        Some("subscribe") => {
            let known: Vec<u32> = inner
                .registry
                .lock()
                .channels
                .iter()
                .map(|c| c.id)
                .collect();
            let mut bad = Vec::new();
            let mut subs = client.subs.lock();
            for s in v["subscriptions"].as_array().into_iter().flatten() {
                let (Some(id), Some(ch)) = (s["id"].as_u64(), s["channelId"].as_u64()) else {
                    bad.push(s.to_string());
                    continue;
                };
                if known.contains(&(ch as u32)) {
                    subs.insert(id as u32, ch as u32);
                } else {
                    bad.push(format!("channel {ch}"));
                }
            }
            (!bad.is_empty()).then(|| status(1, format!("cannot subscribe: {}", bad.join(", "))))
        }
        Some("unsubscribe") => {
            let mut subs = client.subs.lock();
            for id in v["subscriptionIds"].as_array().into_iter().flatten() {
                if let Some(id) = id.as_u64() {
                    subs.remove(&(id as u32));
                }
            }
            None
        }
        Some(op) => Some(status(1, format!("unsupported operation `{op}`"))),
        None => Some(status(2, "message has no `op`".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tungstenite::client::IntoClientRequest;

    fn connect(addr: SocketAddr) -> WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>> {
        let mut req = format!("ws://{addr}").into_client_request().unwrap();
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", SUBPROTOCOL.parse().unwrap());
        let (ws, resp) = tungstenite::connect(req).unwrap();
        assert_eq!(
            resp.headers().get("Sec-WebSocket-Protocol").unwrap(),
            SUBPROTOCOL
        );
        ws
    }

    fn text(ws: &mut WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>) -> Json {
        loop {
            if let Message::Text(t) = ws.read().unwrap() {
                return serde_json::from_str(t.as_str()).unwrap();
            }
        }
    }

    fn wait_clients(s: &FoxgloveServer, n: usize) {
        let t = std::time::Instant::now();
        while s.client_count() != n && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(s.client_count(), n);
    }

    #[test]
    fn advertise_subscribe_publish_and_dynamic_topics() {
        let topics = vec![
            TopicInfo {
                topic: "/rx/a".into(),
                schema_name: Some("demo.Telemetry".into()),
            },
            TopicInfo {
                topic: topics::TEXT_TOPIC.into(),
                schema_name: None,
            },
        ];
        let s = FoxgloveServer::start("127.0.0.1", 0, "test", b"fds", &topics).unwrap();
        let mut ws = connect(s.local_addr());
        let info = text(&mut ws);
        assert_eq!(info["op"], "serverInfo");
        assert_eq!(info["name"], "test");
        let adv = text(&mut ws);
        assert_eq!(adv["op"], "advertise");
        let chans = adv["channels"].as_array().unwrap();
        assert_eq!(chans.len(), 2);
        assert_eq!(chans[0]["topic"], "/rx/a");
        assert_eq!(chans[0]["encoding"], "protobuf");
        assert_eq!(chans[0]["schemaEncoding"], "protobuf");
        assert_eq!(chans[0]["schema"], "ZmRz"); // base64("fds")
        assert_eq!(chans[1]["encoding"], "json");
        assert_eq!(chans[1]["schemaEncoding"], "jsonschema");
        wait_clients(&s, 1);

        ws.send(Message::text(
            json!({"op": "subscribe", "subscriptions": [{"id": 7, "channelId": 1}, {"id": 8, "channelId": 99}]})
                .to_string(),
        ))
        .unwrap();
        let st = text(&mut ws);
        assert_eq!(st["op"], "status");
        assert!(st["message"].as_str().unwrap().contains("channel 99"));

        // Unsubscribed channels are not sent; subscribed ones are.
        s.publish(&topics::for_text("ignored"), 1);
        s.publish(
            &TopicMessage {
                topic: "/rx/a".into(),
                schema_name: Some("demo.Telemetry".into()),
                encoding: "protobuf",
                data: vec![0x10, 0x07],
            },
            1_700_000_000_123_456_789,
        );
        let bin = loop {
            if let Message::Binary(b) = ws.read().unwrap() {
                break b;
            }
        };
        assert_eq!(bin[0], OP_MESSAGE_DATA);
        assert_eq!(u32::from_le_bytes(bin[1..5].try_into().unwrap()), 7);
        assert_eq!(
            u64::from_le_bytes(bin[5..13].try_into().unwrap()),
            1_700_000_000_123_456_789
        );
        assert_eq!(&bin[13..], &[0x10, 0x07]);

        // A new topic is advertised to connected clients.
        s.publish(&topics::for_marker("m"), 2);
        let adv = text(&mut ws);
        assert_eq!(adv["op"], "advertise");
        assert_eq!(adv["channels"][0]["topic"], "/markers");
        assert_eq!(adv["channels"][0]["id"], 3);

        ws.close(None).unwrap();
        let _ = ws.flush();
        drop(ws);
        wait_clients(&s, 0);
    }
}
