//! TLS for network mode: the board's self-signed certificate and a listener
//! that speaks TLS 1.3 and nothing else.
//!
//! Clients pin the certificate by fingerprint (see `openboard_api::auth`),
//! so no certificate authority is involved and the names in the certificate
//! are informational.

use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::connect_info::Connected;
use axum::serve::IncomingStream;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinHandle;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

/// File name of the board's certificate inside the data directory.
pub const CERT_FILE_NAME: &str = "tls-cert.pem";
/// File name of the certificate's private key inside the data directory.
/// Readable only by its owner.
pub const KEY_FILE_NAME: &str = "tls-key.pem";

/// A client must finish its TLS handshake within this time.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Handshakes in progress at once. Further connections wait in the system's
/// accept queue, so slow clients cannot pile up unbounded work.
const MAX_HANDSHAKES: usize = 256;

pub(crate) struct TlsIdentity {
    pub config: Arc<ServerConfig>,
    /// `openboard_api::auth::fingerprint` of the certificate.
    pub fingerprint: String,
}

/// Loads the board's certificate and key from the data directory, creating
/// them on the first network start. `bind` only adds a name to a new
/// certificate. Blocks on disk I/O.
pub(crate) fn load_or_create(data_dir: &Path, bind: IpAddr) -> Result<TlsIdentity, String> {
    let cert_path = data_dir.join(CERT_FILE_NAME);
    let key_path = data_dir.join(KEY_FILE_NAME);
    match (cert_path.is_file(), key_path.is_file()) {
        (true, true) => {}
        (false, false) => generate(data_dir, bind)?,
        (true, false) => {
            return Err(format!(
                "{} exists but {} is missing; restore the key, or remove the certificate to \
                 make a new one (every client must then enrol again)",
                cert_path.display(),
                key_path.display()
            ))
        }
        (false, true) => {
            return Err(format!(
                "{} exists but {} is missing; restore the certificate, or remove the key to \
                 make a new pair (every client must then enrol again)",
                key_path.display(),
                cert_path.display()
            ))
        }
    }
    restrict_to_owner(&key_path)?;

    let cert = CertificateDer::from_pem_file(&cert_path)
        .map_err(|error| format!("cannot read {}: {error}", cert_path.display()))?;
    // The error is not passed on: it must never quote key material.
    let key = PrivateKeyDer::from_pem_file(&key_path)
        .map_err(|_| format!("cannot read a private key from {}", key_path.display()))?;
    let digest: [u8; 32] = Sha256::digest(cert.as_ref()).into();
    let fingerprint = openboard_api::auth::fingerprint(&digest);

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        // TLS 1.3 only. A client that offers nothing newer than 1.2 fails
        // the handshake.
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| format!("cannot configure TLS: {error}"))?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|error| format!("the certificate and key are not usable: {error}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsIdentity {
        config: Arc::new(config),
        fingerprint,
    })
}

fn generate(data_dir: &Path, bind: IpAddr) -> Result<(), String> {
    let failed = |error: rcgen::Error| format!("cannot generate a certificate: {error}");
    let mut names = vec![
        "localhost".to_owned(),
        Ipv4Addr::LOCALHOST.to_string(),
        Ipv6Addr::LOCALHOST.to_string(),
    ];
    if !bind.is_unspecified() && !bind.is_loopback() {
        names.push(bind.to_string());
    }
    let key = rcgen::KeyPair::generate().map_err(failed)?;
    let mut params = rcgen::CertificateParams::new(names).map_err(failed)?;
    let mut subject = rcgen::DistinguishedName::new();
    subject.push(rcgen::DnType::CommonName, "openboard");
    params.distinguished_name = subject;
    let cert = params.self_signed(&key).map_err(failed)?;

    // The key is written first and the certificate last, each under a
    // temporary name, so a crash leaves either nothing or a complete pair.
    write_private(
        &data_dir.join(KEY_FILE_NAME),
        key.serialize_pem().as_bytes(),
    )?;
    write_private(&data_dir.join(CERT_FILE_NAME), cert.pem().as_bytes())
}

/// Writes a new file that only its owner can read, atomically.
fn write_private(path: &Path, contents: &[u8]) -> Result<(), String> {
    let failed = |error: std::io::Error| format!("cannot write {}: {error}", path.display());
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = Path::new(&temp);
    match std::fs::remove_file(temp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(failed(error)),
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(temp).map_err(failed)?;
    file.write_all(contents).map_err(failed)?;
    file.sync_all().map_err(failed)?;
    drop(file);
    std::fs::rename(temp, path).map_err(failed)
}

/// Makes sure nobody but the owner can read the key, whatever happened to
/// the file's mode since it was written.
fn restrict_to_owner(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let failed = |error: std::io::Error| format!("cannot check {}: {error}", path.display());
        let mode = std::fs::metadata(path)
            .map_err(failed)?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            eprintln!(
                "openboard-server: {} was readable by other users; restricting it to its owner",
                path.display()
            );
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(failed)?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// The address of the client behind a request.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClientAddr(pub SocketAddr);

impl Connected<IncomingStream<'_, TlsListener>> for ClientAddr {
    fn connect_info(stream: IncomingStream<'_, TlsListener>) -> Self {
        ClientAddr(*stream.remote_addr())
    }
}

impl Connected<IncomingStream<'_, TcpListener>> for ClientAddr {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        ClientAddr(*stream.remote_addr())
    }
}

/// Accepts TCP connections and hands over only those that complete a TLS
/// handshake. Handshakes run beside the accept loop, so a slow or silent
/// client delays nobody else. A connection that does not speak TLS 1.3,
/// plain HTTP included, is dropped and never reaches a handler.
pub(crate) struct TlsListener {
    ready: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local_addr: SocketAddr,
    accepting: JoinHandle<()>,
}

impl TlsListener {
    pub(crate) fn new(tcp: TcpListener, config: Arc<ServerConfig>) -> std::io::Result<Self> {
        let local_addr = tcp.local_addr()?;
        let (tx, ready) = mpsc::channel(64);
        let accepting = tokio::spawn(accept_loop(tcp, TlsAcceptor::from(config), tx));
        Ok(Self {
            ready,
            local_addr,
            accepting,
        })
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        // Stops accepting and closes the socket.
        self.accepting.abort();
    }
}

async fn accept_loop(
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    ready: mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
) {
    let handshakes = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    loop {
        let Ok(permit) = Arc::clone(&handshakes).acquire_owned().await else {
            return;
        };
        let (stream, addr) = match tcp.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                use std::io::ErrorKind;
                let transient = matches!(
                    error.kind(),
                    ErrorKind::ConnectionRefused
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::ConnectionReset
                );
                if !transient {
                    // Usually the process is out of file descriptors. Wait
                    // for some to free up rather than spin.
                    eprintln!("openboard-server: cannot accept a connection: {error}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let (acceptor, ready) = (acceptor.clone(), ready.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await;
            if let Ok(Ok(tls)) = handshake {
                let _ = ready.send((tls, addr)).await;
            }
        });
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(connection) => connection,
            // The accept loop only ends when this listener is dropped.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}
