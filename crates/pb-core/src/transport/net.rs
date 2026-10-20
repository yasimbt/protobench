//! TCP client, TCP server and UDP transports.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use super::{Transport, READ_TIMEOUT, WRITE_TIMEOUT};
use crate::error::CoreError;

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// TCP client.
pub struct TcpClient {
    stream: TcpStream,
    peer: String,
}

impl TcpClient {
    /// Connects with a 2 s timeout.
    pub fn connect(host: &str, port: u16) -> Result<TcpClient, CoreError> {
        let peer = format!("{host}:{port}");
        let addrs: Vec<SocketAddr> = peer
            .to_socket_addrs()
            .map_err(|e| CoreError::Transport(format!("{peer}: {e}")))?
            .collect();
        let mut last = None;
        for a in addrs {
            match TcpStream::connect_timeout(&a, Duration::from_secs(2)) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(READ_TIMEOUT))?;
                    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
                    stream.set_nodelay(true)?;
                    return Ok(TcpClient { stream, peer });
                }
                Err(e) => last = Some(e),
            }
        }
        Err(CoreError::Transport(format!(
            "{peer}: {}",
            last.map(|e| e.to_string())
                .unwrap_or_else(|| "no address".into())
        )))
    }
}

impl Transport for TcpClient {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.stream.read(buf) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "connection closed by peer",
            )),
            Ok(n) => Ok(n),
            Err(e) if is_timeout(&e) => Ok(0),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(0),
            Err(e) => Err(e),
        }
    }
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        // A timed-out write may have sent part of a frame, so the stream is
        // no longer usable: report a lost link and reconnect.
        self.stream.write_all(data).map_err(|e| {
            if is_timeout(&e) {
                io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    format!("peer stopped reading (write blocked for {WRITE_TIMEOUT:?})"),
                )
            } else {
                e
            }
        })
    }
    fn describe(&self) -> String {
        format!("tcp://{}", self.peer)
    }
}

/// TCP server: listens and serves one client at a time. When the client
/// leaves, it goes back to accepting.
pub struct TcpServer {
    listener: TcpListener,
    client: Option<(TcpStream, SocketAddr)>,
}

impl TcpServer {
    /// Binds `host:port` (port 0 picks a free one).
    pub fn bind(host: &str, port: u16) -> Result<TcpServer, CoreError> {
        let listener = TcpListener::bind((host, port))
            .map_err(|e| CoreError::Transport(format!("bind {host}:{port}: {e}")))?;
        listener.set_nonblocking(true)?;
        Ok(TcpServer {
            listener,
            client: None,
        })
    }

    /// The bound address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    fn accept(&mut self) -> io::Result<()> {
        match self.listener.accept() {
            Ok((s, addr)) => {
                s.set_nonblocking(false)?;
                s.set_read_timeout(Some(READ_TIMEOUT))?;
                s.set_write_timeout(Some(WRITE_TIMEOUT))?;
                s.set_nodelay(true)?;
                tracing::info!("tcp server: client {addr} connected");
                self.client = Some((s, addr));
                Ok(())
            }
            Err(e) if is_timeout(&e) => {
                std::thread::sleep(READ_TIMEOUT);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

impl Transport for TcpServer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.client.is_none() {
            self.accept()?;
            return Ok(0);
        }
        let Some((stream, _)) = self.client.as_mut() else {
            return Ok(0);
        };
        match stream.read(buf) {
            Ok(0) => {
                self.client = None;
                Ok(0)
            }
            Ok(n) => Ok(n),
            Err(e) if is_timeout(&e) || e.kind() == io::ErrorKind::Interrupted => Ok(0),
            Err(_) => {
                self.client = None;
                Ok(0)
            }
        }
    }
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        match self.client.as_mut() {
            Some((s, addr)) => match s.write_all(data) {
                Ok(()) => Ok(()),
                // A failed or timed-out write drops this client; the server
                // keeps listening for the next one.
                Err(e) => {
                    tracing::info!("tcp server: dropping client {addr}: {e}");
                    self.client = None;
                    Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        format!("client dropped: {e}"),
                    ))
                }
            },
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "no client connected",
            )),
        }
    }
    fn describe(&self) -> String {
        let local = self
            .listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        match &self.client {
            Some((_, a)) => format!("tcp server {local} (client {a})"),
            None => format!("tcp server {local} (waiting)"),
        }
    }
    fn has_peer(&self) -> bool {
        self.client.is_some()
    }
}

