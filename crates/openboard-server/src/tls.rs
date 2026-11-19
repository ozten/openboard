//! TLS for network mode: the board's self-signed certificate, and a server
//! configuration that speaks TLS 1.3 and nothing else. The listener that
//! uses it is in `serve`.
//!
//! Clients pin the certificate by fingerprint (see `openboard_api::auth`),
//! so no certificate authority is involved and the names in the certificate
//! are informational.

use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use sha2::{Digest, Sha256};

/// File name of the board's certificate inside the data directory.
pub const CERT_FILE_NAME: &str = "tls-cert.pem";
/// File name of the certificate's private key inside the data directory.
/// Readable only by its owner.
pub const KEY_FILE_NAME: &str = "tls-key.pem";

pub(crate) struct TlsIdentity {
    pub config: Arc<ServerConfig>,
    /// `openboard_api::auth::fingerprint` of the certificate.
    pub fingerprint: String,
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
