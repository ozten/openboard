//! A network board over HTTPS for the integration tests of grants and
//! isolation, and the helpers they share.
#![allow(dead_code)]
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use openboard_api::auth::{parse_fingerprint, JoinResponse};
use openboard_api::{ErrorBody, PostMessageResponse};
use openboard_server::operator::{self, InviteOptions};
use openboard_server::{Config, Limits, ServeError};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

#[derive(Debug)]
struct Pin {
    digest: [u8; 32],
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let digest: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if digest == self.digest {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("certificate pin mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

pub fn pinned_client(fingerprint: &str) -> Client {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let pin = Pin {
        digest: parse_fingerprint(fingerprint).unwrap(),
        algorithms: provider.signature_verification_algorithms,
    };
    let tls = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::ALL_VERSIONS)
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(pin))
        .with_no_client_auth();
    Client::builder()
        .no_proxy()
        .use_preconfigured_tls(tls)
        .build()
        .unwrap()
}

pub struct Board {
    pub dir: TempDir,
    pub addr: SocketAddr,
    pub http: Client,
    _stop: oneshot::Sender<()>,
    _task: JoinHandle<Result<(), ServeError>>,
}

impl Board {
    /// A fresh network board: closed, so nobody is admitted until granted.
    pub async fn closed() -> Self {
        Self::start(false).await
    }

    pub async fn start(open: bool) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = TempDir::new().unwrap();
        let mut config = Config::new(dir.path().join("data"));
        config.port = 0;
        config.network = true;
        config.open = open;
        config.limits = Limits {
            requests_per_minute: 100_000,
            joins_per_minute: 100_000,
            auth_failures_per_minute: 100_000,
            credential_recheck: Duration::from_millis(100),
            ..Limits::default()
        };
        let server = openboard_server::bind(config).await.unwrap();
        let addr = server.local_addr();
        let http = pinned_client(server.fingerprint().unwrap());
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        Self {
            dir,
            addr,
            http,
            _stop: stop,
            _task: task,
        }
    }

    pub fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    pub fn at(&self, path: &str) -> String {
        format!("https://127.0.0.1:{}{path}", self.addr.port())
    }

    pub async fn enrol(&self, name: &str) -> String {
        let invite = operator::create_invite(&self.data_dir(), name, &InviteOptions::default())
            .unwrap()
            .invite;
        let response = self
            .http
            .post(self.at("/v1/join"))
            .json(&json!({ "code": invite.code }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<JoinResponse>().await.unwrap().token
    }

    /// The operator's grant, on the data directory.
    pub fn grant(&self, pattern: &str, who: &str, verbs: &str) -> String {
        operator::grant(&self.data_dir(), "general", pattern, who, verbs, None)
            .unwrap()
            .id
    }

    pub fn get(&self, token: &str, path: &str) -> RequestBuilder {
        self.http.get(self.at(path)).bearer_auth(token)
    }

    pub fn post_json(&self, token: &str, path: &str, body: Value) -> RequestBuilder {
        self.http.post(self.at(path)).bearer_auth(token).json(&body)
    }

    pub async fn post(&self, token: &str, channel: &str, text: &str) -> Response {
        self.post_json(
            token,
            "/v1/boards/general/messages",
            json!({ "channel": channel, "text": text }),
        )
        .send()
        .await
        .unwrap()
    }

    pub async fn posted(&self, token: &str, channel: &str, text: &str) -> PostMessageResponse {
        let response = self.post(token, channel, text).await;
        assert!(
            response.status().is_success(),
            "posting to {channel}: {}",
            response.status()
        );
        response.json().await.unwrap()
    }

    /// The channels `token` sees in the board's channel listing.
    pub async fn channels_seen(&self, token: &str) -> Vec<String> {
        let response = self
            .get(token, "/v1/boards/general/channels")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let list: Value = response.json().await.unwrap();
        list["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|channel| channel["path"].as_str().unwrap().to_owned())
            .collect()
    }

    pub async fn add_grant(&self, token: &str, pattern: &str, who: &str, verbs: &str) -> Response {
        self.post_json(
            token,
            "/v1/boards/general/grants",
            json!({ "pattern": pattern, "grantee": who, "verbs": verbs }),
        )
        .send()
        .await
        .unwrap()
    }
}

pub async fn error_of(response: Response, status: StatusCode, code: &str) -> ErrorBody {
    assert_eq!(response.status(), status);
    let body: ErrorBody = response.json().await.unwrap();
    assert_eq!(body.error.code, code, "{body:?}");
    body
}

impl Board {
    pub async fn mint(&self, token: &str, body: Value) -> Response {
        self.post_json(token, "/v1/tokens", body)
            .send()
            .await
            .unwrap()
    }

    pub async fn minted(&self, token: &str, can: &[&str]) -> openboard_api::tokens::TokenMinted {
        let response = self
            .mint(token, json!({ "board": "general", "can": can }))
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        response.json().await.unwrap()
    }
}
