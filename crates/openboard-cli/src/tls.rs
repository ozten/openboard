//! Certificate pinning for network boards.
//!
//! A network board has a self-signed certificate. The client trusts exactly
//! one certificate, identified by the SHA-256 of its DER encoding (see
//! `openboard_api::auth`). There is no certificate authority and no host-name
//! check, and nothing here can be switched off.
//!
//! Matching the certificate bytes is not enough: a certificate is public, so
//! anyone can present a copy. The TLS 1.3 handshake signature is therefore
//! still verified against the pinned certificate's key with the provider's
//! real algorithms, which proves the peer holds the private key.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::Resumption;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

/// Makes the ring provider the process default. reqwest looks the default
/// up whenever it builds a client, and panics if there is none.
pub fn install_provider() {
    // An error only means a provider is already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
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

/// A TLS 1.3 client configuration that accepts only the certificate with
/// this SHA-256 digest. With `None` it accepts no certificate at all, which
/// is what the client for a plain-HTTP board is given so that it can never
/// speak TLS to anything.
pub fn client_config(pin: Option<[u8; 32]>) -> Result<(ClientConfig, Arc<Verdict>), rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verdict = Arc::new(Verdict::default());
    let verifier = Pin {
        digest: pin,
        algorithms: provider.signature_verification_algorithms,
        verdict: Arc::clone(&verdict),
    };
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    // Every connection goes through the full handshake and the verifier.
    config.resumption = Resumption::disabled();
    Ok((config, verdict))
}