/// UDP socket. Sends to `target`, or to the sender of the last datagram.
pub struct UdpTransport {
    socket: UdpSocket,
    target: Option<SocketAddr>,
    fixed_target: bool,
}

impl UdpTransport {
    /// Binds `bind` (`host:port`) and optionally fixes the target.
    pub fn bind(bind: &str, target: Option<&str>) -> Result<UdpTransport, CoreError> {
        let socket =
            UdpSocket::bind(bind).map_err(|e| CoreError::Transport(format!("bind {bind}: {e}")))?;
        socket.set_read_timeout(Some(READ_TIMEOUT))?;
        let target = match target {
            Some(t) => Some(
                t.to_socket_addrs()
                    .map_err(|e| CoreError::Transport(format!("{t}: {e}")))?
                    .next()
                    .ok_or_else(|| CoreError::Transport(format!("{t}: no address")))?,
            ),
            None => None,
        };
        Ok(UdpTransport {
            socket,
            fixed_target: target.is_some(),
            target,
        })
    }

    /// The bound address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

impl Transport for UdpTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.socket.recv_from(buf) {
            Ok((n, from)) => {
                if !self.fixed_target {
                    self.target = Some(from);
                }
                Ok(n)
            }
            Err(e) if is_timeout(&e) || e.kind() == io::ErrorKind::Interrupted => Ok(0),
            // ICMP port unreachable from an earlier send shows up here on some
            // systems; it is not fatal for a connectionless socket.
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => Ok(0),
            Err(e) => Err(e),
        }
    }
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        match self.target {
            Some(t) => self.socket.send_to(data, t).map(|_| ()),
            None => Ok(()),
        }
    }
    fn describe(&self) -> String {
        let local = self
            .socket
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        match self.target {
            Some(t) => format!("udp {local} -> {t}"),
            None => format!("udp {local} (no peer yet)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_some(t: &mut dyn Transport, want: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        for _ in 0..400 {
            let n = t.read(&mut buf).unwrap();
            out.extend_from_slice(&buf[..n]);
            if out.len() >= want {
                break;
            }
        }
        out
    }

    #[test]
    fn tcp_client_server() {
        let mut server = TcpServer::bind("127.0.0.1", 0).unwrap();
        let port = server.local_addr().unwrap().port();
        assert!(!server.has_peer());
        let mut client = TcpClient::connect("127.0.0.1", port).unwrap();
        for _ in 0..200 {
            let mut b = [0u8; 8];
            server.read(&mut b).unwrap();
            if server.has_peer() {
                break;
            }
        }
        assert!(server.has_peer());
        client.write_all(b"ping").unwrap();
        assert_eq!(read_some(&mut server, 4), b"ping");
        server.write_all(b"pong").unwrap();
        assert_eq!(read_some(&mut client, 4), b"pong");
        drop(server);
        let mut b = [0u8; 8];
        let mut closed = false;
        for _ in 0..200 {
            if client.read(&mut b).is_err() {
                closed = true;
                break;
            }
        }
        assert!(closed);
    }

    #[test]
    fn udp_learns_peer() {
        let mut a = UdpTransport::bind("127.0.0.1:0", None).unwrap();
        let a_addr = a.local_addr().unwrap().to_string();
        let mut b = UdpTransport::bind("127.0.0.1:0", Some(&a_addr)).unwrap();
        b.write_all(b"hi").unwrap();
        assert_eq!(read_some(&mut a, 2), b"hi");
        a.write_all(b"yo").unwrap();
        assert_eq!(read_some(&mut b, 2), b"yo");
    }
}
