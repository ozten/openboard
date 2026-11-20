//! End-to-end tests of network mode: a real server on port 0, reached over
//! real TLS by a client that trusts exactly the board's certificate.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use openboard_api::auth::{
    parse_fingerprint, CredentialInfo, Invite, JoinResponse, WhoAmI, MODE_LOCAL, MODE_NETWORK,
    TOKEN_PREFIX,
};
use openboard_api::{
    codes, ActorKind, ErrorBody, Event, EventsResponse, Health, PostMessageResponse, Thread,
    API_LEVEL, API_LEVEL_HEADER, PROFILE_HEADER, VERSION_HEADER,
};
use openboard_server::operator::{self, log_kinds, InviteOptions, OperatorError};
use openboard_server::{
    verify_log, Config, Limits, LogRetention, Mode, ServeError, CERT_FILE_NAME, DB_FILE_NAME,
    KEY_FILE_NAME,
};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme, SupportedProtocolVersion};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// A client that pins the board's certificate
// ---------------------------------------------------------------------------

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Trusts exactly one certificate, by the SHA-256 of its DER encoding, and
/// still checks the handshake signature against that certificate's key.
#[derive(Debug)]
struct Pin {
    digest: [u8; 32],
    algorithms: WebPkiSupportedAlgorithms,
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

fn tls_config(digest: [u8; 32], versions: &[&'static SupportedProtocolVersion]) -> ClientConfig {
    let provider = provider();
    let pin = Pin {
        digest,
        algorithms: provider.signature_verification_algorithms,
    };
    ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(pin))
        .with_no_client_auth()
}

fn pinned_client(fingerprint: &str) -> Client {
    let digest = parse_fingerprint(fingerprint).expect("a well-formed fingerprint");
    Client::builder()
        .no_proxy()
        .use_preconfigured_tls(tls_config(digest, rustls::ALL_VERSIONS))
        .build()
        .unwrap()
}

// ---------------------------------------------------------------------------
// A running network board
// ---------------------------------------------------------------------------

struct Net {
    dir: TempDir,
    addr: SocketAddr,
    url: String,
    fingerprint: String,
    http: Client,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), ServeError>>,
}

fn data_dir_of(dir: &TempDir) -> PathBuf {
    dir.path().join("data")
}

/// Limits that stay out of the way of tests that are not about them.
fn roomy(config: &mut Config) {
    config.limits = Limits {
        requests_per_minute: 100_000,
        joins_per_minute: 100_000,
        auth_failures_per_minute: 100_000,
        max_waiters: 100,
        credential_recheck: Duration::from_millis(100),
        ..Limits::default()
    };
}

impl Net {
    async fn start() -> Self {
        Self::start_in(TempDir::new().unwrap(), |_| {}).await
    }

    async fn start_with(tweak: impl FnOnce(&mut Config)) -> Self {
        Self::start_in(TempDir::new().unwrap(), tweak).await
    }

    async fn start_in(dir: TempDir, tweak: impl FnOnce(&mut Config)) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut config = Config::new(data_dir_of(&dir));
        config.port = 0;
        config.network = true;
        roomy(&mut config);
        tweak(&mut config);
        let server = openboard_server::bind(config).await.unwrap();
        assert_eq!(server.mode(), Mode::Network);
        let addr = server.local_addr();
        let url = server.url().to_owned();
        let fingerprint = server.fingerprint().expect("a fingerprint").to_owned();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        Self {
            dir,
            addr,
            http: pinned_client(&fingerprint),
            url,
            fingerprint,
            stop: Some(stop),
            task,
        }
    }

    fn data_dir(&self) -> PathBuf {
        data_dir_of(&self.dir)
    }

    /// The board by its loopback address, whatever its public URL says.
    fn at(&self, path: &str) -> String {
        format!("https://127.0.0.1:{}{path}", self.addr.port())
    }

    async fn stop(mut self) -> TempDir {
        self.stop.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("server stops promptly")
            .unwrap()
            .unwrap();
        self.dir
    }

    fn get(&self, token: &str, path: &str) -> RequestBuilder {
        self.http.get(self.at(path)).bearer_auth(token)
    }

    fn post_request(&self, token: &str, board: &str, body: &Value) -> RequestBuilder {
        self.http
            .post(self.at(&format!("/v1/boards/{board}/messages")))
            .bearer_auth(token)
            .json(body)
    }

    async fn post(&self, token: &str, profile: Option<&str>, body: Value) -> PostMessageResponse {
        let mut request = self.post_request(token, "general", &body);
        if let Some(profile) = profile {
            request = request.header(PROFILE_HEADER, profile);
        }
        let response = request.send().await.unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        response.json().await.unwrap()
    }

    async fn thread(&self, token: &str, channel: &str, text: &str) -> PostMessageResponse {
        self.post(token, None, json!({"channel": channel, "text": text}))
            .await
    }

    async fn status(&self, token: &str) -> StatusCode {
        self.get(token, "/v1/whoami").send().await.unwrap().status()
    }

    async fn events(&self, token: &str, profile: Option<&str>, query: &str) -> EventsResponse {
        let mut request = self.get(token, &format!("/v1/boards/general/events?{query}"));
        if let Some(profile) = profile {
            request = request.header(PROFILE_HEADER, profile);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    fn invite(&self, name: &str) -> Invite {
        operator::create_invite(&self.data_dir(), name, &InviteOptions::default())
            .unwrap()
            .invite
    }

    async fn join_raw(&self, code: &str) -> Response {
        self.http
            .post(self.at("/v1/join"))
            .json(&json!({ "code": code }))
            .send()
            .await
            .unwrap()
    }

    /// Invites `name` and enrols it. Returns the token and the credential.
    async fn enrol(&self, name: &str) -> (String, CredentialInfo) {
        let invite = self.invite(name);
        let response = self.join_raw(&invite.code).await;
        assert_eq!(response.status(), StatusCode::OK);
        let joined: JoinResponse = response.json().await.unwrap();
        (joined.token, joined.credential)
    }
}

async fn error_of(response: Response, status: StatusCode, code: &str) -> ErrorBody {
    assert_eq!(response.status(), status);
    let body: ErrorBody = response.json().await.unwrap();
    assert_eq!(body.error.code, code);
    body
}

fn kinds(data_dir: &Path) -> Vec<String> {
    operator::security_log(data_dir, None)
        .unwrap()
        .into_iter()
        .map(|entry| entry.kind)
        .collect()
}

fn open_db(dir: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join(DB_FILE_NAME)).unwrap()
}

