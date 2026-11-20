//! A board at a host name with a supplied certificate (milestone M5), as a
//! browser meets it: a client that verifies the certificate against a
//! certificate authority and the host name, pins nothing, and sends the
//! `Cookie` and `Origin` headers itself.

mod common;

use std::sync::Arc;
use std::time::Duration;

use openboard_api::web::{session_cookie_name, Session, CSRF_HEADER};
use openboard_api::{codes, ErrorBody};
use openboard_server::operator::{self, InviteOptions};
use openboard_server::{Config, Limits};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use reqwest::header::{COOKIE, ORIGIN, SET_COOKIE, STRICT_TRANSPORT_SECURITY};
use reqwest::{Client, StatusCode};
use rustls::pki_types::CertificateDer;
use rustls::{ClientConfig, RootCertStore};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::oneshot;

struct Hosted {
    dir: TempDir,
    url: String,
    port: u16,
    /// Trusts the test's certificate authority and nothing else.
    http: Client,
    _stop: oneshot::Sender<()>,
}

async fn hosted() -> Hosted {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = TempDir::new().unwrap();

    // A certificate authority, and a certificate from it for `localhost`.
    let authority_key = KeyPair::generate().unwrap();
    let mut authority = CertificateParams::new(Vec::<String>::new()).unwrap();
    authority.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    authority.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    authority
        .distinguished_name
        .push(DnType::CommonName, "openboard test authority");
    let authority_der = authority
        .self_signed(&authority_key)
        .unwrap()
        .der()
        .to_vec();
    let issuer = Issuer::new(authority, authority_key);
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    let (cert_path, key_path) = (dir.path().join("fullchain.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key.serialize_pem()).unwrap();

    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let url = format!("https://localhost:{port}");
    let mut config = Config::new(dir.path().join("data"));
    config.port = port;
    config.network = true;
    config.open = true;
    config.public_url = Some(url.clone());
    config.tls_cert = Some(cert_path);
    config.tls_key = Some(key_path);
    config.limits = Limits {
        credential_recheck: Duration::from_millis(100),
        ..Limits::default()
    };
    let server = openboard_server::bind(config).await.unwrap();
    assert_eq!(server.url(), url);
    assert_eq!(server.fingerprint(), None);
    assert!(server.certificate_expires_at().is_some());
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));

    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(authority_der)).unwrap();
    let tls =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    let http = Client::builder()
        .no_proxy()
        .use_preconfigured_tls(tls)
        .build()
        .unwrap();
    Hosted {
        dir,
        url,
        port,
        http,
        _stop: stop,
    }
}

#[tokio::test]
async fn a_person_signs_in_at_the_host_name_and_the_session_belongs_to_it() {
    let board = hosted().await;
    let own_origin = board.url.clone();

    // The certificate checks out for the name, with nothing pinned, and
    // every answer tells the browser to keep to HTTPS for this host.
    for path in ["/v1/health", "/", "/join", "/v1/whoami"] {
        let response = board
            .http
            .get(format!("{}{path}", board.url))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response
                .headers()
                .get(STRICT_TRANSPORT_SECURITY)
                .and_then(|value| value.to_str().ok()),
            Some("max-age=31536000"),
            "{path}"
        );
    }
    // It does not check out for the address the board listens on.
    let by_address = format!("https://127.0.0.1:{}/v1/health", board.port);
    assert!(board.http.get(by_address).send().await.is_err());

    // The sign-in link names the host.
    let data_dir = board.dir.path().join("data");
    let invite =
        operator::create_person_invite(&data_dir, "ada@example.com", &InviteOptions::default())
            .unwrap();
    assert_eq!(invite.link, format!("{own_origin}/join#{}", invite.code));

    // Signing in, as the page does it: a write from the board's own origin.
    let response = board
        .http
        .post(format!("{}/v1/web/login", board.url))
        .header(ORIGIN, &own_origin)
        .json(&json!({ "code": invite.code }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let set_cookie = response
        .headers()
        .get(SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    // The cookie is held to this exact host: `__Host-`, `Secure`, `Path=/`
    // and no `Domain`.
    let name = session_cookie_name(board.port);
    assert!(set_cookie.starts_with(&format!("{name}=")), "{set_cookie}");
    for attribute in ["Secure", "HttpOnly", "SameSite=Strict", "Path=/"] {
        assert!(set_cookie.contains(attribute), "{set_cookie}");
    }
    assert!(
        !set_cookie.to_ascii_lowercase().contains("domain"),
        "{set_cookie}"
    );
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    let session: Session = response.json().await.unwrap();
    assert_eq!(session.actor.handle, "ada@example.com");
    let csrf = session.csrf_token.unwrap();

    let post = |origin: &str| {
        board
            .http
            .post(format!("{}/v1/boards/general/messages", board.url))
            .header(COOKIE, &cookie)
            .header(CSRF_HEADER, &csrf)
            .header(ORIGIN, origin)
            .json(&json!({ "channel": "notes", "text": "hello from a browser" }))
            .send()
    };
    // The page's own origin writes.
    let response = post(&own_origin).await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    // Another origin does not: another site, plain HTTP on the same name,
    // another port on it, and the board's address in place of its name.
    for foreign in [
        "https://evil.example".to_owned(),
        format!("http://localhost:{}", board.port),
        "https://localhost:1".to_owned(),
        format!("https://127.0.0.1:{}", board.port),
    ] {
        let response = post(&foreign).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{foreign}");
        let body: ErrorBody = response.json().await.unwrap();
        assert_eq!(body.error.code, codes::CSRF_FAILED, "{foreign}");
    }
}

#[tokio::test]
async fn a_board_with_a_self_signed_certificate_does_not_ask_for_https_only() {
    let board = common::Board::closed().await;
    let response = board.http.get(board.at("/v1/health")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get(STRICT_TRANSPORT_SECURITY).is_none());
}
