//! TLS for network mode: the board's certificate, and a server
//! configuration that speaks TLS 1.3 and nothing else. The listener that
//! uses it is in `serve`.
//!
//! A board has one of two kinds of certificate:
//!
//! - **Self-signed**, made on the first network start and kept in the data
//!   directory. Clients pin it by fingerprint (see `openboard_api::auth`),
//!   so no certificate authority is involved and the names in it are
//!   informational.
//! - **Supplied** by the operator, who obtained it for the board's host
//!   name from a certificate authority that clients already trust. Clients
//!   check the chain and the host name, as a browser does, and pin nothing.
//!   The board reads the files again when they change (see [`Reloader`]),
//!   so a renewal needs no restart. The board never fetches a certificate
//!   itself.

use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::{ClientHello, ParsedCertificate, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

/// File name of the board's certificate inside the data directory.
pub const CERT_FILE_NAME: &str = "tls-cert.pem";
/// File name of the certificate's private key inside the data directory.
/// Readable only by its owner.
pub const KEY_FILE_NAME: &str = "tls-key.pem";

pub(crate) struct TlsIdentity {
    pub config: Arc<ServerConfig>,
    /// `openboard_api::auth::fingerprint` of a self-signed certificate,
    /// which clients pin. `None` for a supplied certificate.
    pub fingerprint: Option<String>,
    /// Set for a supplied certificate: reads the files again as they change.
    pub reloader: Option<Reloader>,
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// TLS 1.3 only. A client that offers nothing newer than 1.2 fails the
/// handshake.
fn server_config(resolver: Arc<dyn ResolvesServerCert>) -> Result<Arc<ServerConfig>, String> {
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| format!("cannot configure TLS: {error}"))?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Answers every handshake with one certificate, which can be replaced
/// while the server runs. A connection that is already open keeps the
/// certificate it was opened with.
#[derive(Debug)]
struct Current(RwLock<Arc<CertifiedKey>>);

impl Current {
    fn replace(&self, key: Arc<CertifiedKey>) {
        *self.0.write().unwrap_or_else(|poison| poison.into_inner()) = key;
    }
}

impl ResolvesServerCert for Current {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(
            &self.0.read().unwrap_or_else(|poison| poison.into_inner()),
        ))
    }
}

/// A certificate and key the operator supplied, and the host name the
/// certificate must be valid for.
#[derive(Debug, Clone)]
pub(crate) struct Supplied {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// The host of the board's public URL.
    pub host: String,
}

/// The files as they were at one moment: a digest of both, to tell whether
/// they changed since.
type Stamp = [u8; 32];

impl Supplied {
    /// Reads the certificate chain and the key, and checks that they belong
    /// together and that the certificate names the board's host. Whether a
    /// client trusts the chain is for the client to say. Blocks on disk I/O.
    ///
    /// The stamp comes back even when the pair is unusable, as long as both
    /// files could be read.
    fn read(&self) -> (Option<Stamp>, Result<CertifiedKey, String>) {
        let cert_pem = match std::fs::read(&self.cert_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                let path = self.cert_path.display();
                return (None, Err(format!("cannot read {path}: {error}")));
            }
        };
        let key_pem = match std::fs::read(&self.key_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                let path = self.key_path.display();
                return (None, Err(format!("cannot read {path}: {error}")));
            }
        };
        let mut hasher = Sha256::new();
        hasher.update((cert_pem.len() as u64).to_be_bytes());
        hasher.update(&cert_pem);
        hasher.update(&key_pem);
        let stamp: Stamp = hasher.finalize().into();
        (Some(stamp), self.parse(&cert_pem, &key_pem))
    }

    fn parse(&self, cert_pem: &[u8], key_pem: &[u8]) -> Result<CertifiedKey, String> {
        let cert_path = self.cert_path.display();
        let key_path = self.key_path.display();
        // The whole chain is served: the board's certificate first, then
        // the intermediates, as certbot's `fullchain.pem` has them.
        let chain = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("cannot read a certificate from {cert_path}: {error}"))?;
        let Some(leaf) = chain.first() else {
            return Err(format!("{cert_path} holds no certificate"));
        };
        let parsed = ParsedCertificate::try_from(leaf)
            .map_err(|error| format!("the certificate in {cert_path} is not usable: {error}"))?;
        let name = ServerName::try_from(self.host.as_str())
            .map_err(|_| format!("{} is not a host name a certificate can name", self.host))?;
        rustls::client::verify_server_name(&parsed, &name).map_err(|_| {
            format!(
                "the certificate in {cert_path} is not for {}, the host of the board's public                  URL, so no client would accept it",
                self.host
            )
        })?;
        // The errors are not passed on: they must never quote key material.
        let key = PrivateKeyDer::from_pem_slice(key_pem)
            .map_err(|_| format!("cannot read a private key from {key_path}"))?;
        let key = provider().key_provider.load_private_key(key).map_err(|_| {
            format!("the private key in {key_path} is of a kind TLS 1.3 cannot use")
        })?;
        let certified = CertifiedKey::new(chain, key);
        match certified.keys_match() {
            // `Unknown`: the key cannot say what its public half is.
            Ok(()) | Err(rustls::Error::InconsistentKeys(rustls::InconsistentKeys::Unknown)) => {
                Ok(certified)
            }
            Err(_) => Err(format!(
                "the private key in {key_path} does not belong to the certificate in {cert_path}"
            )),
        }
    }
}