fn handles(events: &[Event]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event.by.handle.as_str())
        .collect()
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_is_open_and_every_other_route_needs_a_valid_token() {
    let board = Net::start().await;
    let health: Health = board
        .http
        .get(board.at("/v1/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.mode, MODE_NETWORK);
    assert_eq!(health.version, openboard_api::build::VERSION);
    assert_eq!(health.api_level, Some(API_LEVEL));
    assert_eq!(health.commit.as_deref(), openboard_api::build::commit());

    let (revoked, credential) = board.enrol("gone").await;
    assert_eq!(board.status(&revoked).await, StatusCode::OK);
    operator::revoke_credential(&board.data_dir(), &credential.id).unwrap();
    let (logged_out, _) = board.enrol("left").await;
    let response = board
        .http
        .post(board.at("/v1/logout"))
        .bearer_auth(&logged_out)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let unknown = format!("{TOKEN_PREFIX}{}", "ab".repeat(32));
    let authorizations: Vec<Option<String>> = vec![
        None,
        Some("Bearer garbage".to_owned()),
        Some("Bearer ".to_owned()),
        Some(format!("Bearer {unknown}")),
        Some(format!("Basic {unknown}")),
        Some(unknown.clone()),
        Some(format!("Bearer {revoked}")),
        Some(format!("Bearer {logged_out}")),
    ];
    let json = "application/json";
    // Valid requests, invalid requests, a missing board and an unknown
    // route: without a token they must all look the same.
    type Probe = (
        &'static str,
        &'static str,
        Option<(&'static str, &'static str)>,
    );
    let requests: Vec<Probe> = vec![
        ("GET", "/v1/whoami", None),
        ("POST", "/v1/logout", None),
        (
            "POST",
            "/v1/boards/general/messages",
            Some((json, r#"{"channel":"a","text":"hello"}"#)),
        ),
        ("POST", "/v1/boards/general/messages", Some((json, "{nope"))),
        (
            "POST",
            "/v1/boards/general/messages",
            Some(("text/plain", "x")),
        ),
        ("GET", "/v1/boards/general/channels", None),
        ("GET", "/v1/boards/no-such-board/channels", None),
        ("GET", "/v1/boards/general/threads?channel=a", None),
        ("GET", "/v1/boards/general/threads", None),
        ("GET", "/v1/boards/general/threads/msg_nope", None),
        ("GET", "/v1/boards/general/events", None),
        ("GET", "/v1/boards/general/events?after=bogus", None),
        ("GET", "/v1/boards/general/events/stream", None),
        ("GET", "/v1/nope", None),
        ("DELETE", "/v1/boards/general/channels", None),
    ];
    let mut bodies = Vec::new();
    let mut header_names = Vec::new();
    for (method, path, body) in &requests {
        for authorization in &authorizations {
            let mut request = board.http.request(method.parse().unwrap(), board.at(path));
            if let Some(value) = authorization {
                request = request.header("authorization", value);
            }
            if let Some((content_type, body)) = body {
                request = request
                    .header("content-type", *content_type)
                    .body(body.to_string());
            }
            let response = request.send().await.unwrap();
            let context = format!("{method} {path} with {authorization:?}");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{context}");
            assert_eq!(
                response.headers().get("www-authenticate").unwrap(),
                "Bearer",
                "{context}"
            );
            // The version headers are on every refusal, with the same
            // values, and the refusals carry the same headers.
            assert_eq!(
                response.headers().get(VERSION_HEADER).unwrap(),
                openboard_api::build::VERSION,
                "{context}"
            );
            assert_eq!(
                response.headers().get(API_LEVEL_HEADER).unwrap(),
                API_LEVEL.to_string().as_str(),
                "{context}"
            );
            let mut names: Vec<String> = response
                .headers()
                .keys()
                .map(|name| name.as_str().to_owned())
                .collect();
            names.sort();
            header_names.push(names);
            bodies.push(response.bytes().await.unwrap());
        }
    }
    let first = &bodies[0];
    let parsed: ErrorBody = serde_json::from_slice(first).unwrap();
    assert_eq!(parsed.error.code, codes::UNAUTHENTICATED);
    assert!(bodies.iter().all(|body| body == first));
    for names in &header_names {
        assert_eq!(names, &header_names[0]);
    }

    // Nothing was created or posted by any of it.
    let (token, _) = board.enrol("reader").await;
    let events = board.events(&token, None, "after=start").await;
    assert!(events.events.is_empty());
    board.stop().await;
}

#[tokio::test]
async fn join_swaps_a_code_for_a_token_exactly_once() {
    let board = Net::start().await;
    let created =
        operator::create_invite(&board.data_dir(), "alice", &InviteOptions::default()).unwrap();
    assert!(created.new_principal);
    assert_eq!(created.principal, "alice");
    let invite = created.invite;
    assert_eq!(invite.url, board.url);
    assert_eq!(invite.fingerprint, board.fingerprint);
    assert_eq!(Invite::decode(&invite.encode()), Ok(invite.clone()));

    let response = board.join_raw(&invite.code).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let joined: JoinResponse = response.json().await.unwrap();
    assert!(joined.token.starts_with(TOKEN_PREFIX));
    assert!(joined.token.len() >= TOKEN_PREFIX.len() + 64);
    assert_eq!(joined.credential.principal, "alice");
    assert_eq!(joined.credential.kind, ActorKind::Agent);
    assert!(joined.credential.revoked_at.is_none());
    assert!(joined.credential.expires_at > joined.credential.created_at);
    assert_eq!(board.status(&joined.token).await, StatusCode::OK);

    // A used code, a wrong code and an expired code are indistinguishable.
    let expired = operator::create_invite(
        &board.data_dir(),
        "bob",
        &InviteOptions {
            ttl_secs: 0,
            ..InviteOptions::default()
        },
    )
    .unwrap();
    let withdrawn = board.invite("carol");
    operator::revoke_principal(&board.data_dir(), "carol").unwrap();
    let mut bodies = Vec::new();
    for code in [
        invite.code.as_str(),
        "not-a-code",
        "",
        &"0".repeat(64),
        &expired.invite.code,
        &withdrawn.code,
    ] {
        let response = board.join_raw(code).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{code}");
        bodies.push(response.bytes().await.unwrap());
    }
    let parsed: ErrorBody = serde_json::from_slice(&bodies[0]).unwrap();
    assert_eq!(parsed.error.code, codes::INVITE_INVALID);
    assert!(bodies.iter().all(|body| *body == bodies[0]));

    // A malformed request is a client error and reveals nothing either.
    let response = board
        .http
        .post(board.at("/v1/join"))
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    error_of(response, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;

    // Only alice holds a credential.
    let credentials = operator::list_credentials(&board.data_dir()).unwrap();
    assert_eq!(credentials.len(), 1);
    assert_eq!(credentials[0].id, joined.credential.id);
    board.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_joins_on_one_code_yield_exactly_one_token() {
    let board = Arc::new(Net::start().await);
    let invite = board.invite("alice");
    let attempts: Vec<_> = (0..12)
        .map(|_| {
            let (board, code) = (Arc::clone(&board), invite.code.clone());
            tokio::spawn(async move { board.join_raw(&code).await.status() })
        })
        .collect();
    let mut statuses = Vec::new();
    for attempt in attempts {
        statuses.push(attempt.await.unwrap());
    }
    let won = statuses.iter().filter(|s| **s == StatusCode::OK).count();
    let lost = statuses
        .iter()
        .filter(|s| **s == StatusCode::FORBIDDEN)
        .count();
    assert_eq!((won, lost), (1, 11), "{statuses:?}");
    assert_eq!(
        operator::list_credentials(&board.data_dir()).unwrap().len(),
        1
    );
    Arc::into_inner(board).unwrap().stop().await;
}

#[tokio::test]
async fn the_author_comes_from_the_token_and_cannot_be_spoofed() {
    let board = Net::start().await;
    let (alice, alice_credential) = board.enrol("alice").await;
    let (bob, bob_credential) = board.enrol("bob").await;

    // The body tries to name another author and credential.
    let posted = board
        .post(
            &alice,
            None,
            json!({
                "channel": "research",
                "text": "from alice",
                "by": {"id": "act_x", "kind": "person", "handle": "bob"},
                "credential_id": bob_credential.id,
                "handle": "bob",
            }),
        )
        .await;
    let message = &posted.message;
    assert_eq!(message.by.handle, "alice");
    assert_eq!(message.by.kind, ActorKind::Agent);
    assert_eq!(message.by.profile, None);
    assert_eq!(
        message.credential_id.as_deref(),
        Some(alice_credential.id.as_str())
    );

    let as_profile = board
        .post(
            &alice,
            Some("fx"),
            json!({"thread_id": message.id, "text": "alice with a profile"}),
        )
        .await;
    assert_eq!(as_profile.message.by.handle, "alice/fx");
    assert_eq!(as_profile.message.by.profile.as_deref(), Some("fx"));
    assert_eq!(as_profile.message.by.kind, ActorKind::Agent);
    assert_ne!(as_profile.message.by.id, message.by.id);
    assert_eq!(as_profile.message.credential_id, message.credential_id);

    let reply = board
        .post(
            &bob,
            None,
            json!({"thread_id": message.id, "text": "from bob"}),
        )
        .await;
    assert_eq!(reply.message.by.handle, "bob");
    assert_eq!(
        reply.message.credential_id.as_deref(),
        Some(bob_credential.id.as_str())
    );

    // Reads show the same provenance to everyone.
    let thread: Thread = board
        .get(&bob, &format!("/v1/boards/general/threads/{}", message.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(thread.root, *message);
    assert_eq!(
        thread.replies,
        vec![as_profile.message.clone(), reply.message.clone()]
    );

    let events = board.events(&bob, None, "after=start").await.events;
    assert_eq!(handles(&events), ["alice", "alice/fx", "bob"]);
    assert!(events.iter().all(|e| e.by.kind == ActorKind::Agent));
    assert_eq!(
        events
            .iter()
            .map(|e| e.credential_id.clone().unwrap())
            .collect::<Vec<_>>(),
        [
            alice_credential.id.clone(),
            alice_credential.id.clone(),
            bob_credential.id.clone()
        ]
    );
    assert_eq!(events[0].by, message.by);

    // not_from_self skips the caller's own principal and profile only.
    let seen = |events: EventsResponse| {
        events
            .events
            .iter()
            .map(|e| e.by.handle.clone())
            .collect::<Vec<_>>()
    };
    let query = "after=start&not_from_self=true";
    assert_eq!(
        seen(board.events(&alice, None, query).await),
        ["alice/fx", "bob"]
    );
    assert_eq!(
        seen(board.events(&alice, Some("fx"), query).await),
        ["alice", "bob"]
    );
    assert_eq!(
        seen(board.events(&bob, None, query).await),
        ["alice", "alice/fx"]
    );
    assert_eq!(
        seen(
            board
                .events(&bob, None, "after=start&not_from_self=false")
                .await
        )
        .len(),
        3
    );

    // whoami reports the same actor the board writes.
    let me: WhoAmI = board
        .get(&alice, "/v1/whoami")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.actor, message.by);
    assert_eq!(me.mode, MODE_NETWORK);
    assert_eq!(me.credential.unwrap().id, alice_credential.id);
    let me: WhoAmI = board
        .get(&alice, "/v1/whoami")
        .header(PROFILE_HEADER, "fx")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.actor, as_profile.message.by);

    let dir = board.stop().await;
    assert!(verify_log(&data_dir_of(&dir)).unwrap().is_ok());
}

#[tokio::test]
async fn a_local_board_has_whoami_and_not_from_self_but_no_sessions() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = TempDir::new().unwrap();
    let mut config = Config::new(data_dir_of(&dir));
    config.port = 0;
    let server = openboard_server::bind(config).await.unwrap();
    assert_eq!(server.mode(), Mode::Local);
    assert_eq!(server.fingerprint(), None);
    let base = server.url().to_owned();
    assert_eq!(base, format!("http://{}", server.local_addr()));
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    let http = Client::builder().no_proxy().build().unwrap();

    let me: WhoAmI = http
        .get(format!("{base}/v1/whoami"))
        .header(PROFILE_HEADER, "fx")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.mode, MODE_LOCAL);
    assert_eq!(me.actor.handle, "local/fx");
    assert_eq!(me.actor.kind, ActorKind::Local);
    assert!(me.credential.is_none());

    for (profile, text) in [(None, "plain"), (Some("fx"), "as fx")] {
        let mut request = http
            .post(format!("{base}/v1/boards/general/messages"))
            .json(&json!({"channel": "a", "text": text}));
        if let Some(profile) = profile {
            request = request.header(PROFILE_HEADER, profile);
        }
        let posted: PostMessageResponse = request.send().await.unwrap().json().await.unwrap();
        assert_eq!(posted.message.credential_id, None);
    }
    let events: EventsResponse = http
        .get(format!(
            "{base}/v1/boards/general/events?after=start&not_from_self=true"
        ))
        .header(PROFILE_HEADER, "fx")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(handles(&events.events), ["local"]);
    assert!(events.events.iter().all(|e| e.credential_id.is_none()));

    let response = http
        .post(format!("{base}/v1/join"))
        .json(&json!({"code": "x"}))
        .send()
        .await
        .unwrap();
    error_of(response, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;
    let response = http.post(format!("{base}/v1/logout")).send().await.unwrap();
    error_of(response, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;

    // The operator functions say plainly that there is nothing to enrol in.
    let data_dir = data_dir_of(&dir);
    assert!(matches!(
        operator::create_invite(&data_dir, "alice", &InviteOptions::default()),
        Err(OperatorError::NotNetwork)
    ));
    let info = operator::board_info(&data_dir).unwrap();
    assert_eq!(
        (info.mode, info.url, info.fingerprint),
        (Mode::Local, None, None)
    );
    assert!(operator::list_credentials(&data_dir).unwrap().is_empty());
    assert!(kinds(&data_dir).contains(&log_kinds::SERVER_STARTED.to_owned()));

    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn logout_revokes_the_calling_credential_only() {
    let board = Net::start().await;
    let (first, first_credential) = board.enrol("alice").await;
    // A second credential for the same principal.
    let again =
        operator::create_invite(&board.data_dir(), "Alice", &InviteOptions::default()).unwrap();
    assert!(!again.new_principal);
    assert_eq!(again.principal, "alice");
    let second: JoinResponse = board
        .join_raw(&again.invite.code)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(second.credential.principal, "alice");
    assert_ne!(second.credential.id, first_credential.id);

    let response = board
        .http
        .post(board.at("/v1/logout"))
        .bearer_auth(&first)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(board.status(&first).await, StatusCode::UNAUTHORIZED);
    assert_eq!(board.status(&second.token).await, StatusCode::OK);

    let credentials = operator::list_credentials(&board.data_dir()).unwrap();
    let by_id = |id: &str| credentials.iter().find(|c| c.id == id).unwrap();
    assert!(by_id(&first_credential.id).revoked_at.is_some());
    assert!(by_id(&second.credential.id).revoked_at.is_none());
    assert!(by_id(&second.credential.id).last_used_at.is_some());
    assert!(kinds(&board.data_dir()).contains(&log_kinds::LOGOUT.to_owned()));
    board.stop().await;
}

/// Reads a stream to its end and returns everything it sent.
async fn drain(mut response: Response) -> String {
    let mut text = String::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    text
}

/// Opens a long-poll and a stream as `token`, revokes with `revoke`, lets
/// another agent post, and checks that both ended without a single event.
async fn revocation_ends_open_waits(recheck: Duration, by_name: bool) {
    let board =
        Arc::new(Net::start_with(|config| config.limits.credential_recheck = recheck).await);
    let (alice, credential) = board.enrol("alice").await;
    let (bob, _) = board.enrol("bob").await;
    board.thread(&bob, "a", "before").await;

    let poll = {
        let (board, alice) = (Arc::clone(&board), alice.clone());
        tokio::spawn(async move {
            board
                .get(&alice, "/v1/boards/general/events?wait=30")
                .send()
                .await
                .unwrap()
        })
    };
    let stream = board
        .get(&alice, "/v1/boards/general/events/stream")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    let stream = tokio::spawn(drain(stream));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!poll.is_finished() && !stream.is_finished());

    // The operator acts on the data directory beside the running server.
    let data_dir = board.data_dir();
    let revoked = tokio::task::spawn_blocking(move || {
        if by_name {
            operator::revoke_principal(&data_dir, "ALICE")
        } else {
            operator::revoke_credential(&data_dir, &credential.id)
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(revoked.principal, "alice");
    assert_eq!(revoked.credentials.len(), 1);
    // An event the revoked credential must never see.
    board.thread(&bob, "a", "after the revocation").await;

    let within = Duration::from_secs(5);
    let response = tokio::time::timeout(within, poll)
        .await
        .expect("the long-poll ends promptly")
        .unwrap();
    let body = response.text().await.unwrap();
    assert!(!body.contains("message_id"), "{body}");
    let error: ErrorBody = serde_json::from_str(&body).unwrap();
    assert_eq!(error.error.code, codes::UNAUTHENTICATED);
    let sent = tokio::time::timeout(within, stream)
        .await
        .expect("the stream ends promptly")
        .unwrap();
    assert!(!sent.contains("data:"), "{sent}");

    // The next request fails, and bob is unaffected.
    assert_eq!(board.status(&alice).await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        board.events(&bob, None, "after=start").await.events.len(),
        2
    );
    Arc::into_inner(board).unwrap().stop().await;
}

#[tokio::test]
async fn revocation_is_checked_before_an_event_is_delivered() {
    // The periodic check is far away, so only the check made before each
    // delivery can be what stops the event.
    revocation_ends_open_waits(Duration::from_secs(600), false).await;
}

#[tokio::test]
async fn revocation_ends_idle_waits_on_the_periodic_check() {
    let board = Net::start().await;
    let (alice, _) = board.enrol("alice").await;
    let poll = board
        .get(&alice, "/v1/boards/general/events?wait=30")
        .send();
    let stream = board
        .get(&alice, "/v1/boards/general/events/stream")
        .send()
        .await
        .unwrap();
    let data_dir = board.data_dir();
    let revoke = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        tokio::task::spawn_blocking(move || operator::revoke_principal(&data_dir, "alice"))
            .await
            .unwrap()
            .unwrap();
    };
    // Nothing is posted: only the recheck can notice.
    let (response, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(poll, revoke) })
            .await
            .expect("the long-poll ends promptly");
    error_of(
        response.unwrap(),
        StatusCode::UNAUTHORIZED,
        codes::UNAUTHENTICATED,
    )
    .await;
    let sent = tokio::time::timeout(Duration::from_secs(5), drain(stream))
        .await
        .expect("the stream ends promptly");
    assert!(!sent.contains("data:"), "{sent}");
    board.stop().await;
}

#[tokio::test]
async fn revoking_a_principal_ends_its_open_waits_too() {
    revocation_ends_open_waits(Duration::from_millis(100), true).await;
}

#[tokio::test]
async fn tokens_expire() {
    let board = Net::start().await;
    let invite = operator::create_invite(
        &board.data_dir(),
        "brief",
        &InviteOptions {
            token_ttl_secs: Some(1),
            ..InviteOptions::default()
        },
    )
    .unwrap()
    .invite;
    let joined: JoinResponse = board.join_raw(&invite.code).await.json().await.unwrap();
    assert_eq!(board.status(&joined.token).await, StatusCode::OK);

    // A long-poll that is open when the token expires ends with it.
    let started = std::time::Instant::now();
    let response = board
        .get(&joined.token, "/v1/boards/general/events?wait=30")
        .send()
        .await
        .unwrap();
    error_of(response, StatusCode::UNAUTHORIZED, codes::UNAUTHENTICATED).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(board.status(&joined.token).await, StatusCode::UNAUTHORIZED);

    let log = operator::security_log(&board.data_dir(), None).unwrap();
    let failure = log
        .iter()
        .rfind(|entry| entry.kind == log_kinds::AUTH_FAILED)
        .unwrap();
    assert_eq!(failure.detail.as_deref(), Some("expired token"));
    assert_eq!(failure.principal.as_deref(), Some("brief"));
    assert_eq!(
        failure.credential_id.as_deref(),
        Some(joined.credential.id.as_str())
    );
    board.stop().await;
}

// ---------------------------------------------------------------------------
// Modes, binds and TLS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_non_loopback_bind_needs_network_mode_and_a_public_url() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = TempDir::new().unwrap();
    let mut config = Config::new(data_dir_of(&dir));
    config.bind = "0.0.0.0".parse().unwrap();
    config.port = 0;
    let error = openboard_server::bind(config.clone()).await.unwrap_err();
    assert!(matches!(error, ServeError::NonLoopbackBind(_)), "{error}");

    config.network = true;
    let error = openboard_server::bind(config.clone()).await.unwrap_err();
    assert!(matches!(error, ServeError::PublicUrlRequired(_)), "{error}");
    for bad in [
        "http://192.0.2.7:7878",
        "https://192.0.2.7:7878/path",
        "nope",
    ] {
        config.public_url = Some(bad.to_owned());
        let error = openboard_server::bind(config.clone()).await.unwrap_err();
        assert!(
            matches!(error, ServeError::InvalidPublicUrl(_)),
            "{bad}: {error}"
        );
    }
    assert!(
        !data_dir_of(&dir).exists(),
        "a refused start must not touch the disk"
    );

    let board = Net::start_in(dir, |config| {
        config.bind = "0.0.0.0".parse().unwrap();
        config.public_url = Some("https://192.0.2.7:7878/".to_owned());
    })
    .await;
    assert!(board.addr.ip().is_unspecified());
    assert_eq!(board.url, "https://192.0.2.7:7878");
    let info = operator::board_info(&board.data_dir()).unwrap();
    assert_eq!(info.mode, Mode::Network);
    assert_eq!(info.url.as_deref(), Some("https://192.0.2.7:7878"));
    assert_eq!(
        info.fingerprint.as_deref(),
        Some(board.fingerprint.as_str())
    );
    // Invites carry the public URL, or the one the operator gives.
    assert_eq!(board.invite("alice").url, "https://192.0.2.7:7878");
    let options = |url: &str| InviteOptions {
        url: Some(url.to_owned()),
        ..InviteOptions::default()
    };
    let moved = operator::create_invite(
        &board.data_dir(),
        "alice",
        &options("https://10.0.0.5:9000"),
    )
    .unwrap();
    assert_eq!(moved.invite.url, "https://10.0.0.5:9000");
    assert!(matches!(
        operator::create_invite(&board.data_dir(), "alice", &options("http://10.0.0.5:9000")),
        Err(OperatorError::InvalidUrl(_))
    ));

    // Reachable, with authentication, and no Host guard in network mode.
    let (token, _) = board.enrol("alice").await;
    let response = board
        .get(&token, "/v1/whoami")
        .header("host", "board.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    board.stop().await;
}

#[tokio::test]
async fn only_tls_1_3_with_the_pinned_certificate_gets_through() {
    let board = Net::start().await;
    let digest = parse_fingerprint(&board.fingerprint).unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let handshake = |versions: &[&'static SupportedProtocolVersion], digest: [u8; 32]| {
        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config(digest, versions)));
        let (addr, name) = (board.addr, name.clone());
        async move {
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            connector.connect(name, tcp).await
        }
    };
    let negotiated = handshake(&[&rustls::version::TLS13], digest).await.unwrap();
    assert_eq!(
        negotiated.get_ref().1.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_3)
    );
    assert!(handshake(&[&rustls::version::TLS12], digest).await.is_err());
    // A client that offers both lands on 1.3.
    let both = handshake(rustls::ALL_VERSIONS, digest).await.unwrap();
    assert_eq!(
        both.get_ref().1.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_3)
    );
    // A client pinned to another certificate refuses the board.
    assert!(handshake(&[&rustls::version::TLS13], [7u8; 32])
        .await
        .is_err());
    let stranger = pinned_client(&openboard_api::auth::fingerprint(&[7u8; 32]));
    assert!(stranger.get(board.at("/v1/health")).send().await.is_err());

    // Plain HTTP gets no HTTP answer on the TLS port.
    let plain = Client::builder().no_proxy().build().unwrap();
    let result = plain
        .get(format!("http://127.0.0.1:{}/v1/health", board.addr.port()))
        .send()
        .await;
    assert!(result.is_err(), "{result:?}");
    let mut tcp = tokio::net::TcpStream::connect(board.addr).await.unwrap();
    tcp.write_all(b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), tcp.read_to_end(&mut answer))
        .await
        .expect("the server hangs up");
    assert!(
        !String::from_utf8_lossy(&answer).contains("HTTP/"),
        "{answer:?}"
    );

    // The board still serves proper clients afterwards.
    let response = board.http.get(board.at("/v1/health")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    board.stop().await;
}

#[tokio::test]
async fn the_certificate_persists_and_a_network_directory_stays_one() {
    let board = Net::start().await;
    let fingerprint = board.fingerprint.clone();
    let (token, _) = board.enrol("alice").await;
    board.thread(&token, "a", "kept").await;
    let dir = board.stop().await;
    let data_dir = data_dir_of(&dir);

    let key = data_dir.join(KEY_FILE_NAME);
    let cert = data_dir.join(CERT_FILE_NAME);
    assert!(key.is_file() && cert.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&key), 0o600);
        assert_eq!(mode(&data_dir), 0o700);
    }
    let cert_before = std::fs::read(&cert).unwrap();
    let key_before = std::fs::read(&key).unwrap();
    // The fingerprint is the SHA-256 of the certificate on disk.
    let pem = String::from_utf8(cert_before.clone()).unwrap();
    assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));

    // Asked for local mode, the directory is still served as a network one.
    let board = Net::start_in(dir, |config| config.network = false).await;
    assert_eq!(board.fingerprint, fingerprint);
    assert_eq!(std::fs::read(&cert).unwrap(), cert_before);
    assert_eq!(std::fs::read(&key).unwrap(), key_before);
    let health: Health = board
        .http
        .get(board.at("/v1/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health.mode, MODE_NETWORK);
    let plain = Client::builder().no_proxy().build().unwrap();
    assert!(plain
        .get(format!("http://127.0.0.1:{}/v1/health", board.addr.port()))
        .send()
        .await
        .is_err());
    let response = board.http.get(board.at("/v1/whoami")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // The token from before the restart still works.
    assert_eq!(
        board.events(&token, None, "after=start").await.events.len(),
        1
    );
    let dir = board.stop().await;

    // Without its key the board refuses to start rather than change its pin.
    std::fs::rename(&key, data_dir.join("key.bak")).unwrap();
    let mut config = Config::new(&data_dir);
    config.port = 0;
    let error = openboard_server::bind(config).await.unwrap_err();
    assert!(matches!(error, ServeError::Tls(_)), "{error}");
    drop(dir);
}

#[tokio::test]
async fn a_local_directory_upgrades_one_way() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = TempDir::new().unwrap();
    let data_dir = data_dir_of(&dir);
    let mut config = Config::new(&data_dir);
    config.port = 0;
    let server = openboard_server::bind(config.clone()).await.unwrap();
    assert_eq!(server.mode(), Mode::Local);
    let base = server.url().to_owned();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    let http = Client::builder().no_proxy().build().unwrap();
    let response = http
        .post(format!("{base}/v1/boards/general/messages"))
        .json(&json!({"channel": "a", "text": "written locally"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(!data_dir.join(KEY_FILE_NAME).exists());

    let board = Net::start_in(dir, |_| {}).await;
    let (token, _) = board.enrol("alice").await;
    board.thread(&token, "a", "written over the network").await;
    let events = board.events(&token, None, "after=start").await.events;
    assert_eq!(handles(&events), ["local", "alice"]);
    assert_eq!(events[0].credential_id, None);
    assert!(events[1].credential_id.is_some());
    assert_eq!(events[1].prev_hash, events[0].hash);
    let log = kinds(&board.data_dir());
    assert!(
        log.contains(&log_kinds::MODE_UPGRADED.to_owned()),
        "{log:?}"
    );
    let dir = board.stop().await;
    assert!(verify_log(&data_dir).unwrap().is_ok());

    // There is no way back.
    let server = openboard_server::bind(config).await.unwrap();
    assert_eq!(server.mode(), Mode::Network);
    assert!(server.url().starts_with("https://"));
    drop(server);
    drop(dir);
}

// ---------------------------------------------------------------------------
// Rate limits
// ---------------------------------------------------------------------------

async fn expect_rate_limited(response: Response) {
    let retry_after: u64 = response
        .headers()
        .get("retry-after")
        .expect("a Retry-After header")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry_after), "{retry_after}");
    error_of(response, StatusCode::TOO_MANY_REQUESTS, codes::RATE_LIMITED).await;
}

#[tokio::test]
async fn requests_are_limited_per_credential_across_profiles() {
    let board = Net::start_with(|config| config.limits.requests_per_minute = 6).await;
    let (alice, _) = board.enrol("alice").await;
    let (bob, _) = board.enrol("bob").await;
    for round in 0..6 {
        // Profiles share their credential's budget.
        let profile = ["fx", "fy", "fz"][round % 3];
        let response = board
            .get(&alice, "/v1/whoami")
            .header(PROFILE_HEADER, profile)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "round {round}");
    }
    for profile in ["fx", "new"] {
        let response = board
            .get(&alice, "/v1/whoami")
            .header(PROFILE_HEADER, profile)
            .send()
            .await
            .unwrap();
        expect_rate_limited(response).await;
    }
    // Another credential has its own budget, and health is never limited.
    assert_eq!(board.status(&bob).await, StatusCode::OK);
    let response = board.http.get(board.at("/v1/health")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let log = operator::security_log(&board.data_dir(), None).unwrap();
    let limited: Vec<_> = log
        .iter()
        .filter(|entry| entry.kind == log_kinds::RATE_LIMITED)
        .collect();
    assert_eq!(limited.len(), 1, "logged once per window");
    assert_eq!(limited[0].principal.as_deref(), Some("alice"));
    board.stop().await;
}

#[tokio::test]
async fn joins_and_failed_authentication_are_limited_per_address() {
    let board = Net::start_with(|config| {
        config.limits.joins_per_minute = 3;
        config.limits.auth_failures_per_minute = 4;
    })
    .await;
    let invite = board.invite("alice");
    let wrong = "0".repeat(64);
    for _ in 0..2 {
        let response = board.join_raw(&wrong).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    // A good code does not use up the budget, which counts failures only.
    let joined: JoinResponse = board.join_raw(&invite.code).await.json().await.unwrap();
    let response = board.join_raw("not even shaped like a code").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // Three failures: this address is over its budget. Junk is turned away,
    // in whatever shape it comes.
    expect_rate_limited(board.join_raw(&wrong).await).await;
    expect_rate_limited(board.join_raw("wrong").await).await;
    let response = board
        .http
        .post(board.at("/v1/join"))
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    expect_rate_limited(response).await;
    // An agent behind the same address with a real invite still gets in,
    // once: a used code is junk like any other.
    let spare = board.invite("bob");
    let response = board.join_raw(&spare.code).await;
    assert_eq!(response.status(), StatusCode::OK);
    let second: JoinResponse = response.json().await.unwrap();
    assert_eq!(second.credential.principal, "bob");
    expect_rate_limited(board.join_raw(&spare.code).await).await;
    assert!(operator::list_invites(&board.data_dir())
        .unwrap()
        .is_empty());

    for _ in 0..4 {
        assert_eq!(board.status("garbage").await, StatusCode::UNAUTHORIZED);
    }
    let response = board.get("garbage", "/v1/whoami").send().await.unwrap();
    expect_rate_limited(response).await;
    let response = board.http.get(board.at("/v1/whoami")).send().await.unwrap();
    expect_rate_limited(response).await;
    // A valid token from the same address is still served.
    assert_eq!(board.status(&joined.token).await, StatusCode::OK);

    let log = operator::security_log(&board.data_dir(), None).unwrap();
    let count = |kind: &str| log.iter().filter(|entry| entry.kind == kind).count();
    assert_eq!(count(log_kinds::AUTH_FAILED), 4);
    assert_eq!(count(log_kinds::ENROL_FAILED), 3);
    assert_eq!(count(log_kinds::ENROL_SUCCEEDED), 2);
    assert_eq!(count(log_kinds::RATE_LIMITED), 2);
    board.stop().await;
}

#[tokio::test]
async fn open_waits_are_capped_per_credential() {
    let board = Arc::new(Net::start_with(|config| config.limits.max_waiters = 2).await);
    let (alice, _) = board.enrol("alice").await;
    let (bob, _) = board.enrol("bob").await;
    let poll = {
        let (board, alice) = (Arc::clone(&board), alice.clone());
        tokio::spawn(async move {
            board
                .get(&alice, "/v1/boards/general/events?wait=30")
                .send()
                .await
                .unwrap()
        })
    };
    let stream = board
        .get(&alice, "/v1/boards/general/events/stream")
        .header(PROFILE_HEADER, "fx")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(300)).await;

    for path in [
        "/v1/boards/general/events?wait=5",
        "/v1/boards/general/events/stream",
    ] {
        expect_rate_limited(board.get(&alice, path).send().await.unwrap()).await;
    }
    // A plain read is not a wait, and another credential has its own places.
    let response = board
        .get(&alice, "/v1/boards/general/events")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = board
        .get(&bob, "/v1/boards/general/events?wait=1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Ending the waits frees their places.
    board.thread(&bob, "a", "wake").await;
    let response = tokio::time::timeout(Duration::from_secs(5), poll)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(stream);
    // Both places come back: two waits can be open together again.
    let mut freed = false;
    for _ in 0..50 {
        let wait = || board.get(&alice, "/v1/boards/general/events?wait=1").send();
        let (one, two) = tokio::join!(wait(), wait());
        if one.unwrap().status() == StatusCode::OK && two.unwrap().status() == StatusCode::OK {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(freed);
    Arc::into_inner(board).unwrap().stop().await;
}

// ---------------------------------------------------------------------------
// Security log and operator functions
// ---------------------------------------------------------------------------

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[tokio::test]
async fn the_security_log_records_what_happened_without_secrets() {
    let board = Net::start().await;
    let invite = board.invite("alice");
    let joined: JoinResponse = board.join_raw(&invite.code).await.json().await.unwrap();
    let token = joined.token.clone();
    let secret_text = "the launch code is swordfish";
    board.thread(&token, "a", secret_text).await;
    board.join_raw("wrong-code-attempt").await;
    board.join_raw(&invite.code).await;
    assert_eq!(board.status("obt_garbage").await, StatusCode::UNAUTHORIZED);
    let (other, other_credential) = board.enrol("bob").await;
    operator::revoke_credential(&board.data_dir(), &other_credential.id).unwrap();
    assert_eq!(board.status(&other).await, StatusCode::UNAUTHORIZED);
    let response = board
        .http
        .post(board.at("/v1/logout"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let dir = board.stop().await;
    let data_dir = data_dir_of(&dir);

    let log = operator::security_log(&data_dir, None).unwrap();
    let summary: Vec<(&str, Option<&str>, Option<&str>)> = log
        .iter()
        .map(|e| (e.kind.as_str(), e.principal.as_deref(), e.detail.as_deref()))
        .collect();
    let alice = Some("alice");
    let bob = Some("bob");
    assert_eq!(
        summary,
        [
            (log_kinds::SERVER_STARTED, None, Some("mode=network")),
            (log_kinds::INVITE_CREATED, alice, summary[1].2),
            (log_kinds::ENROL_SUCCEEDED, alice, None),
            (log_kinds::ENROL_FAILED, None, Some("unusable code")),
            (log_kinds::ENROL_FAILED, None, Some("unusable code")),
            (log_kinds::AUTH_FAILED, None, Some("malformed token")),
            (log_kinds::INVITE_CREATED, bob, summary[6].2),
            (log_kinds::ENROL_SUCCEEDED, bob, None),
            (log_kinds::CREDENTIAL_REVOKED, bob, Some("by operator")),
            (log_kinds::AUTH_FAILED, bob, Some("revoked token")),
            (log_kinds::LOGOUT, alice, None),
        ]
    );
    assert!(summary[1].2.unwrap().starts_with("inv_"));
    assert!(log.windows(2).all(|pair| pair[0].id < pair[1].id));
    assert_eq!(
        log[2].credential_id.as_deref(),
        Some(joined.credential.id.as_str())
    );
    assert_eq!(
        log[9].credential_id.as_deref(),
        Some(other_credential.id.as_str())
    );
    // Requests carry a truncated address; operator actions carry none.
    for entry in &log {
        let from_network = matches!(
            entry.kind.as_str(),
            log_kinds::ENROL_SUCCEEDED
                | log_kinds::ENROL_FAILED
                | log_kinds::AUTH_FAILED
                | log_kinds::LOGOUT
        );
        let expected = from_network.then_some("127.0.0.0");
        assert_eq!(entry.client.as_deref(), expected, "{entry:?}");
    }
    let newest = operator::security_log(&data_dir, Some(2)).unwrap();
    assert_eq!(newest, log[log.len() - 2..]);

    // The log cannot be rewritten.
    let conn = open_db(&data_dir);
    assert!(conn
        .execute("UPDATE security_log SET kind = 'x'", [])
        .is_err());
    assert!(conn.execute("DELETE FROM security_log", []).is_err());
    let rows = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
    assert_eq!(rows("SELECT count(*) FROM security_log"), log.len() as i64);
    // Neither secret is anywhere in the database, in any table, and the log
    // holds no message text.
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    drop(conn);
    for entry in std::fs::read_dir(&data_dir).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        for secret in [&token, &other, &invite.code] {
            assert!(
                !contains(&bytes, secret.as_bytes()),
                "a secret is stored in {}",
                path.display()
            );
        }
    }
    let database = std::fs::read(data_dir.join(DB_FILE_NAME)).unwrap();
    assert!(
        contains(&database, secret_text.as_bytes()),
        "the scan sees stored text"
    );
    let conn = open_db(&data_dir);
    let leaked: i64 = conn
        .query_row(
            "SELECT count(*) FROM security_log
             WHERE coalesce(detail, '') || coalesce(principal, '') LIKE '%swordfish%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0);
}

#[tokio::test]
async fn failed_authentication_cannot_flood_the_security_log() {
    // One address alone stops at its share, far below the budget of all.
    let board = Net::start().await;
    let limits = Limits::default();
    let share = limits.failure_log_per_address_per_minute as usize;
    assert!(share * 4 < limits.failure_log_per_minute as usize);
    for _ in 0..share + 30 {
        assert_eq!(board.status("garbage").await, StatusCode::UNAUTHORIZED);
    }
    let log = operator::security_log(&board.data_dir(), None).unwrap();
    let count = |kind: &str| log.iter().filter(|entry| entry.kind == kind).count();
    assert_eq!(count(log_kinds::AUTH_FAILED), share);
    assert_eq!(count(log_kinds::LOG_SUPPRESSED), 1);
    assert_eq!(log.len(), share + 2, "{log:?}");
    let marker = log.last().unwrap();
    assert_eq!(marker.kind, log_kinds::LOG_SUPPRESSED);
    assert_eq!(marker.client.as_deref(), Some("127.0.0.0"));
    board.stop().await;
}

#[tokio::test]
async fn one_address_cannot_use_up_the_failure_log_for_another() {
    let Some(board) = start_dual(|config| {
        config.limits.failure_log_per_address_per_minute = 5;
        config.limits.failure_log_per_minute = 9;
    })
    .await
    else {
        return;
    };
    let fail = |host: &'static str| {
        let url = format!("https://{host}:{}/v1/whoami", board.addr.port());
        let request = board.http.get(url).bearer_auth("garbage");
        async move {
            assert_eq!(
                request.send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
    };
    // A noisy address: its share of entries, one marker, then nothing.
    for _ in 0..40 {
        fail("127.0.0.1").await;
    }
    // Another address is still logged, until the budget of all is spent.
    for _ in 0..10 {
        fail("[::1]").await;
    }
    let log = operator::security_log(&board.data_dir(), None).unwrap();
    let entries: Vec<(&str, Option<&str>)> = log
        .iter()
        .skip_while(|entry| entry.kind != log_kinds::AUTH_FAILED)
        .map(|entry| (entry.kind.as_str(), entry.client.as_deref()))
        .collect();
    let (noisy, other) = (Some("127.0.0.0"), Some("::"));
    let mut expected = vec![(log_kinds::AUTH_FAILED, noisy); 5];
    expected.push((log_kinds::LOG_SUPPRESSED, noisy));
    expected.extend([(log_kinds::AUTH_FAILED, other); 3]);
    // Nine entries so far: the budget of all clients. One marker, and the
    // rest of the minute is dropped.
    expected.push((log_kinds::LOG_SUPPRESSED, None));
    assert_eq!(entries, expected);
    board.stop().await;
}

/// Makes the security log entries of the given types look years old, the
/// way only someone with the database file could.
fn backdate_log(data_dir: &Path, kinds: &[&str]) {
    let kinds = kinds
        .iter()
        .map(|kind| format!("'{kind}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let conn = open_db(data_dir);
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.execute_batch(&format!(
        "BEGIN IMMEDIATE;
         DROP TRIGGER security_log_is_append_only_update;
         UPDATE security_log SET at = '2020-01-01T00:00:00.000Z' WHERE kind IN ({kinds});
         CREATE TRIGGER security_log_is_append_only_update BEFORE UPDATE ON security_log
         BEGIN SELECT RAISE(ABORT, 'the security log is append-only'); END;
         COMMIT;"
    ))
    .unwrap();
}

#[tokio::test]
async fn the_security_log_is_trimmed_by_age_and_by_count() {
    let board = Net::start().await;
    for name in ["alice", "bob", "carol"] {
        board.enrol(name).await;
    }
    let dir = board.stop().await;
    let data_dir = data_dir_of(&dir);
    let before = operator::security_log(&data_dir, None).unwrap();
    // server.started, then invite.created and enrol.succeeded for each.
    assert_eq!(before.len(), 7);

    // By count, at start-up: the newest three are kept, and the removal is
    // itself on record.
    let board = Net::start_in(dir, |config| config.security_log.max_entries = 3).await;
    let log = operator::security_log(&board.data_dir(), None).unwrap();
    let summary: Vec<(i64, &str, Option<&str>)> = log
        .iter()
        .map(|entry| (entry.id, entry.kind.as_str(), entry.detail.as_deref()))
        .collect();
    assert_eq!(
        summary,
        [
            (6, log_kinds::INVITE_CREATED, before[5].detail.as_deref()),
            (7, log_kinds::ENROL_SUCCEEDED, None),
            (8, log_kinds::SERVER_STARTED, Some("mode=network")),
            (
                9,
                log_kinds::LOG_PRUNED,
                Some("5 entries removed by retention")
            ),
        ]
    );
    let dir = board.stop().await;

    // By age, while running: entries from long ago go at the next check.
    let old = [
        log_kinds::INVITE_CREATED,
        log_kinds::ENROL_SUCCEEDED,
        log_kinds::SERVER_STARTED,
        log_kinds::LOG_PRUNED,
    ];
    backdate_log(&data_dir, &old);
    let board = Net::start_in(dir, |config| {
        config.security_log = LogRetention {
            max_age: Duration::from_secs(3600),
            max_entries: 1000,
            check_every: Duration::from_millis(200),
        };
    })
    .await;
    // The start trimmed the four old entries and logged itself.
    let kinds_now = kinds(&board.data_dir());
    assert_eq!(
        kinds_now,
        [log_kinds::SERVER_STARTED, log_kinds::LOG_PRUNED]
    );
    board.enrol("dave").await;
    backdate_log(&board.data_dir(), &old[2..]);
    let mut trimmed = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trimmed = kinds(&board.data_dir());
        if trimmed.last().map(String::as_str) == Some(log_kinds::LOG_PRUNED) {
            break;
        }
    }
    assert_eq!(
        trimmed,
        [
            log_kinds::INVITE_CREATED,
            log_kinds::ENROL_SUCCEEDED,
            log_kinds::LOG_PRUNED
        ]
    );
    let dir = board.stop().await;

    // Trimming is the only way an entry goes: the log still refuses
    // changes, and nothing is trimmed when nothing is beyond the bounds.
    let conn = open_db(&data_dir);
    assert!(conn.execute("DELETE FROM security_log", []).is_err());
    assert!(conn
        .execute("UPDATE security_log SET kind = 'x'", [])
        .is_err());
    drop(conn);
    let board = Net::start_in(dir, |_| {}).await;
    let log = kinds(&board.data_dir());
    assert_eq!(log.len(), 4, "{log:?}");
    assert_eq!(log[3], log_kinds::SERVER_STARTED);
    board.stop().await;
}

#[tokio::test]
async fn last_used_follows_use_at_the_configured_interval() {
    let last_used = |board: &Net, id: &str| {
        operator::list_credentials(&board.data_dir())
            .unwrap()
            .into_iter()
            .find(|credential| credential.id == id)
            .unwrap()
            .last_used_at
    };
    // With a short interval, a later request moves the time on.
    let board = Net::start_with(|config| {
        config.limits.last_used_interval = Duration::from_millis(200);
    })
    .await;
    let (token, credential) = board.enrol("alice").await;
    assert_eq!(last_used(&board, &credential.id), None);
    assert_eq!(board.status(&token).await, StatusCode::OK);
    let first = last_used(&board, &credential.id).expect("set by the first use");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(last_used(&board, &credential.id).as_ref(), Some(&first));
    assert_eq!(board.status(&token).await, StatusCode::OK);
    let second = last_used(&board, &credential.id).unwrap();
    assert!(second > first, "{second} is later than {first}");
    board.stop().await;

    // With the default of a minute it is deliberately coarse: a request
    // soon after the first one does not write.
    let board = Net::start().await;
    let (token, credential) = board.enrol("alice").await;
    assert_eq!(board.status(&token).await, StatusCode::OK);
    let first = last_used(&board, &credential.id).expect("set by the first use");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(board.status(&token).await, StatusCode::OK);
    assert_eq!(last_used(&board, &credential.id), Some(first));
    board.stop().await;
}

#[tokio::test]
async fn operator_functions_work_with_the_server_running_and_stopped() {
    let dir = TempDir::new().unwrap();
    let data_dir = data_dir_of(&dir);
    assert!(matches!(
        operator::board_info(&data_dir),
        Err(OperatorError::NoDatabase(_))
    ));
    assert!(matches!(
        operator::create_invite(&data_dir, "alice", &InviteOptions::default()),
        Err(OperatorError::NoDatabase(_))
    ));

    // Running.
    let board = Net::start_in(dir, |_| {}).await;
    for bad in [
        "",
        "9lives",
        "has space",
        "local",
        "Anonymous",
        &"a".repeat(33),
    ] {
        assert!(
            matches!(
                operator::create_invite(&data_dir, bad, &InviteOptions::default()),
                Err(OperatorError::InvalidName(_))
            ),
            "{bad}"
        );
    }
    let (alice, alice_credential) = board.enrol("Alice").await;
    let pending = operator::create_invite(&data_dir, "bob", &InviteOptions::default()).unwrap();
    let principals = operator::list_principals(&data_dir).unwrap();
    let rows: Vec<_> = principals
        .iter()
        .map(|p| {
            (
                p.handle.as_str(),
                p.kind,
                p.active_credentials,
                p.pending_invites,
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("Alice", ActorKind::Agent, 1, 0),
            ("bob", ActorKind::Agent, 0, 1)
        ]
    );
    let invites = operator::list_invites(&data_dir).unwrap();
    assert_eq!(invites.len(), 1);
    assert_eq!(
        (invites[0].id.as_str(), invites[0].principal.as_str()),
        (pending.id.as_str(), "bob")
    );
    assert_eq!(invites[0].expires_at, pending.expires_at);
    let credentials = operator::list_credentials(&data_dir).unwrap();
    assert_eq!(credentials.len(), 1);
    assert_eq!(credentials[0].id, alice_credential.id);
    assert_eq!(credentials[0].principal, "Alice");
    assert_eq!(credentials[0].expires_at, alice_credential.expires_at);
    assert_eq!(board.status(&alice).await, StatusCode::OK);
    assert!(matches!(
        operator::revoke_credential(&data_dir, "crd_nope"),
        Err(OperatorError::NoSuchCredential(_))
    ));
    assert!(matches!(
        operator::revoke_principal(&data_dir, "nobody"),
        Err(OperatorError::NoSuchPrincipal(_))
    ));
    let info = operator::board_info(&data_dir).unwrap();
    assert_eq!(info.url.as_deref(), Some(board.url.as_str()));
    assert_eq!(
        info.fingerprint.as_deref(),
        Some(board.fingerprint.as_str())
    );
    let dir = board.stop().await;

    // Stopped: everything still works on the directory alone.
    let offline = operator::create_invite(&data_dir, "carol", &InviteOptions::default()).unwrap();
    assert_eq!(
        offline.invite.fingerprint,
        info.fingerprint.clone().unwrap()
    );
    let revoked = operator::revoke_principal(&data_dir, "BOB").unwrap();
    assert_eq!(revoked.principal, "bob");
    assert_eq!(
        (revoked.credentials.len(), revoked.invites),
        (0, vec![pending.id.clone()])
    );
    let revoked = operator::revoke_credential(&data_dir, &alice_credential.id).unwrap();
    assert_eq!(
        revoked.credentials,
        std::slice::from_ref(&alice_credential.id)
    );
    // Revoking again changes nothing and is not an error.
    let again = operator::revoke_credential(&data_dir, &alice_credential.id).unwrap();
    assert!(again.credentials.is_empty());
    assert_eq!(operator::list_invites(&data_dir).unwrap().len(), 1);
    assert!(operator::list_credentials(&data_dir).unwrap()[0]
        .revoked_at
        .is_some());
    assert_eq!(operator::board_info(&data_dir).unwrap(), info);
    let log = kinds(&data_dir);
    assert!(log.contains(&log_kinds::INVITE_REVOKED.to_owned()));
    assert_eq!(
        log.iter()
            .filter(|k| *k == log_kinds::CREDENTIAL_REVOKED)
            .count(),
        1
    );

    // Running again: what was done offline holds.
    let board = Net::start_in(dir, |_| {}).await;
    assert_eq!(board.status(&alice).await, StatusCode::UNAUTHORIZED);
    let response = board.join_raw(&pending.invite.code).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let joined: JoinResponse = board
        .join_raw(&offline.invite.code)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(joined.credential.principal, "carol");
    assert_eq!(board.status(&joined.token).await, StatusCode::OK);
    // A revoked principal keeps its name and can be invited back.
    let back = operator::create_invite(&data_dir, "bob", &InviteOptions::default()).unwrap();
    assert!(!back.new_principal);
    board.stop().await;
}

// ---------------------------------------------------------------------------
// Migration and the hash chain
// ---------------------------------------------------------------------------

/// The schema milestone M0 shipped, frozen here as a fixture.
const M0_SCHEMA: &str = "
CREATE TABLE boards (
    id INTEGER PRIMARY KEY, name TEXT NOT NULL, name_key TEXT NOT NULL UNIQUE,
    tag TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL,
    head_seq INTEGER NOT NULL DEFAULT 0, head_hash TEXT NOT NULL
);
CREATE TABLE actors (
    id TEXT PRIMARY KEY, kind TEXT NOT NULL, principal TEXT NOT NULL, profile TEXT,
    handle TEXT NOT NULL
);
CREATE TABLE channels (
    board_id INTEGER NOT NULL REFERENCES boards(id), path TEXT NOT NULL,
    created_at TEXT NOT NULL, thread_count INTEGER NOT NULL, last_activity_at TEXT NOT NULL,
    PRIMARY KEY (board_id, path)
) WITHOUT ROWID;
CREATE TABLE messages (
    id TEXT PRIMARY KEY, board_id INTEGER NOT NULL REFERENCES boards(id),
    channel TEXT NOT NULL, thread_id TEXT NOT NULL,
    actor_id TEXT NOT NULL REFERENCES actors(id), text TEXT NOT NULL,
    created_at TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 1, seq INTEGER NOT NULL
);
CREATE INDEX messages_by_thread ON messages(thread_id, seq);
CREATE TABLE threads (
    root_id TEXT PRIMARY KEY REFERENCES messages(id),
    board_id INTEGER NOT NULL REFERENCES boards(id), channel TEXT NOT NULL,
    reply_count INTEGER NOT NULL DEFAULT 0, last_seq INTEGER NOT NULL,
    last_activity_at TEXT NOT NULL
);
CREATE UNIQUE INDEX threads_by_activity ON threads(board_id, last_seq);
CREATE TABLE events (
    board_id INTEGER NOT NULL REFERENCES boards(id), seq INTEGER NOT NULL,
    kind TEXT NOT NULL, channel TEXT NOT NULL, thread_id TEXT NOT NULL,
    message_id TEXT NOT NULL, actor_id TEXT NOT NULL REFERENCES actors(id),
    at TEXT NOT NULL, prev_hash TEXT NOT NULL, hash TEXT NOT NULL,
    PRIMARY KEY (board_id, seq)
) WITHOUT ROWID;
CREATE TRIGGER events_are_append_only_update BEFORE UPDATE ON events
BEGIN SELECT RAISE(ABORT, 'the event log is append-only'); END;
CREATE TRIGGER events_are_append_only_delete BEFORE DELETE ON events
BEGIN SELECT RAISE(ABORT, 'the event log is append-only'); END;
CREATE TABLE idempotency (
    board_id INTEGER NOT NULL REFERENCES boards(id), actor_id TEXT NOT NULL,
    key TEXT NOT NULL, fingerprint TEXT NOT NULL,
    message_id TEXT NOT NULL REFERENCES messages(id),
    PRIMARY KEY (board_id, actor_id, key)
) WITHOUT ROWID;
PRAGMA user_version = 1;
";

/// The M0 hash of a `message.posted` event by the local principal, written
/// out from the documented version 1 encoding.
fn m0_hash(prev: &str, seq: i64, thread_id: &str, message_id: &str, at: &str) -> String {
    let field = |name: &str, value: &str| format!("{name}={}:{value}\n", value.len());
    let mut encoded = String::from("openboard.event.v1\n");
    encoded += &field("prev_hash", prev);
    encoded += &field("board", "general");
    encoded += &field("seq", &seq.to_string());
    encoded += &field("type", "message.posted");
    encoded += &field("channel", "research");
    encoded += &field("thread_id", thread_id);
    encoded += &field("message_id", message_id);
    encoded += &field("actor_id", "act_m0local");
    encoded += &field("actor_kind", "local");
    encoded += &field("actor_handle", "local");
    encoded += "actor_profile=-\n";
    encoded += &field("at", at);
    Sha256::digest(encoded.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Writes a data directory exactly as an M0 server left it: schema version
/// 1, one thread with a reply, hashes in encoding 1.
fn write_m0_directory(data_dir: &Path) {
    std::fs::create_dir_all(data_dir).unwrap();
    let conn = open_db(data_dir);
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(M0_SCHEMA).unwrap();
    let genesis = "0".repeat(64);
    let at = ["2026-10-01T10:00:00.000Z", "2026-10-01T10:05:00.000Z"];
    let first = m0_hash(&genesis, 1, "msg_m0root", "msg_m0root", at[0]);
    let second = m0_hash(&first, 2, "msg_m0root", "msg_m0reply", at[1]);
    conn.execute(
        "INSERT INTO boards (id, name, name_key, tag, created_at, head_seq, head_hash)
         VALUES (1, 'general', 'general', 'aabbccdd00112233', ?1, 2, ?2)",
        [at[0], second.as_str()],
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO actors VALUES ('act_m0local', 'local', 'local', NULL, 'local');
         INSERT INTO channels VALUES (1, 'research', '2026-10-01T10:00:00.000Z', 1, '2026-10-01T10:05:00.000Z');
         INSERT INTO messages VALUES ('msg_m0root', 1, 'research', 'msg_m0root', 'act_m0local', 'an M0 thread', '2026-10-01T10:00:00.000Z', 1, 1);
         INSERT INTO messages VALUES ('msg_m0reply', 1, 'research', 'msg_m0root', 'act_m0local', 'an M0 reply', '2026-10-01T10:05:00.000Z', 1, 2);
         INSERT INTO threads VALUES ('msg_m0root', 1, 'research', 1, 2, '2026-10-01T10:05:00.000Z');",
    )
    .unwrap();
    for (seq, message_id, at, prev, hash) in [
        (1, "msg_m0root", at[0], genesis.as_str(), first.as_str()),
        (2, "msg_m0reply", at[1], first.as_str(), second.as_str()),
    ] {
        conn.execute(
            "INSERT INTO events VALUES (1, ?1, 'message.posted', 'research', 'msg_m0root', ?2,
                                        'act_m0local', ?3, ?4, ?5)",
            rusqlite::params![seq, message_id, at, prev, hash],
        )
        .unwrap();
    }
}

fn verified(data_dir: &Path) -> u64 {
    let report = verify_log(data_dir).unwrap();
    assert!(report.is_ok(), "{report:?}");
    report.boards[0].verified_events
}

#[tokio::test]
async fn an_m0_data_directory_migrates_verifies_and_accepts_new_events() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = TempDir::new().unwrap();
    let data_dir = data_dir_of(&dir);
    write_m0_directory(&data_dir);
    // Before any new server opens it, the offline tools already read it.
    assert_eq!(verified(&data_dir), 2);
    assert_eq!(operator::board_info(&data_dir).unwrap().mode, Mode::Local);
    assert!(operator::security_log(&data_dir, None).unwrap().is_empty());
    assert!(matches!(
        operator::create_invite(&data_dir, "alice", &InviteOptions::default()),
        Err(OperatorError::NotNetwork)
    ));
    // An M0 directory is a local one: it cannot be served beyond loopback.
    let mut config = Config::new(&data_dir);
    config.port = 0;
    config.bind = "0.0.0.0".parse().unwrap();
    let error = openboard_server::bind(config.clone()).await.unwrap_err();
    assert!(matches!(error, ServeError::NonLoopbackBind(_)), "{error}");
    let version = |dir: &Path| -> i64 {
        open_db(dir)
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(version(&data_dir), 1, "a refused start migrates nothing");

    // Opening it as a local board migrates it in place.
    config.bind = "127.0.0.1".parse().unwrap();
    let server = openboard_server::bind(config).await.unwrap();
    assert_eq!(server.mode(), Mode::Local);
    let base = server.url().to_owned();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    assert_eq!(version(&data_dir), CURRENT_SCHEMA);
    assert_eq!(verified(&data_dir), 2);
    let http = Client::builder().no_proxy().build().unwrap();
    let thread: Thread = http
        .get(format!("{base}/v1/boards/general/threads/msg_m0reply"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(thread.root.text, "an M0 thread");
    assert_eq!(thread.replies[0].text, "an M0 reply");
    assert_eq!(thread.root.credential_id, None);
    let posted: PostMessageResponse = http
        .post(format!("{base}/v1/boards/general/messages"))
        .json(&json!({"thread_id": "msg_m0root", "text": "written after the migration"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(posted.message.thread_id, "msg_m0root");
    let events: EventsResponse = http
        .get(format!("{base}/v1/boards/general/events?after=start"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(events.events.len(), 3);
    assert_eq!(events.events[2].prev_hash, events.events[1].hash);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(verified(&data_dir), 3);
    let versions: Vec<i64> = open_db(&data_dir)
        .prepare("SELECT hash_version FROM events ORDER BY seq")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(versions, [1, 1, 2]);

    // It then upgrades to a network board, and the one chain carries on.
    let board = Net::start_in(dir, |_| {}).await;
    let (token, credential) = board.enrol("alice").await;
    let reply = board
        .post(
            &token,
            None,
            json!({"thread_id": "msg_m0root", "text": "from the network"}),
        )
        .await;
    assert_eq!(reply.message.credential_id, Some(credential.id));
    let dir = board.stop().await;
    assert_eq!(verified(&data_dir_of(&dir)), 4);
}

#[tokio::test]
async fn verify_log_detects_a_tampered_credential() {
    let board = Net::start().await;
    let (alice, _) = board.enrol("alice").await;
    let (_, bob_credential) = board.enrol("bob").await;
    board.thread(&alice, "a", "one").await;
    board.thread(&alice, "a", "two").await;
    let dir = board.stop().await;
    let data_dir = data_dir_of(&dir);
    assert_eq!(verified(&data_dir), 2);

    let conn = open_db(&data_dir);
    conn.execute_batch("DROP TRIGGER events_are_append_only_update")
        .unwrap();
    let original: String = conn
        .query_row(
            "SELECT credential_id FROM events WHERE seq = 2",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let broken_at = |change: &str| -> Option<u64> {
        conn.execute(&format!("UPDATE events SET {change} WHERE seq = 2"), [])
            .unwrap();
        let report = verify_log(&data_dir).unwrap();
        let broken = report.boards[0].broken.clone().map(|entry| entry.position);
        assert_eq!(
            report.boards[0].verified_events,
            if broken.is_some() { 1 } else { 2 }
        );
        broken
    };
    // Attributing the event to another credential, or to none, breaks it.
    assert_eq!(
        broken_at(&format!("credential_id = '{}'", bob_credential.id)),
        Some(2)
    );
    assert_eq!(broken_at("credential_id = NULL"), Some(2));
    // So does passing it off as an old-format event, with or without the
    // credential, and so does an encoding nobody knows.
    assert_eq!(broken_at("hash_version = 1"), Some(2));
    assert_eq!(
        broken_at(&format!("credential_id = '{original}', hash_version = 1")),
        Some(2)
    );
    assert_eq!(broken_at("hash_version = 3"), Some(2));
    // Putting everything back heals it.
    assert_eq!(broken_at("hash_version = 2"), None);
}

// ---------------------------------------------------------------------------
// Hardening: revocation under backlog, file modes, connection limits, modes
// ---------------------------------------------------------------------------

/// A raw TLS 1.3 connection to the board that trusts its pinned certificate.
async fn raw_tls(
    board: &Net,
    addr: SocketAddr,
    recv_buffer: Option<u32>,
) -> std::io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let digest = parse_fingerprint(&board.fingerprint).unwrap();
    let connector =
        tokio_rustls::TlsConnector::from(Arc::new(tls_config(digest, &[&rustls::version::TLS13])));
    let socket = if addr.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    if let Some(size) = recv_buffer {
        socket.set_recv_buffer_size(size)?;
    }
    let tcp = socket.connect(addr).await?;
    connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
}

/// The board's loopback address in IPv4, whatever it is bound to.
fn v4(board: &Net) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], board.addr.port()))
}

/// The board's loopback address in IPv6. Only a board from [`start_dual`]
/// listens there.
fn v6(board: &Net) -> SocketAddr {
    SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, board.addr.port()))
}

/// A network board listening on every address, so that a test can reach it
/// from two client addresses, `127.0.0.1` and `::1`. `None`, after a note on
/// stderr, on a host without an IPv6 loopback or a dual-stack `::`.
async fn start_dual(tweak: impl FnOnce(&mut Config)) -> Option<Net> {
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        eprintln!("skipped: this host has no IPv6 loopback address");
        return None;
    }
    let board = Net::start_with(|config| {
        config.bind = "::".parse().unwrap();
        config.public_url = Some("https://192.0.2.7:7878".to_owned());
        tweak(config);
    })
    .await;
    if tokio::net::TcpStream::connect(v4(&board)).await.is_err() {
        eprintln!("skipped: `::` does not accept IPv4 connections on this host");
        board.stop().await;
        return None;
    }
    Some(board)
}

/// Appends `count` events to the default board straight in the database, as
/// copies of its first event. Their hashes do not chain, which reading the
/// log does not look at; it is the cheapest way to a long backlog.
fn write_backlog(data_dir: &Path, count: i64) {
    let conn = open_db(data_dir);
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.execute_batch(&format!(
        "BEGIN IMMEDIATE;
         INSERT INTO events (board_id, seq, kind, channel, thread_id, message_id, actor_id, at,
                             prev_hash, hash, credential_id, hash_version)
         WITH RECURSIVE n(i) AS (SELECT 2 UNION ALL SELECT i + 1 FROM n WHERE i <= {count})
         SELECT e.board_id, n.i, e.kind, e.channel, e.thread_id, 'msg_backlog_' || n.i,
                e.actor_id, e.at, e.hash, e.hash, e.credential_id, e.hash_version
         FROM n JOIN events e ON e.seq = 1
         WHERE e.board_id = (SELECT id FROM boards WHERE name_key = 'general');
         UPDATE boards SET head_seq = {count} + 1 WHERE name_key = 'general';
         COMMIT;"
    ))
    .unwrap();
}

#[tokio::test]
async fn a_stalled_stream_with_a_backlog_ends_on_revocation() {
    const BACKLOG: i64 = 20_000;
    let board =
        Net::start_with(|config| config.limits.credential_recheck = Duration::from_millis(200))
            .await;
    let (alice, credential) = board.enrol("alice").await;
    let (bob, _) = board.enrol("bob").await;
    board.thread(&bob, "a", "before").await;
    let data_dir = board.data_dir();
    tokio::task::spawn_blocking(move || write_backlog(&data_dir, BACKLOG))
        .await
        .unwrap();

    // A reader that asks for the whole log, one event a page, and then does
    // not read: the server fills the socket and has to wait.
    let mut reader = raw_tls(&board, board.addr, Some(4096)).await.unwrap();
    let request = format!(
        "GET /v1/boards/general/events/stream?after=start&limit=1 HTTP/1.1\r\n\
         Host: localhost\r\nAuthorization: Bearer {alice}\r\nConnection: close\r\n\r\n"
    );
    reader.write_all(request.as_bytes()).await.unwrap();
    reader.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;

    let data_dir = board.data_dir();
    tokio::task::spawn_blocking(move || operator::revoke_credential(&data_dir, &credential.id))
        .await
        .unwrap()
        .unwrap();
    // Well past the re-check interval, an event the reader must never see.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let after = board.thread(&bob, "a", "after the revocation").await;

    // Now the reader drains everything the server will ever send it.
    let mut received = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(60), reader.read_to_end(&mut received))
        .await
        .expect("the stream ends");
    let text = String::from_utf8_lossy(&received);
    assert!(text.starts_with("HTTP/1.1 200"), "{:.200}", text);
    let delivered = text.matches("\"message_id\"").count();
    assert!(delivered >= 1, "the stream was being served");
    assert!(
        !text.contains(&after.message.id),
        "an event posted after the revocation was delivered ({delivered} events in all)"
    );
    // Only what was already on its way when the credential was revoked.
    assert!(
        (delivered as i64) < BACKLOG / 2,
        "{delivered} events were delivered to a revoked credential"
    );
    assert_eq!(board.status(&bob).await, StatusCode::OK);
    drop(reader);
    board.stop().await;
}

#[cfg(unix)]
#[tokio::test]
async fn an_existing_data_directory_and_its_database_are_made_private() {
    use std::os::unix::fs::PermissionsExt;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let chmod = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    let private = |data_dir: &Path, open: bool, network: bool| {
        assert_eq!(mode(data_dir), 0o700, "the data directory");
        let wal = format!("{DB_FILE_NAME}-wal");
        let shm = format!("{DB_FILE_NAME}-shm");
        let mut expected = vec![DB_FILE_NAME, openboard_server::LOCK_FILE_NAME];
        if open {
            expected.extend([wal.as_str(), shm.as_str()]);
        }
        if network {
            expected.push(KEY_FILE_NAME);
        }
        for name in expected {
            let path = data_dir.join(name);
            assert!(path.is_file(), "{name} exists");
            assert_eq!(mode(&path), 0o600, "{name}");
        }
        // Whatever else is there, nothing but the public certificate is
        // readable by anyone else.
        for entry in std::fs::read_dir(data_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().unwrap() != CERT_FILE_NAME {
                assert_eq!(mode(&path) & 0o077, 0, "{}", path.display());
            }
        }
    };
    let offline_tools = |data_dir: &Path| {
        let info = operator::board_info(data_dir).unwrap();
        operator::security_log(data_dir, None).unwrap();
        operator::list_credentials(data_dir).unwrap();
        if info.mode == Mode::Network {
            operator::create_invite(data_dir, "alice", &InviteOptions::default()).unwrap();
        }
        assert!(verify_log(data_dir).unwrap().is_ok());
    };

    for network in [false, true] {
        let dir = TempDir::new().unwrap();
        let data_dir = data_dir_of(&dir);
        std::fs::create_dir(&data_dir).unwrap();
        chmod(&data_dir, 0o755);
        let mut config = Config::new(&data_dir);
        config.port = 0;
        config.network = network;

        let server = openboard_server::bind(config.clone()).await.unwrap();
        private(&data_dir, true, network);
        // The operator functions and the verifier open the database a
        // second time. They work in the private directory and loosen nothing.
        offline_tools(&data_dir);
        private(&data_dir, true, network);
        drop(server);
        offline_tools(&data_dir);
        private(&data_dir, false, network);

        // A directory and a database that were loosened afterwards, as a
        // database written by an older version is, are tightened again.
        chmod(&data_dir, 0o755);
        chmod(&data_dir.join(DB_FILE_NAME), 0o644);
        chmod(&data_dir.join(openboard_server::LOCK_FILE_NAME), 0o644);
        offline_tools(&data_dir);
        let server = openboard_server::bind(config).await.unwrap();
        private(&data_dir, true, network);
        drop(server);
    }

    // A database path that is a symbolic link is refused, and what it
    // points at is neither created nor touched.
    let dir = TempDir::new().unwrap();
    let data_dir = data_dir_of(&dir);
    std::fs::create_dir(&data_dir).unwrap();
    let elsewhere = dir.path().join("elsewhere.db");
    std::os::unix::fs::symlink(&elsewhere, data_dir.join(DB_FILE_NAME)).unwrap();
    let mut config = Config::new(&data_dir);
    config.port = 0;
    let error = openboard_server::bind(config.clone()).await.unwrap_err();
    assert!(matches!(error, ServeError::Storage(_)), "{error}");
    assert!(error.to_string().contains("symbolic link"), "{error}");
    assert!(!elsewhere.exists());
    // The data directory itself may be one.
    std::fs::remove_file(data_dir.join(DB_FILE_NAME)).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&data_dir, &link).unwrap();
    config.data_dir = link;
    let server = openboard_server::bind(config).await.unwrap();
    assert!(data_dir.join(DB_FILE_NAME).is_file());
    assert_eq!(mode(&data_dir), 0o700);
    drop(server);
}

#[tokio::test]
async fn silent_connections_from_one_address_do_not_hold_up_another() {
    let Some(board) = start_dual(|_| {}).await else {
        return;
    };
    // One address opens far more connections than it may have handshakes
    // pending, and says nothing on any of them.
    let mut silent = Vec::new();
    for _ in 0..300 {
        silent.push(tokio::net::TcpStream::connect(v4(&board)).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let url = format!("https://[::1]:{}/v1/health", board.addr.port());
    let response = tokio::time::timeout(Duration::from_secs(2), board.http.get(url).send())
        .await
        .expect("another address is served without waiting")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(started.elapsed() < Duration::from_secs(2));

    // The silent connections are hung up on when the handshake time is
    // over, and then their own address is served again too.
    let mut first = silent.swap_remove(0);
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(8), first.read_to_end(&mut answer))
        .await
        .expect("a connection that never starts a handshake is closed");
    assert!(answer.is_empty());
    let response = board.http.get(board.at("/v1/health")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(silent);
    board.stop().await;
}

/// Reads until the peer closes the connection. Panics if it does not within
/// `within`. Returns what arrived first.
async fn closed_within<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    within: Duration,
) -> Vec<u8> {
    let mut received = Vec::new();
    let _ = tokio::time::timeout(within, stream.read_to_end(&mut received))
        .await
        .expect("the server closes the connection");
    received
}

#[tokio::test]
async fn a_connection_that_does_not_send_a_request_head_is_closed() {
    let quick = |config: &mut Config| {
        config.limits.header_read_timeout = Duration::from_millis(400);
    };
    let board = Net::start_with(quick).await;
    let (token, _) = board.enrol("alice").await;
    let within = Duration::from_secs(5);

    // Nothing after the handshake.
    let mut idle = raw_tls(&board, board.addr, None).await.unwrap();
    // Half a request head, then nothing.
    let mut half = raw_tls(&board, board.addr, None).await.unwrap();
    half.write_all(b"GET /v1/health HTTP/1.1\r\nHost: local")
        .await
        .unwrap();
    half.flush().await.unwrap();
    // A whole request, answered, and then the connection is left idle.
    let mut kept = raw_tls(&board, board.addr, None).await.unwrap();
    kept.write_all(b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    // A slow enrolment: the head arrives, the body never does.
    let mut slow = raw_tls(&board, board.addr, None).await.unwrap();
    slow.write_all(
        b"POST /v1/join HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
          Content-Length: 80\r\nConnection: close\r\n\r\n{\"code\":",
    )
    .await
    .unwrap();
    slow.flush().await.unwrap();

    let started = std::time::Instant::now();
    let answer = closed_within(&mut idle, within).await;
    assert!(!String::from_utf8_lossy(&answer).contains("200 OK"));
    let answer = closed_within(&mut half, within).await;
    assert!(!String::from_utf8_lossy(&answer).contains("200 OK"));
    let answer = closed_within(&mut kept, within).await;
    assert!(String::from_utf8_lossy(&answer).starts_with("HTTP/1.1 200 OK"));
    let answer = closed_within(&mut slow, within).await;
    assert!(
        String::from_utf8_lossy(&answer).starts_with("HTTP/1.1 408"),
        "{}",
        String::from_utf8_lossy(&answer)
    );
    assert!(started.elapsed() < within);

    // A long-poll and a stream outlast the timeout many times over: it
    // times the request head, not the response.
    let stream = board
        .get(&token, "/v1/boards/general/events/stream")
        .send()
        .await
        .unwrap();
    let stream = tokio::spawn(drain(stream));
    let started = std::time::Instant::now();
    let response = board
        .get(&token, "/v1/boards/general/events?wait=2")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(started.elapsed() >= Duration::from_millis(1900));
    assert!(!stream.is_finished());
    board.thread(&token, "a", "still listening").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!stream.is_finished());
    let dir = board.stop().await;
    let sent = stream.await.unwrap();
    assert!(sent.contains("data:"), "{sent}");
    drop(dir);

    // A local board has the same timeout.
    let dir = TempDir::new().unwrap();
    let mut config = Config::new(data_dir_of(&dir));
    config.port = 0;
    quick(&mut config);
    let server = openboard_server::bind(config).await.unwrap();
    let addr = server.local_addr();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    let mut half = tokio::net::TcpStream::connect(addr).await.unwrap();
    half.write_all(b"GET /v1/health HTTP/1.1\r\nHost: local")
        .await
        .unwrap();
    let answer = closed_within(&mut half, within).await;
    assert!(!String::from_utf8_lossy(&answer).contains("200 OK"));
    let http = Client::builder().no_proxy().build().unwrap();
    let response = http
        .get(format!("http://{addr}/v1/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn connections_are_capped_per_address_and_in_total() {
    let Some(board) = start_dual(|config| {
        config.limits.max_connections_per_address = 3;
        config.limits.max_connections = 5;
        // Idle connections stay, so the test decides when they go.
        config.limits.header_read_timeout = Duration::from_secs(60);
    })
    .await
    else {
        return;
    };
    let within = Duration::from_secs(2);
    // Whether a new connection from this address gets a handshake. One
    // over a cap is closed straight away, without one.
    let admitted = |addr: SocketAddr| {
        let board = &board;
        async move {
            tokio::time::timeout(within, raw_tls(board, addr, None))
                .await
                .expect("an answer either way, promptly")
                .ok()
        }
    };

    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(admitted(v4(&board)).await.expect("within the cap"));
    }
    // The address is full. Another address is served as ever.
    assert!(admitted(v4(&board)).await.is_none());
    assert!(admitted(v4(&board)).await.is_none());
    let other = Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .use_preconfigured_tls(tls_config(
            parse_fingerprint(&board.fingerprint).unwrap(),
            rustls::ALL_VERSIONS,
        ))
        .build()
        .unwrap();
    let health = format!("https://[::1]:{}/v1/health", board.addr.port());
    let response = other.get(&health).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    // The connections that are open are unharmed.
    let kept = &mut held[0];
    kept.write_all(b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut answer = [0u8; 15];
    kept.read_exact(&mut answer).await.unwrap();
    assert_eq!(&answer, b"HTTP/1.1 200 OK");

    // Two more from the other address fill the board: five in all. The
    // connection of the request above takes a moment to be gone.
    let mut more = Vec::new();
    for _ in 0..50 {
        match admitted(v6(&board)).await {
            Some(connection) => more.push(connection),
            None => tokio::time::sleep(Duration::from_millis(50)).await,
        }
        if more.len() == 2 {
            break;
        }
    }
    assert_eq!(more.len(), 2);
    assert!(admitted(v6(&board)).await.is_none());
    assert!(other.get(&health).send().await.is_err());

    // Places come back when connections close.
    drop(more);
    let mut freed = false;
    for _ in 0..50 {
        if other.get(&health).send().await.is_ok() {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(freed);
    held.pop();
    let mut freed = false;
    for _ in 0..50 {
        if let Some(connection) = admitted(v4(&board)).await {
            held.push(connection);
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(freed);
    drop(held);
    board.stop().await;
}

fn delete_mode_record(data_dir: &Path) {
    let changed = open_db(data_dir)
        .execute("DELETE FROM meta WHERE key = 'mode'", [])
        .unwrap();
    assert_eq!(changed, 1);
}

#[tokio::test]
async fn a_network_directory_without_its_mode_record_is_still_a_network_board() {
    // With credentials: the rows give it away, even with the certificate
    // gone and the mode forged to local.
    let board = Net::start().await;
    let fingerprint = board.fingerprint.clone();
    let (token, _) = board.enrol("alice").await;
    let dir = board.stop().await;
    let data_dir = data_dir_of(&dir);
    delete_mode_record(&data_dir);
    assert_eq!(
        operator::board_info(&data_dir).unwrap().mode,
        Mode::Network,
        "the operator functions see through it too"
    );
    let mut config = Config::new(&data_dir);
    config.port = 0;
    // Asking for a non-loopback local board is refused as for any network
    // directory: with the public URL error, not by binding without TLS.
    config.bind = "0.0.0.0".parse().unwrap();
    let error = openboard_server::bind(config.clone()).await.unwrap_err();
    assert!(matches!(error, ServeError::PublicUrlRequired(_)), "{error}");
    let board = Net::start_in(dir, |config| config.network = false).await;
    assert_eq!(board.fingerprint, fingerprint);
    let plain = Client::builder().no_proxy().build().unwrap();
    assert!(plain
        .get(format!("http://127.0.0.1:{}/v1/health", board.addr.port()))
        .send()
        .await
        .is_err());
    assert_eq!(board.status(&token).await, StatusCode::OK);
    assert_eq!(board.status("garbage").await, StatusCode::UNAUTHORIZED);
    let dir = board.stop().await;
    let info = operator::board_info(&data_dir).unwrap();
    assert_eq!(info.mode, Mode::Network);
    let recorded: String = open_db(&data_dir)
        .query_row("SELECT value FROM meta WHERE key = 'mode'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(recorded, MODE_NETWORK, "the record is repaired");
    assert!(kinds(&data_dir).contains(&log_kinds::MODE_REPAIRED.to_owned()));

    open_db(&data_dir)
        .execute("UPDATE meta SET value = 'local' WHERE key = 'mode'", [])
        .unwrap();
    std::fs::remove_file(data_dir.join(CERT_FILE_NAME)).unwrap();
    std::fs::remove_file(data_dir.join(KEY_FILE_NAME)).unwrap();
    let mut config = Config::new(&data_dir);
    config.port = 0;
    let server = openboard_server::bind(config).await.unwrap();
    assert_eq!(server.mode(), Mode::Network);
    drop(server);
    drop(dir);

    // Without any agent yet: the certificate gives it away.
    let board = Net::start().await;
    let dir = board.stop().await;
    let data_dir = data_dir_of(&dir);
    delete_mode_record(&data_dir);
    let mut config = Config::new(&data_dir);
    config.port = 0;
    let server = openboard_server::bind(config).await.unwrap();
    assert_eq!(server.mode(), Mode::Network);
    assert!(server.url().starts_with("https://"));
    drop(server);
}

#[tokio::test]
async fn a_start_that_cannot_bind_does_not_upgrade_a_local_directory() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = TempDir::new().unwrap();
    let data_dir = data_dir_of(&dir);
    let mut config = Config::new(&data_dir);
    config.port = 0;
    let server = openboard_server::bind(config.clone()).await.unwrap();
    assert_eq!(server.mode(), Mode::Local);
    drop(server);

    // The port is taken, so the network start fails.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut upgrade = config.clone();
    upgrade.network = true;
    upgrade.port = taken.local_addr().unwrap().port();
    let error = openboard_server::bind(upgrade.clone()).await.unwrap_err();
    assert!(matches!(error, ServeError::Bind { .. }), "{error}");

    // Nothing about the directory says network afterwards.
    assert_eq!(operator::board_info(&data_dir).unwrap().mode, Mode::Local);
    assert!(!data_dir.join(CERT_FILE_NAME).exists());
    assert!(!data_dir.join(KEY_FILE_NAME).exists());
    assert!(!kinds(&data_dir).contains(&log_kinds::MODE_UPGRADED.to_owned()));
    let server = openboard_server::bind(config).await.unwrap();
    assert_eq!(server.mode(), Mode::Local);
    assert!(server.url().starts_with("http://"));
    drop(server);

    // With the port free the same start upgrades it.
    drop(taken);
    let server = openboard_server::bind(upgrade).await.unwrap();
    assert_eq!(server.mode(), Mode::Network);
    assert!(kinds(&data_dir).contains(&log_kinds::MODE_UPGRADED.to_owned()));
    drop(server);
}

// ---------------------------------------------------------------------------
// Milestone M2: message features and the web UI's static files
// ---------------------------------------------------------------------------

impl Net {
    /// Sends a request with a bearer token and, when given, a JSON body.
    async fn send(
        &self,
        token: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Response {
        let mut request = self.http.request(method, self.at(path)).bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.unwrap()
    }

    async fn read(&self, token: &str, id: &str) -> Thread {
        let response = self
            .get(token, &format!("/v1/boards/general/threads/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }
}

async fn message_of(response: Response) -> openboard_api::Message {
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

/// The first event frame of an event stream.
async fn first_frame(response: &mut Response) -> Event {
    let mut buffer = String::new();
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(10), response.chunk())
            .await
            .expect("the stream delivers in time")
            .unwrap()
            .expect("the stream stays open");
        buffer.push_str(std::str::from_utf8(&chunk).unwrap());
        while let Some(end) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..end + 2).collect();
            if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data:")) {
                return serde_json::from_str(data.trim_start()).unwrap();
            }
        }
    }
}

#[tokio::test]
async fn only_the_author_edits_or_deletes_and_everyone_labels_and_reacts() {
    use reqwest::Method;
    let board = Net::start().await;
    let (alice, alice_credential) = board.enrol("alice").await;
    let (bob, bob_credential) = board.enrol("bob").await;
    let root = board
        .thread(&alice, "dev", "alice wrote this")
        .await
        .message;
    let path = format!("/v1/boards/general/messages/{}", root.id);

    // Bob can read it and cannot change or remove it.
    let response = board
        .send(
            &bob,
            Method::PATCH,
            &path,
            Some(json!({"text": "bob's now"})),
        )
        .await;
    error_of(response, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    let response = board.send(&bob, Method::DELETE, &path, None).await;
    error_of(response, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    // Neither can alice under a profile the message was not written with.
    let response = board
        .http
        .patch(board.at(&path))
        .bearer_auth(&alice)
        .header(PROFILE_HEADER, "fx")
        .json(&json!({"text": "as another actor"}))
        .send()
        .await
        .unwrap();
    error_of(response, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    assert_eq!(board.read(&bob, &root.id).await.root, root);

    // Labels and reactions are open to every enrolled principal.
    let labelled = board
        .send(
            &bob,
            Method::POST,
            &format!("{path}/labels"),
            Some(json!({"set": {"status": "claimed"}, "if_unset": ["status"]})),
        )
        .await;
    assert_eq!(message_of(labelled).await.labels["status"], "claimed");
    let raced = board
        .send(
            &alice,
            Method::POST,
            &format!("{path}/labels"),
            Some(json!({"set": {"status": "mine"}, "if_unset": ["status"]})),
        )
        .await;
    error_of(raced, StatusCode::CONFLICT, codes::LABEL_CONFLICT).await;
    for token in [&bob, &alice, &bob] {
        let reacted = board
            .send(
                token,
                Method::PUT,
                &format!("{path}/reactions/thumbsup"),
                None,
            )
            .await;
        assert_eq!(reacted.status(), StatusCode::OK);
    }
    let reacted = board.read(&alice, &root.id).await.root;
    assert_eq!(reacted.reactions.len(), 1);
    // One each, oldest first, by the principal's handle.
    assert_eq!(reacted.reactions[0].by, ["bob", "alice"]);
    // Removing one's own reaction leaves the other's.
    let removed = board
        .send(
            &bob,
            Method::DELETE,
            &format!("{path}/reactions/thumbsup"),
            None,
        )
        .await;
    assert_eq!(message_of(removed).await.reactions[0].by, ["alice"]);

    // Alice edits and deletes her own message.
    let edited = board
        .send(
            &alice,
            Method::PATCH,
            &path,
            Some(json!({"text": "alice rewrote this", "revision": 1})),
        )
        .await;
    let edited = message_of(edited).await;
    assert_eq!(edited.revision, 2);
    assert_eq!(edited.by.handle, "alice");
    let deleted = board.send(&alice, Method::DELETE, &path, None).await;
    assert!(message_of(deleted).await.deleted_at.is_some());
    // Bob still may not delete it, tombstone or not.
    let response = board.send(&bob, Method::DELETE, &path, None).await;
    error_of(response, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;

    // Each event is by whoever made the change, with their credential.
    let log = board.events(&alice, None, "after=start").await;
    let seen: Vec<(&str, &str, Option<&str>)> = log
        .events
        .iter()
        .map(|event| {
            (
                event.kind.as_str(),
                event.by.handle.as_str(),
                event.credential_id.as_deref(),
            )
        })
        .collect();
    let (a, b) = (
        Some(alice_credential.id.as_str()),
        Some(bob_credential.id.as_str()),
    );
    assert_eq!(
        seen,
        [
            ("message.posted", "alice", a),
            ("labels.changed", "bob", b),
            ("reactions.changed", "bob", b),
            ("reactions.changed", "alice", a),
            ("reactions.changed", "bob", b),
            ("message.edited", "alice", a),
            ("message.deleted", "alice", a),
        ]
    );
    assert!(log
        .events
        .iter()
        .all(|event| event.by.kind == ActorKind::Agent));

    // Without a token none of the new routes says anything.
    for (method, path, body) in [
        (Method::PATCH, path.clone(), Some(json!({"text": "x"}))),
        (Method::DELETE, path.clone(), None),
        (Method::POST, format!("{path}/labels"), Some(json!({}))),
        (Method::PUT, format!("{path}/reactions/eyes"), None),
        (Method::DELETE, format!("{path}/reactions/eyes"), None),
        (Method::GET, "/v1/boards".to_owned(), None),
    ] {
        let mut request = board.http.request(method, board.at(&path));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        error_of(response, StatusCode::UNAUTHORIZED, codes::UNAUTHENTICATED).await;
    }
    let dir = board.stop().await;
    assert_eq!(verified(&data_dir_of(&dir)), 7);
}

#[tokio::test]
async fn a_mention_wakes_the_mentioned_agents_long_poll_and_stream() {
    let board = Arc::new(Net::start().await);
    let (alice, _) = board.enrol("agent-a").await;
    let (carol, _) = board.enrol("Agent-C").await;
    board.enrol("agent-b").await;

    let waiter = {
        let (board, carol) = (Arc::clone(&board), carol.clone());
        tokio::spawn(async move {
            board
                .events(&carol, None, "wait=30&mentions_self=true")
                .await
        })
    };
    let mut stream = board
        .get(
            &carol,
            "/v1/boards/general/events/stream?mentions_self=true",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    // A second stream, as the same agent under a declared profile.
    let mut reviewer = board
        .get(
            &carol,
            "/v1/boards/general/events/stream?mentions_self=true",
        )
        .header(PROFILE_HEADER, "reviewer")
        .send()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // None of these is for carol with no profile.
    let quiet = board.thread(&alice, "dev", "nothing for anyone").await;
    assert!(quiet.message.mentions.is_empty());
    let other = board.thread(&alice, "dev", "over to @agent-b").await;
    assert_eq!(other.message.mentions[0].handle, "agent-b");
    let coded = board
        .thread(&alice, "dev", "`@agent-c` and @agent-cc")
        .await;
    assert!(coded.message.mentions.is_empty());
    // On a network board `local` is nobody.
    let local = board.thread(&alice, "dev", "@local and @anonymous").await;
    assert!(local.message.mentions.is_empty());
    let profiled = board
        .thread(&alice, "dev", "for @agent-c/reviewer only")
        .await;
    assert_eq!(profiled.message.mentions.len(), 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished());
    // The profiled stream did get the one that names its profile.
    assert_eq!(
        first_frame(&mut reviewer).await.message_id,
        profiled.message.id
    );

    // Handles match without regard to case, and the mention records the
    // handle as the board has it, with the principal's kind.
    let wanted = board
        .thread(&alice, "dev", "please review, @AGENT-C.")
        .await
        .message;
    assert_eq!(wanted.mentions.len(), 1);
    assert_eq!(wanted.mentions[0].handle, "Agent-C");
    assert_eq!(wanted.mentions[0].kind, ActorKind::Agent);
    assert_eq!(wanted.mentions[0].profile, None);

    let woke = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the mention wakes the long-poll")
        .unwrap();
    assert_eq!(woke.events.len(), 1);
    assert_eq!(woke.events[0].message_id, wanted.id);
    assert_eq!(woke.events[0].by.handle, "agent-a");
    assert_eq!(first_frame(&mut stream).await.message_id, wanted.id);
    assert_eq!(first_frame(&mut reviewer).await.message_id, wanted.id);

    // Alice is not woken by her own mention of someone else.
    let mine = board
        .events(&alice, None, "after=start&mentions_self=true")
        .await;
    assert!(mine.events.is_empty());
    // The rendered text marks the mention.
    let thread: Thread = board
        .get(
            &carol,
            &format!("/v1/boards/general/threads/{}?html=true", wanted.id),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        thread.root.html.as_deref(),
        Some("<p>please review, <span class=\"mention\">@AGENT-C</span>.</p>")
    );
    drop((stream, reviewer));
    Arc::into_inner(board).unwrap().stop().await;
}

#[tokio::test]
async fn the_static_pages_need_no_token_and_nothing_else_is_exempt() {
    use reqwest::Method;
    let board = Net::start().await;
    let (token, _) = board.enrol("alice").await;
    board
        .thread(&token, "dev", "SAFFRON-KITE board content")
        .await;
    let csp = "default-src 'none'; script-src 'self'; style-src 'self'; \
               img-src 'self' data:; connect-src 'self'; base-uri 'none'; \
               form-action 'none'; frame-ancestors 'none'";
    let assert_headers = |response: &Response, what: &str| {
        let headers = response.headers();
        assert_eq!(headers["content-security-policy"], csp, "{what}");
        assert_eq!(headers["x-content-type-options"], "nosniff", "{what}");
        assert_eq!(headers["referrer-policy"], "no-referrer", "{what}");
        assert!(
            headers
                .keys()
                .all(|name| !name.as_str().starts_with("access-control-")),
            "{what}"
        );
    };

    // The page and its files, with no credential at all, and with a bad one.
    for path in ["/", "/join", "/assets/app.js", "/assets/app.css"] {
        for authorization in [None, Some("Bearer garbage")] {
            let mut request = board.http.get(board.at(path));
            if let Some(value) = authorization {
                request = request.header("authorization", value);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_headers(&response, path);
            let cache = if path.starts_with("/assets/") {
                "no-cache"
            } else {
                "no-store"
            };
            assert_eq!(response.headers()["cache-control"], cache, "{path}");
            // They hold no board content.
            let body = response.text().await.unwrap();
            assert!(!body.is_empty() && !body.contains("SAFFRON-KITE"));
        }
    }
    let unknown = board
        .http
        .get(board.at("/assets/nope.js"))
        .send()
        .await
        .unwrap();
    assert_headers(&unknown, "an unknown asset");
    error_of(unknown, StatusCode::NOT_FOUND, codes::NOT_FOUND).await;

    // Everything else still needs a token, and says the same thing without.
    let mut refusals = Vec::new();
    for (method, path) in [
        (Method::GET, "/v1/boards"),
        (Method::GET, "/v1/boards/general/channels"),
        (Method::GET, "/v1/whoami"),
        (Method::GET, "/v1/web/session"),
        // Signing in is a `POST`; see `tests/web.rs`.
        (Method::GET, "/v1/web/login"),
        (Method::PUT, "/v1/web/login"),
        (Method::GET, "/index.html"),
        (Method::GET, "/assets"),
        (Method::GET, "/assets/"),
        (Method::GET, "/assets/a/b"),
        (Method::GET, "/join/"),
        (Method::GET, "/nope"),
        (Method::POST, "/"),
        (Method::PUT, "/join"),
        (Method::DELETE, "/assets/app.js"),
        (Method::POST, "/assets/app.js"),
    ] {
        let what = format!("{method} {path}");
        let response = board
            .http
            .request(method, board.at(path))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{what}");
        assert_headers(&response, &what);
        assert_eq!(response.headers()["cache-control"], "no-store", "{what}");
        refusals.push(response.text().await.unwrap());
    }
    assert!(refusals.iter().all(|body| body == &refusals[0]));
    assert!(refusals[0].contains(codes::UNAUTHENTICATED));
    let head = board.http.head(board.at("/")).send().await.unwrap();
    assert_eq!(head.status(), StatusCode::UNAUTHORIZED);

    // With a token the API answers, and carries the same headers.
    let boards = board.get(&token, "/v1/boards").send().await.unwrap();
    assert_eq!(boards.status(), StatusCode::OK);
    assert_headers(&boards, "the board list");
    assert_eq!(boards.headers()["cache-control"], "no-store");
    let boards: openboard_api::BoardList = boards.json().await.unwrap();
    assert_eq!(boards.boards.len(), 1);
    assert_eq!(boards.boards[0].name, "general");
    assert!(boards.boards[0].last_activity_at.is_some());
    let unknown = board.get(&token, "/assets/nope.js").send().await.unwrap();
    error_of(unknown, StatusCode::NOT_FOUND, codes::NOT_FOUND).await;
    // The session route answers a token as it does a cookie; `tests/web.rs`
    // covers it.
    let session = board.get(&token, "/v1/web/session").send().await.unwrap();
    assert_eq!(session.status(), StatusCode::OK);
    assert_headers(&session, "the session description");
    board.stop().await;
}

/// What milestone M1 added to the M0 schema, frozen here as a fixture.
const M1_MIGRATION: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE principals (
    id TEXT PRIMARY KEY, kind TEXT NOT NULL, handle TEXT NOT NULL,
    handle_key TEXT NOT NULL UNIQUE, created_ms INTEGER NOT NULL
);
CREATE TABLE credentials (
    id TEXT PRIMARY KEY, principal_id TEXT NOT NULL REFERENCES principals(id),
    token_hash TEXT NOT NULL UNIQUE, created_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL, revoked_ms INTEGER, last_used_ms INTEGER
);
CREATE INDEX credentials_by_principal ON credentials(principal_id);
CREATE TABLE invites (
    id TEXT PRIMARY KEY, principal_id TEXT NOT NULL REFERENCES principals(id),
    code_hash TEXT NOT NULL UNIQUE, token_ttl_ms INTEGER NOT NULL,
    created_ms INTEGER NOT NULL, expires_ms INTEGER NOT NULL,
    used_ms INTEGER, revoked_ms INTEGER
);
CREATE INDEX invites_by_principal ON invites(principal_id);
CREATE TABLE security_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT, at TEXT NOT NULL, kind TEXT NOT NULL,
    principal TEXT, credential_id TEXT, client TEXT, detail TEXT
);
CREATE TRIGGER security_log_is_append_only_update BEFORE UPDATE ON security_log
BEGIN SELECT RAISE(ABORT, 'the security log is append-only'); END;
CREATE TRIGGER security_log_is_append_only_delete BEFORE DELETE ON security_log
BEGIN SELECT RAISE(ABORT, 'the security log is append-only'); END;
ALTER TABLE messages ADD COLUMN credential_id TEXT;
ALTER TABLE events ADD COLUMN credential_id TEXT;
ALTER TABLE events ADD COLUMN hash_version INTEGER NOT NULL DEFAULT 1;
PRAGMA user_version = 2;
";

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Writes a data directory as an M1 server left a network board: schema
/// version 2, one agent with a live credential, and one thread by it with
/// its event in hash encoding 2. Returns the agent's token and its message.
fn write_m1_directory(data_dir: &Path) -> (String, &'static str) {
    std::fs::create_dir_all(data_dir).unwrap();
    let conn = open_db(data_dir);
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(M0_SCHEMA).unwrap();
    conn.execute_batch(M1_MIGRATION).unwrap();

    let token = format!("{TOKEN_PREFIX}{}", "5e".repeat(32));
    let (principal, credential, message) = ("prn_m1alice", "crd_m1alice", "msg_m1root");
    let at = "2026-11-01T09:00:00.000Z";
    // The actor ID as every version derives it from the principal.
    let actor = format!(
        "act_{}",
        &sha256_hex(&format!("openboard.actor.v1\n{principal}\n"))[..24]
    );
    let genesis = "0".repeat(64);
    let field = |name: &str, value: &str| format!("{name}={}:{value}\n", value.len());
    let mut encoded = String::from("openboard.event.v2\n");
    encoded += &field("prev_hash", &genesis);
    encoded += &field("board", "general");
    encoded += &field("seq", "1");
    encoded += &field("type", "message.posted");
    encoded += &field("channel", "research");
    encoded += &field("thread_id", message);
    encoded += &field("message_id", message);
    encoded += &field("actor_id", &actor);
    encoded += &field("actor_kind", "agent");
    encoded += &field("actor_handle", "alice");
    encoded += "actor_profile=-\n";
    encoded += &field("at", at);
    encoded += &field("credential_id", credential);
    let hash = sha256_hex(&encoded);

    conn.execute(
        "INSERT INTO boards (id, name, name_key, tag, created_at, head_seq, head_hash)
         VALUES (1, 'general', 'general', '1122334455667788', ?1, 1, ?2)",
        [at, hash.as_str()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO principals VALUES (?1, 'agent', 'alice', 'alice', 1)",
        [principal],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO credentials VALUES (?1, ?2, ?3, 1, 9999999999999, NULL, NULL)",
        [credential, principal, sha256_hex(&token).as_str()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO actors VALUES (?1, 'agent', ?2, NULL, 'alice')",
        [actor.as_str(), principal],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO messages VALUES (?1, 1, 'research', ?1, ?2, 'an M1 thread, cc @alice', ?3, 1, 1, ?4)",
        [message, actor.as_str(), at, credential],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO events VALUES (1, 1, 'message.posted', 'research', ?1, ?1, ?2, ?3, ?4, ?5, ?6, 2)",
        [message, actor.as_str(), at, genesis.as_str(), hash.as_str(), credential],
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO channels VALUES (1, 'research', '2026-11-01T09:00:00.000Z', 1, '2026-11-01T09:00:00.000Z');
         INSERT INTO threads VALUES ('msg_m1root', 1, 'research', 0, 1, '2026-11-01T09:00:00.000Z');
         INSERT INTO meta VALUES ('mode', 'network');",
    )
    .unwrap();
    (token, message)
}

#[tokio::test]
async fn an_m1_data_directory_migrates_verifies_and_takes_the_new_features() {
    use reqwest::Method;
    let dir = TempDir::new().unwrap();
    let data_dir = data_dir_of(&dir);
    let (alice, message) = write_m1_directory(&data_dir);
    let version = |dir: &Path| -> i64 {
        open_db(dir)
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    };
    // Before a new server opens it, the offline tools read it as it is.
    assert_eq!(version(&data_dir), 2);
    assert_eq!(verified(&data_dir), 1);
    assert_eq!(operator::board_info(&data_dir).unwrap().mode, Mode::Network);
    assert_eq!(operator::list_credentials(&data_dir).unwrap().len(), 1);
    assert_eq!(version(&data_dir), 2, "reading migrates nothing");
    // A network directory cannot be opened as a local board.
    let mut config = Config::new(&data_dir);
    config.port = 0;
    let server = openboard_server::bind(config).await.unwrap();
    assert_eq!(server.mode(), Mode::Network);
    drop(server);
    assert_eq!(version(&data_dir), CURRENT_SCHEMA);
    assert_eq!(verified(&data_dir), 1);

    let board = Net::start_in(dir, |_| {}).await;
    assert_eq!(board.status(&alice).await, StatusCode::OK);
    // The old message reads with the new fields at their defaults. Its
    // text was written before mentions existed, so it has none.
    let old = board.read(&alice, message).await.root;
    assert_eq!(old.text, "an M1 thread, cc @alice");
    assert_eq!(old.revision, 1);
    assert_eq!((&old.edited_at, &old.deleted_at), (&None, &None));
    assert!(old.labels.is_empty() && old.reactions.is_empty() && old.mentions.is_empty());
    assert_eq!(old.by.handle, "alice");
    let before = board
        .events(&alice, None, "after=start&mentions_self=true")
        .await;
    assert!(before.events.is_empty());

    // Its author is still its author, and every new operation works on it.
    let (bob, _) = board.enrol("bob").await;
    let path = format!("/v1/boards/general/messages/{message}");
    let response = board
        .send(
            &bob,
            Method::PATCH,
            &path,
            Some(json!({"text": "not bob's"})),
        )
        .await;
    error_of(response, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    let edited = board
        .send(
            &alice,
            Method::PATCH,
            &path,
            Some(json!({"text": "an M1 thread, edited, cc @alice and @bob", "revision": 1})),
        )
        .await;
    let edited = message_of(edited).await;
    assert_eq!(edited.revision, 2);
    assert_eq!(edited.mentions.len(), 2);
    let labelled = board
        .send(
            &bob,
            Method::POST,
            &format!("{path}/labels"),
            Some(json!({"set": {"status": "seen"}})),
        )
        .await;
    assert_eq!(message_of(labelled).await.labels.len(), 1);
    let reacted = board
        .send(&bob, Method::PUT, &format!("{path}/reactions/eyes"), None)
        .await;
    assert_eq!(message_of(reacted).await.reactions[0].by, ["bob"]);
    let reply = board
        .post(&bob, None, json!({"thread_id": message, "text": "a reply"}))
        .await;
    assert_eq!(reply.message.thread_id, message);
    let mentioned = board
        .events(
            &bob,
            None,
            "after=start&mentions_self=true&types=message.edited",
        )
        .await;
    assert_eq!(mentioned.events.len(), 1);
    let deleted = board.send(&alice, Method::DELETE, &path, None).await;
    assert_eq!(message_of(deleted).await.revision, 3);

    // The one chain carries on from the M1 event, in the same encoding.
    let log = board.events(&alice, None, "after=start").await;
    assert_eq!(log.events.len(), 6);
    for pair in log.events.windows(2) {
        assert_eq!(pair[1].prev_hash, pair[0].hash);
    }
    let dir = board.stop().await;
    let data_dir = data_dir_of(&dir);
    assert_eq!(verified(&data_dir), 6);
    let versions: Vec<i64> = open_db(&data_dir)
        .prepare("SELECT hash_version FROM events ORDER BY seq")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(versions, [2; 6]);
    // Opening it again changes nothing.
    let board = Net::start_in(dir, |_| {}).await;
    assert_eq!(board.status(&alice).await, StatusCode::OK);
    let dir = board.stop().await;
    assert_eq!(version(&data_dir_of(&dir)), CURRENT_SCHEMA);
    assert_eq!(verified(&data_dir_of(&dir)), 6);
}

// ---------------------------------------------------------------------------
// Upgrades: backups before migrations, and refusing newer databases
// ---------------------------------------------------------------------------

/// The schema version this build writes.
const CURRENT_SCHEMA: i64 = 4;

fn schema_of(path: &Path) -> i64 {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

/// Opens a data directory with a server of this version, as a start would,
/// and closes it again.
async fn start_once(data_dir: &Path) -> Result<(), ServeError> {
    let mut config = Config::new(data_dir);
    config.port = 0;
    openboard_server::bind(config).await.map(drop)
}

/// Checks a backup the way an operator rolling back would use it: it is
/// private, it opens at the schema it was taken at, and its event log
/// verifies. Returns the backup's path.
fn check_backup(data_dir: &Path, before: i64, events: u64) -> PathBuf {
    let backups = operator::list_backups(data_dir).unwrap();
    assert_eq!(backups.len(), 1, "{backups:?}");
    let backup = &backups[0];
    assert!(
        backup
            .file_name
            .starts_with(&format!("openboard.db.pre-v{CURRENT_SCHEMA}-")),
        "{backup:?}"
    );
    assert!(backup.file_name.ends_with('Z'), "{backup:?}");
    assert_eq!(backup.before_schema_version, Some(CURRENT_SCHEMA));
    assert!(backup.size_bytes > 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&backup.path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }
    assert_eq!(schema_of(&backup.path), before);
    // Verified as a data directory of its own, as it is after a rollback.
    let copy = TempDir::new().unwrap();
    std::fs::copy(&backup.path, copy.path().join(DB_FILE_NAME)).unwrap();
    let report = verify_log(copy.path()).unwrap();
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(report.boards[0].verified_events, events);
    backup.path.clone()
}

/// The rollback the server prints: move the backup into place, remove the
/// side files. The old version's offline tools then read it as before.
fn roll_back(data_dir: &Path, backup: &Path) {
    std::fs::rename(backup, data_dir.join(DB_FILE_NAME)).unwrap();
    for side in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(data_dir.join(format!("{DB_FILE_NAME}{side}")));
    }
}

#[tokio::test]
async fn a_version_2_directory_is_backed_up_before_it_is_migrated() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = TempDir::new().unwrap();
    let data_dir = data_dir_of(&dir);
    let (alice, message) = write_m1_directory(&data_dir);

    // The check says what will happen, and changes nothing.
    let check = operator::upgrade_check(&data_dir).unwrap();
    assert_eq!(check.schema_version, 2);
    assert_eq!(
        check.action,
        operator::UpgradeAction::Migrate {
            from: 2,
            to: CURRENT_SCHEMA
        }
    );
    assert_eq!(
        check.backup_name.as_deref(),
        Some(format!("openboard.db.pre-v{CURRENT_SCHEMA}-<UTC time>").as_str())
    );
    assert_eq!(check.backup_dir, data_dir);
    assert!(check.database_bytes > 0);
    assert_eq!(check.room_for_backup, Some(true));
    assert_eq!(check.last_started_by, None);
    assert!(operator::list_backups(&data_dir).unwrap().is_empty());
    assert_eq!(schema_of(&data_dir.join(DB_FILE_NAME)), 2);

    start_once(&data_dir).await.unwrap();
    assert_eq!(schema_of(&data_dir.join(DB_FILE_NAME)), CURRENT_SCHEMA);
    let backup = check_backup(&data_dir, 2, 1);
    // The backup holds the board as it was.
    let text: String = rusqlite::Connection::open(&backup)
        .unwrap()
        .query_row(
            "SELECT text FROM messages WHERE id = ?1",
            [message],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(text, "an M1 thread, cc @alice");
    // The start was recorded, and the next one needs no migration and
    // writes no backup.
    let check = operator::upgrade_check(&data_dir).unwrap();
    assert_eq!(check.action, operator::UpgradeAction::None);
    assert_eq!(check.backup_name, None);
    let by = check.last_started_by.unwrap();
    assert_eq!(by.version, openboard_api::build::VERSION);
    assert_eq!(by.commit.as_deref(), openboard_api::build::commit());
    start_once(&data_dir).await.unwrap();
    assert_eq!(operator::list_backups(&data_dir).unwrap().len(), 1);

    // Rolled back, the directory is the version 2 one again: the old
    // version's tools read it, and the credential still works there.
    roll_back(&data_dir, &backup);
    assert_eq!(schema_of(&data_dir.join(DB_FILE_NAME)), 2);
    assert_eq!(verified(&data_dir), 1);
    assert_eq!(operator::list_credentials(&data_dir).unwrap().len(), 1);
    // Upgraded again, it is backed up again, and the board works.
    let board = Net::start_in(dir, |_| {}).await;
    assert_eq!(board.status(&alice).await, StatusCode::OK);
    assert_eq!(operator::list_backups(&board.data_dir()).unwrap().len(), 1);
    let pruned = operator::prune_backups(&board.data_dir()).unwrap();
    assert_eq!(pruned.len(), 1);
    assert!(!pruned[0].path.exists());
    assert!(operator::list_backups(&board.data_dir())
        .unwrap()
        .is_empty());
    assert_eq!(board.read(&alice, message).await.root.id, message);
    board.stop().await;
}

#[tokio::test]
async fn a_version_1_directory_is_backed_up_before_it_is_migrated() {
    let dir = TempDir::new().unwrap();
    let data_dir = data_dir_of(&dir);
    write_m0_directory(&data_dir);
    let check = operator::upgrade_check(&data_dir).unwrap();
    assert_eq!(
        check.action,
        operator::UpgradeAction::Migrate {
            from: 1,
            to: CURRENT_SCHEMA
        }
    );
    start_once(&data_dir).await.unwrap();
    assert_eq!(schema_of(&data_dir.join(DB_FILE_NAME)), CURRENT_SCHEMA);
    assert_eq!(verified(&data_dir), 2);
    let backup = check_backup(&data_dir, 1, 2);
    roll_back(&data_dir, &backup);
    assert_eq!(schema_of(&data_dir.join(DB_FILE_NAME)), 1);
    assert_eq!(verified(&data_dir), 2);
    assert_eq!(operator::board_info(&data_dir).unwrap().mode, Mode::Local);
}

#[cfg(unix)]
#[tokio::test]
async fn a_read_only_data_directory_refuses_to_migrate_and_is_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let data_dir = data_dir_of(&dir);
    write_m1_directory(&data_dir);
    // A connection held open keeps the write-ahead log's side files, so
    // that the database can still be read once the directory cannot be
    // written; and the lock file is there already.
    let holder = open_db(&data_dir);
    holder
        .query_row("SELECT count(*) FROM messages", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    std::fs::write(data_dir.join(openboard_server::LOCK_FILE_NAME), b"").unwrap();
    let before = std::fs::read(data_dir.join(DB_FILE_NAME)).unwrap();
    let listing = |dir: &Path| {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    };
    let files = listing(&data_dir);
    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    // Someone who can write anyway (root) cannot run this test.
    let probe = data_dir.join("probe");
    if std::fs::write(&probe, b"").is_ok() {
        let _ = std::fs::remove_file(&probe);
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("skipped: the directory is writable whatever its mode");
        return;
    }

    let error = start_once(&data_dir).await.unwrap_err().to_string();
    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    for needle in [
        "schema version 2",
        &format!("migrated to version {CURRENT_SCHEMA}"),
        "backup",
        "Nothing was changed",
        "upgrade-check",
    ] {
        assert!(error.contains(needle), "{needle}: {error}");
    }
    drop(holder);
    assert_eq!(schema_of(&data_dir.join(DB_FILE_NAME)), 2);
    assert_eq!(std::fs::read(data_dir.join(DB_FILE_NAME)).unwrap(), before);
    assert_eq!(listing(&data_dir), files);
    assert!(operator::list_backups(&data_dir).unwrap().is_empty());
}

#[tokio::test]
async fn a_newer_database_is_refused_with_who_wrote_it_and_what_to_do() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let board = Net::start().await;
    let dir = board.stop().await;
    let data_dir = data_dir_of(&dir);
    // As a later version would leave it.
    {
        let conn = open_db(&data_dir);
        conn.execute_batch(
            "UPDATE meta SET value = '9.9.9' WHERE key = 'last_started_version';
             UPDATE meta SET value = 'abcdef012345' WHERE key = 'last_started_commit';
             PRAGMA user_version = 99;",
        )
        .unwrap();
    }
    let error = start_once(&data_dir).await.unwrap_err().to_string();
    for needle in [
        "schema version 99",
        "newer than this version of Openboard",
        "last started by Openboard 9.9.9 (abcdef012345)",
        "install Openboard 9.9.9 or newer",
        "restore a backup",
        "openboard admin backups",
    ] {
        assert!(error.contains(needle), "{needle}: {error}");
    }
    assert_eq!(schema_of(&data_dir.join(DB_FILE_NAME)), 99);
    // The check reads it and says the same; operator commands refuse it.
    let check = operator::upgrade_check(&data_dir).unwrap();
    assert_eq!(check.action, operator::UpgradeAction::Refuse);
    assert_eq!(check.last_started_by.unwrap().version, "9.9.9");
    let refused = operator::board_info(&data_dir).unwrap_err().to_string();
    assert!(refused.contains("9.9.9"), "{refused}");
}
