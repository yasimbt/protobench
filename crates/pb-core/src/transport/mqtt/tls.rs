//! TCP or TLS stream for the MQTT client.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use crate::error::CoreError;

/// A plain or TLS connection.
pub enum Stream {
    /// Plain TCP.
    Plain(TcpStream),
    /// TLS over TCP.
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Stream {
    /// Starts TLS on `tcp` for `host`. The handshake happens with the first
    /// read or write.
    pub fn tls(tcp: TcpStream, config: Arc<ClientConfig>, host: &str) -> io::Result<Stream> {
        let name = ServerName::try_from(host.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{host}: {e}")))?;
        let conn = ClientConnection::new(config, name).map_err(io::Error::other)?;
        Ok(Stream::Tls(Box::new(StreamOwned::new(conn, tcp))))
    }

    /// The TCP socket underneath (for timeouts).
    pub fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(t) => t,
            Stream::Tls(s) => s.get_ref(),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(t) => t.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(t) => t.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(t) => t.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

fn certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, CoreError> {
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .map_err(|e| CoreError::Transport(format!("{}: {e}", path.display())))?;
    if certs.is_empty() {
        return Err(CoreError::Transport(format!(
            "{}: no PEM certificates",
            path.display()
        )));
    }
    Ok(certs)
}

/// A TLS client configuration that trusts the OS root store plus the
/// certificates in `ca`, with an optional client certificate for mutual
/// TLS.
pub fn client_config(
    ca: Option<&Path>,
    cert: Option<&Path>,
    key: Option<&Path>,
) -> Result<Arc<ClientConfig>, CoreError> {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for c in native.certs {
        let _ = roots.add(c);
    }
    if let Some(ca) = ca {
        for c in certs(ca)? {
            roots
                .add(c)
                .map_err(|e| CoreError::Transport(format!("{}: {e}", ca.display())))?;
        }
    }
    if roots.is_empty() {
        return Err(CoreError::Transport(
            "TLS: no trusted certificates (the OS store is empty; set tls_ca)".into(),
        ));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| CoreError::Transport(format!("TLS: {e}")))?
        .with_root_certificates(roots);
    let config = match (cert, key) {
        (Some(c), Some(k)) => {
            let key = PrivateKeyDer::from_pem_file(k)
                .map_err(|e| CoreError::Transport(format!("{}: {e}", k.display())))?;
            builder
                .with_client_auth_cert(certs(c)?, key)
                .map_err(|e| CoreError::Transport(format!("TLS client certificate: {e}")))?
        }
        _ => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}