/// Serves a certificate the operator supplied. Fails when the pair cannot
/// be used as it is now; a later change is picked up by the [`Reloader`].
/// Blocks on disk I/O.
pub(crate) fn load_supplied(supplied: Supplied, interval: Duration) -> Result<TlsIdentity, String> {
    let (stamp, pair) = supplied.read();
    let current = Arc::new(Current(RwLock::new(Arc::new(pair?))));
    Ok(TlsIdentity {
        config: server_config(Arc::clone(&current) as Arc<dyn ResolvesServerCert>)?,
        fingerprint: None,
        reloader: Some(Reloader {
            supplied,
            current,
            seen: stamp,
            complaint: None,
            interval,
        }),
    })
}

/// Puts a renewed certificate into service without a restart.
///
/// The board makes no outbound requests, so another tool renews the
/// certificate and replaces the files. This reads them every
/// [`crate::Config::tls_reload_interval`], and when their content has
/// changed and the new pair is usable, new connections get it. Open
/// connections, long-polls and streams included, are not touched.
///
/// A pair that cannot be used leaves the old certificate in service: a
/// file caught half-written, or a certificate that arrived before its key,
/// is tried again when the files next change, and the reason is written to
/// stderr once.
pub(crate) struct Reloader {
    supplied: Supplied,
    current: Arc<Current>,
    /// The files as last read, usable or not.
    seen: Option<Stamp>,
    /// The last failure written to stderr, so it is not repeated.
    complaint: Option<String>,
    interval: Duration,
}

impl Reloader {
    pub(crate) async fn run(mut self, mut stop: watch::Receiver<bool>) {
        loop {
            tokio::select! {
                () = crate::http::stopping(&mut stop) => return,
                () = tokio::time::sleep(self.interval) => {}
            }
            let supplied = self.supplied.clone();
            let Ok((stamp, pair)) = tokio::task::spawn_blocking(move || supplied.read()).await
            else {
                continue;
            };
            self.consider(stamp, pair);
        }
    }

    fn consider(&mut self, stamp: Option<Stamp>, pair: Result<CertifiedKey, String>) {
        if stamp.is_some() && stamp == self.seen {
            return;
        }
        match pair {
            Ok(pair) => {
                self.current.replace(Arc::new(pair));
                self.seen = stamp;
                self.complaint = None;
                eprintln!(
                    "openboard-server: the certificate in {} changed and is now in use",
                    self.supplied.cert_path.display()
                );
            }
            Err(reason) => {
                // An unreadable file has no stamp and is read again next
                // time; an unusable pair is not looked at again until it
                // changes.
                if stamp.is_some() {
                    self.seen = stamp;
                }
                if self.complaint.as_deref() != Some(reason.as_str()) {
                    eprintln!(
                        "openboard-server: the certificate files changed but cannot be used,                          so the certificate in service stays: {reason}"
                    );
                    self.complaint = Some(reason);
                }
            }
        }
    }
}

/// Loads the board's certificate and key from the data directory, creating
/// them on the first network start. `bind` only adds a name to a new
/// certificate. Blocks on disk I/O.
///
/// `pinned` says whether a fingerprint was ever recorded for this board,
/// which a network start does once it has its certificate. Until then no
/// client can have pinned anything, so a key without a certificate, which is
/// what a crash in the middle of [`generate`] leaves, is replaced by a new
/// pair. Afterwards half a pair is an error for the operator to resolve,
/// because a new certificate would silently change the pin.
pub(crate) fn load_or_create(
    data_dir: &Path,
    bind: IpAddr,
    pinned: bool,
) -> Result<TlsIdentity, String> {
    let cert_path = data_dir.join(CERT_FILE_NAME);
    let key_path = data_dir.join(KEY_FILE_NAME);
    match (cert_path.is_file(), key_path.is_file()) {
        (true, true) => {}
        (false, false) => generate(data_dir, bind)?,
        (false, true) if !pinned => generate(data_dir, bind)?,
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

    let key = provider()
        .key_provider
        .load_private_key(key)
        .map_err(|error| format!("the certificate and key are not usable: {error}"))?;
    let certified = CertifiedKey::new(vec![cert], key);
    Ok(TlsIdentity {
        config: server_config(Arc::new(Current(RwLock::new(Arc::new(certified)))))?,
        fingerprint: Some(fingerprint),
        reloader: None,
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

    // Each file is written under a temporary name and renamed, so neither is
    // ever seen half-written. The key comes first and the certificate last:
    // a crash between the two leaves a key without a certificate, never a
    // certificate that could be pinned without its key, and the next start
    // replaces that key (see `load_or_create`).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_left_by_an_interrupted_first_start_is_replaced() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().to_path_buf();
        let bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let key_path = dir.join(KEY_FILE_NAME);
        let cert_path = dir.join(CERT_FILE_NAME);

        // What a crash between the two writes leaves behind.
        std::fs::write(&key_path, "half a pair").unwrap();
        let first = load_or_create(&dir, bind, false)
            .unwrap_or_else(|error| panic!("a new pair is made: {error}"));
        assert!(cert_path.is_file());
        assert_ne!(std::fs::read(&key_path).unwrap(), b"half a pair");
        // The pair is then kept.
        let again = load_or_create(&dir, bind, false).unwrap();
        assert_eq!(again.fingerprint, first.fingerprint);

        // Once clients may have pinned the certificate, losing it is an
        // error, not a reason to make another.
        let key = std::fs::read(&key_path).unwrap();
        std::fs::remove_file(&cert_path).unwrap();
        assert!(load_or_create(&dir, bind, true).is_err());
        assert_eq!(std::fs::read(&key_path).unwrap(), key);
        assert!(!cert_path.exists());
    }
}
