//! Transports: byte pipes to a device.
//!
//! All transports implement [`Transport`]: a blocking `read` that waits at
//! most a few milliseconds, and `write_all`. New link types (RTT, BLE, CAN,
//! MQTT) plug in by implementing the trait and adding a [`connector`] arm.

pub mod can;
mod loopback;
pub mod mqtt;
mod net;
pub mod replay;
mod serial;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub use loopback::{pair as loopback_pair, pair_with_mode as loopback_pair_with_mode, LoopbackEnd};
pub use net::{TcpClient, TcpServer, UdpTransport};
pub use replay::ReplayTransport;
pub use serial::{list_ports, pulse, resolve_port, select_port, PortInfo, SerialTransport};

use crate::config::{ConnectionProfile, FramingConfig, TransportKind};
use crate::error::CoreError;

/// How long a `read` may block when no data is available.
pub const READ_TIMEOUT: Duration = Duration::from_millis(5);

/// How long a `write_all` may block before it fails. A TCP write that times
/// out loses the link; a serial write that misses it fails only that request.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// A bidirectional byte link.
pub trait Transport: Send {
    /// Reads available bytes, blocking at most about [`READ_TIMEOUT`].
    /// `Ok(0)` means nothing arrived; an error means the link is gone.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Writes all bytes, blocking at most about [`WRITE_TIMEOUT`]. An error of
    /// kind `TimedOut`, `WouldBlock`, `NotConnected`, `InvalidInput` or
    /// `InvalidData` fails only this write; any other error means the link
    /// is gone.
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;
    /// Sets DTR (serial only).
    fn set_dtr(&mut self, _on: bool) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "DTR not supported by this transport",
        ))
    }
    /// Sets RTS (serial only).
    fn set_rts(&mut self, _on: bool) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "RTS not supported by this transport",
        ))
    }
    /// Short description of the endpoint.
    fn describe(&self) -> String;
    /// False while a server transport has no peer.
    fn has_peer(&self) -> bool {
        true
    }
    /// True if the transport ends by itself (replay at end of file).
    fn finished(&self) -> bool {
        false
    }
    /// Text lines that arrived on a side channel (e.g. an RTT printf
    /// channel), taken once. The session shows and records them like text
    /// passthrough lines. Called after every `read`.
    fn take_text(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// True if every `read` returns exactly one whole message (MQTT, CAN,
    /// UDP datagrams), so `framing.type = "none"` finds the boundaries.
    /// False for byte streams, which need a framer.
    fn message_based(&self) -> bool {
        false
    }
}

/// Opens a transport on demand; called again on reconnect.
pub type Connector = Box<dyn FnMut() -> Result<Box<dyn Transport>, CoreError> + Send>;

/// Factory for the in-process simulator transport (provided by `pb-sim`).
pub type SimFactory = Arc<dyn Fn() -> Result<Box<dyn Transport>, CoreError> + Send + Sync>;

/// Builds a connector for a profile of `main`. `sim` is required for
/// `transport = "sim"`. `port_override` replaces the serial port or
/// `host:port` for TCP/UDP.
pub fn connector(
    profile: &ConnectionProfile,
    project_dir: &Path,
    framing: &FramingConfig,
    sim: Option<SimFactory>,
    port_override: Option<String>,
) -> Result<Connector, CoreError> {
    connector_for(
        crate::config::MAIN_DEVICE,
        profile,
        project_dir,
        framing,
        sim,
        port_override,
    )
}

/// Like [`connector`], for device `device` (a replay plays that device's
/// records).
pub fn connector_for(
    device: &str,
    profile: &ConnectionProfile,
    project_dir: &Path,
    framing: &FramingConfig,
    sim: Option<SimFactory>,
    port_override: Option<String>,
) -> Result<Connector, CoreError> {
    let mut p = profile.clone();
    if let Some(o) = port_override {
        match p.transport {
            TransportKind::Serial => p.port = o,
            TransportKind::TcpClient => match o.rsplit_once(':') {
                Some((h, port)) => {
                    p.host = h.to_string();
                    p.tcp_port = port
                        .parse()
                        .map_err(|_| CoreError::Transport(format!("bad port in `{o}`")))?;
                }
                None => {
                    p.tcp_port = o
                        .parse()
                        .map_err(|_| CoreError::Transport(format!("bad port `{o}`")))?
                }
            },
            TransportKind::TcpServer => {
                p.tcp_port = o
                    .parse()
                    .map_err(|_| CoreError::Transport(format!("bad port `{o}`")))?
            }
            TransportKind::Udp => p.target = Some(o),
            TransportKind::Replay => p.file = Some(o),
            TransportKind::Mqtt => p.broker = o,
            TransportKind::Can => match p.can_backend {
                crate::config::CanBackend::Slcan => p.port = o,
                crate::config::CanBackend::Socketcan => p.interface = o,
            },
            TransportKind::Sim => {}
        }
    }
    Ok(match p.transport {
        TransportKind::Serial => {
            Box::new(move || Ok(Box::new(SerialTransport::open(&p)?) as Box<dyn Transport>))
        }
        TransportKind::TcpClient => Box::new(move || {
            Ok(Box::new(TcpClient::connect(&p.host, p.tcp_port)?) as Box<dyn Transport>)
        }),
        TransportKind::TcpServer => {
            let bind = p.bind.clone().unwrap_or_else(|| "0.0.0.0".into());
            let port = p.tcp_port;
            Box::new(move || Ok(Box::new(TcpServer::bind(&bind, port)?) as Box<dyn Transport>))
        }
        TransportKind::Udp => {
            let bind = p.bind.clone().unwrap_or_else(|| "0.0.0.0:0".into());
            let target = p.target.clone();
            Box::new(move || {
                Ok(Box::new(UdpTransport::bind(&bind, target.as_deref())?) as Box<dyn Transport>)
            })
        }
        TransportKind::Sim => {
            let f = sim.ok_or_else(|| {
                CoreError::Transport("simulator transport is not available in this build".into())
            })?;
            Box::new(move || f())
        }
        TransportKind::Mqtt => mqtt::connector(&p, project_dir)?,
        TransportKind::Can => can::connector(&p, project_dir)?,
        TransportKind::Replay => {
            let file = p
                .file
                .clone()
                .ok_or_else(|| CoreError::Transport("replay profile needs `file`".into()))?;
            let path = project_dir.join(file);
            let speed = p.speed;
            let framing = framing.clone();
            let device = device.to_string();
            Box::new(move || {
                Ok(Box::new(ReplayTransport::open_device(
                    &path, speed, &framing, &device,
                )?) as Box<dyn Transport>)
            })
        }
    })
}
