//! How a network board proves it is the board: [`Trust`].
//!
//! **A pinned board** has a self-signed certificate. The client trusts
//! exactly one certificate, identified by the SHA-256 of its DER encoding
//! (see `openboard_api::auth`). There is no certificate authority and no
//! host-name check.
//!
//! Matching the certificate bytes is not enough: a certificate is public, so
//! anyone can present a copy. The TLS 1.3 handshake signature is therefore
//! still verified against the pinned certificate's key with the provider's
//! real algorithms, which proves the peer holds the private key.
//!
//! **A trusted board** has a certificate for its host name from a
//! certificate authority. The client verifies the chain against this
//! machine's certificate authorities and checks the host name, as a browser
//! does, so the board can renew its certificate without telling anyone.
//!
//! Which of the two applies is settled before the connection, by whether
//! the invite, the stored login or the environment holds a fingerprint. It
//! never depends on what the server presents, there is no falling back from
//! one to the other, and nothing here can be switched off.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{Resumption, WebPkiServerVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use sha2::{Digest, Sha256};

/// Makes the ring provider the process default. reqwest looks the default
/// up whenever it builds a client, and panics if there is none.
pub fn install_provider() {
    // An error only means a provider is already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// What a client accepts as proof that the peer is the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// No certificate at all: the client of a plain-HTTP board, which can
    /// therefore never speak TLS to anything.
    Nothing,
    /// Only the certificate with this SHA-256 digest.
    Pin([u8; 32]),
    /// A certificate for the host name that this machine's certificate
    /// authorities vouch for.
    Authorities,
}

/// Why the verifier refused a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The peer presented another certificate. Holds its fingerprint.
    WrongCertificate { presented: String },
    /// The peer presented the pinned certificate but its handshake signature
    /// does not verify: it does not hold the certificate's private key.
    BadSignature,
    /// This client pins nothing, so it never completes a TLS handshake.
    NoPin,
    /// The board is known by its host name, and this machine's certificate
    /// authorities do not vouch for the certificate presented for it.
    Untrusted(Distrust),
}

/// What is wrong with a certificate the certificate authorities were asked
/// about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Distrust {
    /// No certificate authority this machine trusts signed it. A
    /// self-signed certificate ends here.
    UnknownIssuer,
    /// It has expired, or is not valid yet.
    Expired,
    /// It is for another host name.
    WrongName,
    /// Anything else, in the TLS library's words.
    Other(String),
}

impl Distrust {
    fn of(error: &rustls::Error) -> Self {
        use CertificateError as E;
        match error {
            rustls::Error::InvalidCertificate(reason) => match reason {
                E::UnknownIssuer => Self::UnknownIssuer,
                E::Expired
                | E::ExpiredContext { .. }
                | E::NotValidYet
                | E::NotValidYetContext { .. } => Self::Expired,
                E::NotValidForName | E::NotValidForNameContext { .. } => Self::WrongName,
                other => Self::Other(format!("{other:?}")),
            },
            other => Self::Other(other.to_string()),
        }
    }
}

/// What the verifier decided, readable after a failed connection. TLS errors
/// reach the caller wrapped several layers deep; this is the reliable way to
/// tell a pin failure from an ordinary network failure.
#[derive(Debug, Default)]
pub struct Verdict(Mutex<Option<Refusal>>);

impl Verdict {
    pub fn refusal(&self) -> Option<Refusal> {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn refuse(&self, refusal: Refusal) {
        *self.0.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(refusal);
    }
}

fn digest_of(cert: &CertificateDer<'_>) -> [u8; 32] {
    Sha256::digest(cert.as_ref()).into()
}

/// Trusts exactly one certificate, or none at all.
#[derive(Debug)]
struct Pin {
    digest: Option<[u8; 32]>,
    algorithms: WebPkiSupportedAlgorithms,
    verdict: Arc<Verdict>,
}

impl Pin {
    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let presented = digest_of(cert);
        let refusal = match self.digest {
            Some(pinned) if pinned == presented => return Ok(()),
            Some(_) => Refusal::WrongCertificate {
                presented: openboard_api::auth::fingerprint(&presented),
            },
            None => Refusal::NoPin,
        };
        self.verdict.refuse(refusal);
        Err(rustls::Error::InvalidCertificate(
            CertificateError::ApplicationVerificationFailure,
        ))
    }
}

impl ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // The configuration offers TLS 1.3 only, so this is never reached.
        Err(rustls::Error::General(
            "TLS 1.2 is not accepted by this client".into(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // The signature must be made by the key in the pinned certificate,
        // so the certificate is checked here again rather than assumed.
        self.check(cert)?;
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms).inspect_err(
            |_| {
                self.verdict.refuse(Refusal::BadSignature);
            },
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Asks the certificate authorities, and notes why they refused.
#[derive(Debug)]
struct Authorities {
    webpki: Arc<WebPkiServerVerifier>,
    verdict: Arc<Verdict>,
}

impl ServerCertVerifier for Authorities {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.webpki
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            .inspect_err(|error| {
                self.verdict.refuse(Refusal::Untrusted(Distrust::of(error)));
            })
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // The configuration offers TLS 1.3 only, so this is never reached.
        Err(rustls::Error::General(
            "TLS 1.2 is not accepted by this client".into(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.webpki
            .verify_tls13_signature(message, cert, dss)
            .inspect_err(|error| {
                self.verdict.refuse(Refusal::Untrusted(Distrust::of(error)));
            })
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

/// This machine's certificate authorities: the system's store, or the
/// bundle `SSL_CERT_FILE` names when it is set.
fn system_roots() -> Result<RootCertStore, String> {
    let found = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(found.certs);
    if roots.is_empty() {
        let why = found
            .errors
            .first()
            .map(|error| format!(": {error}"))
            .unwrap_or_default();
        return Err(format!(
            "this machine has no certificate authorities to check the board's certificate \
             against{why}; install the system's CA certificates (the `ca-certificates` package \
             on most Linux systems) or set SSL_CERT_FILE to a PEM bundle of them"
        ));
    }
    Ok(roots)
}

/// A TLS 1.3 client configuration that accepts what `trust` says and
/// nothing else.
pub fn client_config(trust: Trust) -> Result<(ClientConfig, Arc<Verdict>), String> {
    let roots = match trust {
        Trust::Authorities => Some(system_roots()?),
        Trust::Nothing | Trust::Pin(_) => None,
    };
    client_config_with(trust, roots)
}

/// [`client_config`] with the certificate authorities given, which
/// [`Trust::Authorities`] needs.
fn client_config_with(
    trust: Trust,
    roots: Option<RootCertStore>,
) -> Result<(ClientConfig, Arc<Verdict>), String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verdict = Arc::new(Verdict::default());
    let pin = |digest| Pin {
        digest,
        algorithms: provider.signature_verification_algorithms,
        verdict: Arc::clone(&verdict),
    };
    let verifier: Arc<dyn ServerCertVerifier> = match trust {
        Trust::Nothing => Arc::new(pin(None)),
        Trust::Pin(digest) => Arc::new(pin(Some(digest))),
        Trust::Authorities => {
            let roots = roots.ok_or("no certificate authorities were given")?;
            let webpki =
                WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                    .build()
                    .map_err(|error| error.to_string())?;
            Arc::new(Authorities {
                webpki,
                verdict: Arc::clone(&verdict),
            })
        }
    };
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| error.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    // Every connection goes through the full handshake and the verifier.
    config.resumption = Resumption::disabled();
    Ok((config, verdict))
}
