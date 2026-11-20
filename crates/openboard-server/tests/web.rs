//! End-to-end tests of people and browser sessions: person invites, the
//! sign-in exchange, cookie authentication on every route, the anti-CSRF
//! checks in both modes, and sessions as credentials. A real server on
//! port 0, reached over TLS by a client that pins the board's certificate
//! and sends the `Cookie` header itself, as a browser would.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use openboard_api::auth::{
    parse_fingerprint, CredentialInfo, JoinResponse, WhoAmI, MODE_LOCAL, MODE_NETWORK,
};
use openboard_api::web::{
    session_cookie_name, sign_in_link, Session, CSRF_HEADER, DEFAULT_SESSION_TTL_SECS,
    SESSION_COOKIE_PREFIX, WEB_CODE_PREFIX,
};
use openboard_api::{
    codes, ActorKind, BoardList, ErrorBody, EventsResponse, Message, PostMessageResponse, Thread,
};
use openboard_server::operator::{
    self, log_kinds, InviteOptions, Invited, OperatorError, PersonInvite,
};
use openboard_server::{Config, Limits, Mode, ServeError, DB_FILE_NAME};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// A client that pins the board's certificate
// ---------------------------------------------------------------------------

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

fn pinned_client(fingerprint: &str) -> Client {
    let digest = parse_fingerprint(fingerprint).expect("a well-formed fingerprint");
    let provider: Arc<CryptoProvider> = Arc::new(rustls::crypto::ring::default_provider());
    let pin = Pin {
        digest,
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

// ---------------------------------------------------------------------------
// A running board
// ---------------------------------------------------------------------------

struct Net {
    dir: TempDir,
    addr: SocketAddr,
    url: String,
    http: Client,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), ServeError>>,
}

/// A signed-in browser: what it holds after `POST /v1/web/login`.
#[derive(Debug, Clone)]
struct Browser {
    /// The cookie's name: the one of the board that set it.
    name: String,
    /// The cookie's value: the session's secret.
    secret: String,
    csrf: String,
    credential: CredentialInfo,
}

impl Browser {
    /// The `Cookie` header a browser sends.
    fn cookie(&self) -> String {
        format!("{}={}", self.name, self.secret)
    }
}

fn data_dir_of(dir: &TempDir) -> PathBuf {
    dir.path().join("data")
}

impl Net {
    async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    /// A network board with limits that stay out of the way.
    async fn start_with(tweak: impl FnOnce(&mut Config)) -> Self {
        Self::start_as(true, tweak).await
    }

    async fn start_as(network: bool, tweak: impl FnOnce(&mut Config)) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = TempDir::new().unwrap();
        let mut config = Config::new(data_dir_of(&dir));
        config.port = 0;
        config.network = network;
        // Every enrolled agent and person may use the board, as before
        // grants.
        config.open = true;
        config.limits = Limits {
            requests_per_minute: 100_000,
            joins_per_minute: 100_000,
            auth_failures_per_minute: 100_000,
            max_waiters: 100,
            credential_recheck: Duration::from_millis(100),
            ..Limits::default()
        };
        tweak(&mut config);
        let server = openboard_server::bind(config).await.unwrap();
        let addr = server.local_addr();
        let url = server.url().to_owned();
        let http = match server.fingerprint() {
            Some(fingerprint) => {
                assert_eq!(server.mode(), Mode::Network);
                pinned_client(fingerprint)
            }
            None => Client::builder().no_proxy().build().unwrap(),
        };
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        Self {
            dir,
            addr,
            url,
            http,
            stop: Some(stop),
            task,
        }
    }

    fn data_dir(&self) -> PathBuf {
        data_dir_of(&self.dir)
    }

    /// The scheme and authority requests are sent to: what a browser on one
    /// of the board's own pages sends as `Origin`.
    fn origin(&self) -> String {
        self.url.clone()
    }

    fn at(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    /// The name of this board's session cookie: the one for its port.
    fn cookie_name(&self) -> String {
        session_cookie_name(self.addr.port())
    }

    /// A `Cookie` header that holds this board's session cookie with
    /// `value`.
    fn cookie(&self, value: &str) -> String {
        format!("{}={value}", self.cookie_name())
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

    /// Invites a person with the default options.
    fn person(&self, email: &str) -> PersonInvite {
        match operator::invite(&self.data_dir(), email, &InviteOptions::default()).unwrap() {
            Invited::Person(invite) => invite,
            Invited::Agent(_) => panic!("{email} was invited as an agent"),
        }
    }

    async fn login_raw(&self, code: &str) -> Response {
        self.http
            .post(self.at("/v1/web/login"))
            .json(&json!({ "code": code }))
            .send()
            .await
            .unwrap()
    }

    async fn join_raw(&self, code: &str) -> Response {
        self.http
            .post(self.at("/v1/join"))
            .json(&json!({ "code": code }))
            .send()
            .await
            .unwrap()
    }

    /// Swaps a sign-in code for a session, as the sign-in page does.
    async fn login(&self, code: &str) -> Browser {
        let response = self.login_raw(code).await;
        self.signed_in(response).await
    }

    /// Signs in from a browser that already holds cookies for this host.
    async fn login_with_cookies(&self, code: &str, cookies: &str) -> Response {
        self.http
            .post(self.at("/v1/web/login"))
            .header("cookie", cookies)
            .json(&json!({ "code": code }))
            .send()
            .await
            .unwrap()
    }

    /// What the browser holds after a successful sign-in.
    async fn signed_in(&self, response: Response) -> Browser {
        assert_eq!(response.status(), StatusCode::OK);
        let secret = cookie_set_by(&response)
            .split(';')
            .next()
            .and_then(|pair| pair.strip_prefix(&self.cookie("")))
            .expect("the session cookie")
            .to_owned();
        let session: Session = response.json().await.unwrap();
        Browser {
            name: self.cookie_name(),
            secret,
            csrf: session.csrf_token.expect("a CSRF token"),
            credential: session.credential.expect("a credential"),
        }
    }

    /// Invites `email` and signs it in.
    async fn sign_in(&self, email: &str) -> Browser {
        self.login(&self.person(email).code).await
    }

    /// Invites the agent `name` and enrols it. Returns its token.
    async fn enrol(&self, name: &str) -> String {
        let created =
            operator::create_invite(&self.data_dir(), name, &InviteOptions::default()).unwrap();
        let response = self.join_raw(&created.invite.code).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<JoinResponse>().await.unwrap().token
    }

    /// A request with the browser's cookie and nothing else.
    fn cookie_only(&self, method: Method, path: &str, browser: &Browser) -> RequestBuilder {
        self.http
            .request(method, self.at(path))
            .header("cookie", browser.cookie())
    }

    /// A request as the page makes it: the cookie and the CSRF header.
    fn browser(&self, method: Method, path: &str, browser: &Browser) -> RequestBuilder {
        self.cookie_only(method, path, browser)
            .header(CSRF_HEADER, &browser.csrf)
    }

    async fn post_as(&self, browser: &Browser, body: Value) -> PostMessageResponse {
        let response = self
            .browser(Method::POST, MESSAGES, browser)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        response.json().await.unwrap()
    }

    async fn post_with_token(&self, token: &str, body: Value) -> PostMessageResponse {
        let response = self
            .http
            .post(self.at(MESSAGES))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        response.json().await.unwrap()
    }

    async fn status_of(&self, browser: &Browser) -> StatusCode {
        self.cookie_only(Method::GET, "/v1/whoami", browser)
            .send()
            .await
            .unwrap()
            .status()
    }

    /// How many events the board holds, as `reader` sees them.
    async fn event_count(&self, reader: &Browser) -> usize {
        let events: EventsResponse = self
            .cookie_only(Method::GET, "/v1/boards/general/events?after=start", reader)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        events.events.len()
    }
}

const MESSAGES: &str = "/v1/boards/general/messages";

/// The one `Set-Cookie` header of a response.
fn cookie_set_by(response: &Response) -> String {
    let mut cookies = response.headers().get_all("set-cookie").iter();
    let cookie = cookies.next().expect("a Set-Cookie header");
    assert!(cookies.next().is_none(), "more than one Set-Cookie header");
    cookie.to_str().unwrap().to_owned()
}

async fn error_of(response: Response, status: StatusCode, code: &str) -> ErrorBody {
    assert_eq!(response.status(), status);
    let body: ErrorBody = response.json().await.unwrap();
    assert_eq!(body.error.code, code);
    body
}

async fn csrf_failed(response: Response) {
    assert!(response.headers().get("set-cookie").is_none());
    error_of(response, StatusCode::FORBIDDEN, codes::CSRF_FAILED).await;
}

async fn expect_rate_limited(response: Response) {
    assert!(response.headers().get("retry-after").is_some());
    error_of(response, StatusCode::TOO_MANY_REQUESTS, codes::RATE_LIMITED).await;
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Fails when any of `secrets` is stored anywhere in the data directory or
/// returned by the security log.
fn assert_not_stored(data_dir: &Path, secrets: &[&str]) {
    let log = format!("{:?}", operator::security_log(data_dir, None).unwrap());
    let conn = rusqlite::Connection::open(data_dir.join(DB_FILE_NAME)).unwrap();
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    drop(conn);
    for entry in std::fs::read_dir(data_dir).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        for secret in secrets {
            assert!(secret.len() >= 32, "a secret this short proves nothing");
            assert!(
                !contains(&bytes, secret.as_bytes()),
                "a secret is stored in {}",
                path.display()
            );
            // Nor the part after its prefix, in case only that were kept.
            let bare = secret.rsplit('_').next().unwrap();
            assert!(!contains(&bytes, bare.as_bytes()), "{}", path.display());
            assert!(!log.contains(bare));
        }
    }
}

async fn drain(mut response: Response) -> String {
    let mut text = String::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    text
}

fn seconds_between(earlier: &str, later: &str) -> i64 {
    // Both are RFC 3339 in UTC with the same shape, so SQLite can subtract.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.query_row(
        "SELECT CAST(round((julianday(?2) - julianday(?1)) * 86400) AS INTEGER)",
        [earlier, later],
        |row| row.get(0),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Inviting people
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_operator_invites_lists_and_revokes_people() {
    let board = Net::start().await;
    let data_dir = board.data_dir();
    let defaults = InviteOptions::default();

    // A name with an `@` is a person, and the handle is the address
    // lowercased.
    let first = match operator::invite(&data_dir, "Ada@Example.COM", &defaults).unwrap() {
        Invited::Person(invite) => invite,
        Invited::Agent(_) => panic!("an email address invited an agent"),
    };
    assert_eq!(first.principal, "ada@example.com");
    assert!(first.new_principal);
    assert!(first.id.starts_with("inv_"));
    assert_eq!(first.url, board.url);
    assert_eq!(first.session_ttl_secs, DEFAULT_SESSION_TTL_SECS);
    let secret = first
        .code
        .strip_prefix(WEB_CODE_PREFIX)
        .expect("the prefix");
    assert_eq!(secret.len(), 64);
    assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(first.link, sign_in_link(&board.url, &first.code));
    assert_eq!(first.link, format!("{}/join#{}", board.url, first.code));

    // Any other name is an agent, exactly as before.
    let agent = match operator::invite(&data_dir, "builder", &defaults).unwrap() {
        Invited::Agent(invite) => invite,
        Invited::Person(_) => panic!("an agent's name invited a person"),
    };
    assert_eq!(agent.principal, "builder");
    assert_eq!(agent.invite.code.len(), 64);
    assert!(!agent.invite.code.starts_with(WEB_CODE_PREFIX));

    // The same address again, in any case, is the same person: one more
    // link, for a second browser or after the first expired.
    let second = operator::create_person_invite(&data_dir, "ADA@example.com", &defaults).unwrap();
    assert_eq!(second.principal, "ada@example.com");
    assert!(!second.new_principal);
    assert_ne!(second.code, first.code);
    assert_ne!(second.id, first.id);

    // Each function takes only its own kind of name, and addresses the
    // board could not mention are refused.
    for bad in [
        "ada@",
        "@example.com",
        "ada@@example.com",
        "ada @example.com",
        "ada@example.com\n",
        "ada@example.com.",
        "ada@exämple.com",
        "ada@example.com/reviewer",
        &format!("{}@example.com", "a".repeat(250)),
    ] {
        for result in [
            operator::invite(&data_dir, bad, &defaults).map(|_| ()),
            operator::create_person_invite(&data_dir, bad, &defaults).map(|_| ()),
        ] {
            assert!(
                matches!(result, Err(OperatorError::InvalidName(_))),
                "{bad:?}: {result:?}"
            );
        }
    }
    assert!(matches!(
        operator::create_invite(&data_dir, "ada@example.com", &defaults),
        Err(OperatorError::InvalidName(_))
    ));
    assert!(matches!(
        operator::create_person_invite(&data_dir, "builder", &defaults),
        Err(OperatorError::InvalidName(_))
    ));
    // The options apply to a person's link as to an agent's invite.
    assert!(matches!(
        operator::create_person_invite(
            &data_dir,
            "ada@example.com",
            &InviteOptions {
                url: Some("http://plain.example".to_owned()),
                ..InviteOptions::default()
            }
        ),
        Err(OperatorError::InvalidUrl(_))
    ));
    let elsewhere = operator::create_person_invite(
        &data_dir,
        "grace@example.com",
        &InviteOptions {
            url: Some("https://board.example:8443/".to_owned()),
            token_ttl_secs: Some(600),
            ..InviteOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        elsewhere.link,
        format!("https://board.example:8443/join#{}", elsewhere.code)
    );
    assert_eq!(elsewhere.session_ttl_secs, 600);

    // The listings tell people from agents.
    let handles = |principals: Vec<operator::PrincipalInfo>| -> Vec<String> {
        principals.into_iter().map(|p| p.handle).collect()
    };
    let people = operator::list_people(&data_dir).unwrap();
    assert!(people.iter().all(|p| p.kind == ActorKind::Person));
    assert_eq!(people[0].pending_invites, 2);
    assert_eq!(people[0].active_credentials, 0);
    assert_eq!(handles(people), ["ada@example.com", "grace@example.com"]);
    let agents = operator::list_agents(&data_dir).unwrap();
    assert!(agents.iter().all(|p| p.kind == ActorKind::Agent));
    assert_eq!(handles(agents), ["builder"]);
    assert_eq!(
        handles(operator::list_principals(&data_dir).unwrap()),
        ["ada@example.com", "builder", "grace@example.com"]
    );
    let pending: Vec<(String, ActorKind)> = operator::list_invites(&data_dir)
        .unwrap()
        .into_iter()
        .map(|invite| (invite.principal, invite.kind))
        .collect();
    assert_eq!(
        pending,
        [
            ("ada@example.com".to_owned(), ActorKind::Person),
            ("builder".to_owned(), ActorKind::Agent),
            ("ada@example.com".to_owned(), ActorKind::Person),
            ("grace@example.com".to_owned(), ActorKind::Person),
        ]
    );

    // Sessions are credentials: listed, with the lifetime the invite set,
    // and revoked by ID or with their person.
    let laptop = board.login(&first.code).await;
    let phone = board.login(&second.code).await;
    let grace = board.login(&elsewhere.code).await;
    let lifetime = |c: &CredentialInfo| seconds_between(&c.created_at, &c.expires_at);
    assert_eq!(
        lifetime(&laptop.credential),
        DEFAULT_SESSION_TTL_SECS as i64
    );
    assert_eq!(lifetime(&grace.credential), 600);
    let listed = operator::list_credentials(&data_dir).unwrap();
    assert_eq!(listed.len(), 3);
    assert!(listed.iter().all(|c| c.kind == ActorKind::Person));
    assert!(listed.iter().all(|c| c.revoked_at.is_none()));
    // Two sessions made in the same millisecond list in either order.
    let first_listed = listed
        .iter()
        .find(|c| c.id == laptop.credential.id)
        .unwrap();
    assert_eq!(first_listed.principal, "ada@example.com");
    assert_eq!(
        operator::list_people(&data_dir).unwrap()[0].active_credentials,
        2
    );

    let revoked = operator::revoke_credential(&data_dir, &laptop.credential.id).unwrap();
    assert_eq!(revoked.principal, "ada@example.com");
    assert_eq!(
        revoked.credentials,
        std::slice::from_ref(&laptop.credential.id)
    );
    assert_eq!(board.status_of(&laptop).await, StatusCode::UNAUTHORIZED);
    assert_eq!(board.status_of(&phone).await, StatusCode::OK);

    // By name, in any case: every session, and every link not yet used.
    let spare = board.person("ada@example.com");
    let revoked = operator::revoke_principal(&data_dir, "Ada@Example.com").unwrap();
    assert_eq!(revoked.principal, "ada@example.com");
    assert_eq!(
        revoked.credentials,
        std::slice::from_ref(&phone.credential.id)
    );
    assert_eq!(revoked.invites, std::slice::from_ref(&spare.id));
    assert_eq!(board.status_of(&phone).await, StatusCode::UNAUTHORIZED);
    assert_eq!(board.status_of(&grace).await, StatusCode::OK);
    let response = board.login_raw(&spare.code).await;
    error_of(response, StatusCode::FORBIDDEN, codes::INVITE_INVALID).await;
    assert!(matches!(
        operator::revoke_principal(&data_dir, "nobody@example.com"),
        Err(OperatorError::NoSuchPrincipal(_))
    ));
    // The person keeps the handle and can be invited again.
    let back = board.person("ada@example.com");
    assert!(!back.new_principal);
    assert_eq!(
        board.status_of(&board.login(&back.code).await).await,
        StatusCode::OK
    );

    // The log names the person and the session, never a secret.
    let log = operator::security_log(&data_dir, None).unwrap();
    let ada = Some("ada@example.com");
    let entry = |kind: &str| log.iter().find(|e| e.kind == kind).unwrap();
    let invited = entry(log_kinds::INVITE_CREATED);
    assert_eq!(invited.principal.as_deref(), ada);
    assert_eq!(invited.detail.as_deref(), Some(first.id.as_str()));
    let signed_in = entry(log_kinds::WEB_LOGIN_SUCCEEDED);
    assert_eq!(signed_in.principal.as_deref(), ada);
    assert_eq!(
        signed_in.credential_id.as_deref(),
        Some(laptop.credential.id.as_str())
    );
    assert_eq!(signed_in.client.as_deref(), Some("127.0.0.0"));
    let session_revoked = entry(log_kinds::CREDENTIAL_REVOKED);
    assert_eq!(session_revoked.principal.as_deref(), ada);
    assert_eq!(session_revoked.detail.as_deref(), Some("by operator"));
    assert_eq!(
        session_revoked.credential_id.as_deref(),
        Some(laptop.credential.id.as_str())
    );
    assert!(log.iter().any(|e| e.kind == log_kinds::WEB_LOGIN_FAILED
        && e.detail.as_deref() == Some("unusable code")
        && e.principal.is_none()));
    assert!(log.iter().any(|e| e.kind == log_kinds::AUTH_FAILED
        && e.detail.as_deref() == Some("revoked session")
        && e.principal.as_deref() == ada));

    let dir = board.stop().await;
    assert_not_stored(
        &data_dir_of(&dir),
        &[
            &first.code,
            &second.code,
            &spare.code,
            &laptop.secret,
            &phone.secret,
            &grace.secret,
            &laptop.csrf,
        ],
    );

    // A local board has no people: it needs no sign-in.
    let local = Net::start_as(false, |_| {}).await;
    assert!(matches!(
        operator::invite(&local.data_dir(), "ada@example.com", &defaults),
        Err(OperatorError::NotNetwork)
    ));
    assert!(operator::list_people(&local.data_dir()).unwrap().is_empty());
    local.stop().await;
}

// ---------------------------------------------------------------------------
// Signing in
// ---------------------------------------------------------------------------

#[tokio::test]
async fn signing_in_swaps_the_code_for_a_session_cookie_exactly_once() {
    let board = Net::start().await;
    let invite = board.person("ada@example.com");

    // The page needs no credential, and a preview of the link spends
    // nothing: the code is in the fragment, which is never sent.
    let page = board.http.get(board.at("/join")).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);

    let response = board.login_raw(&invite.code).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    let cookie = cookie_set_by(&response);
    let body = response.text().await.unwrap();

    // The cookie, attribute by attribute.
    let mut parts = cookie.split("; ");
    let (name, secret) = parts.next().unwrap().split_once('=').unwrap();
    assert_eq!(
        name,
        format!("__Host-openboard_session_{}", board.addr.port())
    );
    assert_eq!(name, session_cookie_name(board.addr.port()));
    assert!(name.starts_with(SESSION_COOKIE_PREFIX));
    let attributes: Vec<&str> = parts.collect();
    assert_eq!(
        attributes,
        [
            "Secure",
            "HttpOnly",
            "SameSite=Strict",
            "Path=/",
            &format!("Max-Age={DEFAULT_SESSION_TTL_SECS}")[..],
        ]
    );
    assert!(!cookie.to_ascii_lowercase().contains("domain"));
    // A fresh random secret, of a shape no bearer token has.
    let random = secret.strip_prefix("obs_").expect("the session prefix");
    assert_eq!(random.len(), 64);
    assert!(random.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(!secret.contains(&invite.code[WEB_CODE_PREFIX.len()..]));

    // The body describes the session and holds no part of the secret.
    assert!(!body.contains(secret));
    assert!(!body.contains(random));
    assert!(!body.contains(&invite.code));
    let session: Session = serde_json::from_str(&body).unwrap();
    assert_eq!(session.mode, MODE_NETWORK);
    assert_eq!(session.actor.kind, ActorKind::Person);
    assert_eq!(session.actor.handle, "ada@example.com");
    assert_eq!(session.actor.profile, None);
    let csrf = session.csrf_token.clone().expect("a CSRF token");
    assert_eq!(csrf.len(), 64);
    assert_ne!(csrf, random);
    let credential = session.credential.clone().expect("a credential");
    assert_eq!(credential.principal, "ada@example.com");
    assert_eq!(credential.kind, ActorKind::Person);
    assert!(credential.id.starts_with("crd_"));
    assert!(credential.revoked_at.is_none());
    assert_eq!(
        seconds_between(&credential.created_at, &credential.expires_at),
        DEFAULT_SESSION_TTL_SECS as i64
    );
    let browser = Browser {
        name: board.cookie_name(),
        secret: secret.to_owned(),
        csrf,
        credential,
    };

    // `GET /v1/web/session` describes the same session to the page, on
    // every load, among whatever other cookies the browser holds.
    let described = board
        .http
        .get(board.at("/v1/web/session"))
        .header("cookie", "theme=dark; other=a=b")
        .header("cookie", format!("x=y;  {} ; z", browser.cookie()))
        .send()
        .await
        .unwrap();
    assert_eq!(described.status(), StatusCode::OK);
    assert!(described.headers().get("set-cookie").is_none());
    let text = described.text().await.unwrap();
    assert!(!text.contains(secret));
    let described: Session = serde_json::from_str(&text).unwrap();
    assert_eq!(described.actor, session.actor);
    assert_eq!(described.mode, MODE_NETWORK);
    assert_eq!(described.csrf_token, session.csrf_token);
    assert_eq!(
        described.credential.as_ref().map(|c| &c.id),
        Some(&browser.credential.id)
    );
    // `whoami` agrees, and the declared profile only picks a label.
    let me: WhoAmI = board
        .cookie_only(Method::GET, "/v1/whoami", &browser)
        .header("x-board-profile", "desk")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.actor.handle, "ada@example.com/desk");
    assert_eq!(me.actor.kind, ActorKind::Person);

    // Without a session the page gets the usual 401, in every case the
    // same one, which is how it knows to show the sign-in screen.
    let no_credential = board
        .http
        .get(board.at("/v1/web/session"))
        .send()
        .await
        .unwrap();
    assert_eq!(no_credential.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(no_credential.headers()["www-authenticate"], "Bearer");
    let refusal = no_credential.bytes().await.unwrap();
    let unknown = format!("obs_{}", "ab".repeat(32));
    let token_shaped = format!("obt_{}", "ab".repeat(32));
    let huge = format!(
        "{}{}",
        "a=b; ".repeat(1500),
        browser.cookie().to_uppercase()
    );
    for cookie in [
        board.cookie(&unknown),
        board.cookie(&token_shaped),
        board.cookie(""),
        board.cookie(&format!("\"{secret}\"")),
        board.cookie(&format!("{secret}x")),
        board.cookie(&secret.to_uppercase()),
        // The first cookie of that name counts.
        format!("{}; {}", board.cookie("nope"), browser.cookie()),
        // The name without a port, and with another board's port, are
        // not this board's cookie.
        format!("__Host-openboard_session={secret}"),
        format!("{SESSION_COOKIE_PREFIX}={secret}"),
        format!("{SESSION_COOKIE_PREFIX}1={secret}"),
        format!("{}0={secret}", board.cookie_name()),
        format!("openboard_session={secret}"),
        format!("__Secure-openboard_session={secret}"),
        "unrelated=1".to_owned(),
        String::new(),
        huge,
    ] {
        let response = board
            .http
            .get(board.at("/v1/web/session"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{cookie:.60}");
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        assert!(response.headers().get("set-cookie").is_none());
        assert_eq!(response.bytes().await.unwrap(), refusal, "{cookie:.60}");
    }
    // The session secret is no bearer token.
    let response = board
        .http
        .get(board.at("/v1/web/session"))
        .bearer_auth(secret)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.bytes().await.unwrap(), refusal);

    // A spent code, a wrong one, an expired one, a withdrawn one, an
    // agent's code and junk are indistinguishable, and set no cookie.
    let expired = operator::create_person_invite(
        &board.data_dir(),
        "late@example.com",
        &InviteOptions {
            ttl_secs: 0,
            ..InviteOptions::default()
        },
    )
    .unwrap();
    let withdrawn = board.person("gone@example.com");
    operator::revoke_principal(&board.data_dir(), "gone@example.com").unwrap();
    let agent =
        operator::create_invite(&board.data_dir(), "builder", &InviteOptions::default()).unwrap();
    let mut bodies = Vec::new();
    for code in [
        invite.code.as_str(),
        &format!("{WEB_CODE_PREFIX}{}", "0".repeat(64)),
        &expired.code,
        &withdrawn.code,
        &agent.invite.code,
        &format!("{WEB_CODE_PREFIX}{}", agent.invite.code),
        &invite.code.to_uppercase(),
        &format!(" {}", invite.code),
        WEB_CODE_PREFIX,
        "not-a-code",
        "",
    ] {
        let response = board.login_raw(code).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{code}");
        assert!(response.headers().get("set-cookie").is_none(), "{code}");
        bodies.push(response.bytes().await.unwrap());
    }
    let parsed: ErrorBody = serde_json::from_slice(&bodies[0]).unwrap();
    assert_eq!(parsed.error.code, codes::INVITE_INVALID);
    assert!(bodies.iter().all(|body| *body == bodies[0]));
    // The same answer `POST /v1/join` gives.
    let at_join = board.join_raw("not-a-code").await.bytes().await.unwrap();
    assert_eq!(at_join, bodies[0]);

    // A malformed request is a client error, and only `POST` signs in.
    let response = board
        .http
        .post(board.at("/v1/web/login"))
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    error_of(response, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;
    let response = board
        .http
        .post(board.at("/v1/web/login"))
        .body(format!("code={}", invite.code))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let response = board
        .http
        .get(board.at("/v1/web/login"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // One session exists, and it still works.
    let credentials = operator::list_credentials(&board.data_dir()).unwrap();
    assert_eq!(credentials.len(), 1);
    assert_eq!(credentials[0].id, browser.credential.id);
    assert_eq!(board.status_of(&browser).await, StatusCode::OK);

    let dir = board.stop().await;
    assert_not_stored(
        &data_dir_of(&dir),
        &[&invite.code, secret, &browser.csrf, &expired.code],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_logins_with_one_code_yield_exactly_one_session() {
    let board = Arc::new(Net::start().await);
    let invite = board.person("ada@example.com");
    let attempts: Vec<_> = (0..16)
        .map(|_| {
            let (board, code) = (Arc::clone(&board), invite.code.clone());
            tokio::spawn(async move { board.login_raw(&code).await.status() })
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
    assert_eq!((won, lost), (1, 15), "{statuses:?}");
    assert_eq!(
        operator::list_credentials(&board.data_dir()).unwrap().len(),
        1
    );
    Arc::into_inner(board).unwrap().stop().await;
}

#[tokio::test]
async fn a_code_is_not_spent_at_the_wrong_endpoint() {
    let board = Net::start().await;
    let person = board.person("ada@example.com");
    let agent =
        operator::create_invite(&board.data_dir(), "builder", &InviteOptions::default()).unwrap();
    let agent_code = agent.invite.code.as_str();
    let pending = || operator::list_invites(&board.data_dir()).unwrap().len();

    // A web code at the agents' endpoint, whole and without its prefix.
    let bare = &person.code[WEB_CODE_PREFIX.len()..];
    for code in [person.code.as_str(), bare] {
        let response = board.join_raw(code).await;
        error_of(response, StatusCode::FORBIDDEN, codes::INVITE_INVALID).await;
    }
    // An agent's code at the people's endpoint, bare and dressed as a web
    // code.
    for code in [
        agent_code.to_owned(),
        format!("{WEB_CODE_PREFIX}{agent_code}"),
    ] {
        let response = board.login_raw(&code).await;
        assert!(response.headers().get("set-cookie").is_none());
        error_of(response, StatusCode::FORBIDDEN, codes::INVITE_INVALID).await;
    }
    assert_eq!(pending(), 2);
    assert!(operator::list_credentials(&board.data_dir())
        .unwrap()
        .is_empty());

    // Both still work where they belong, once.
    let browser = board.login(&person.code).await;
    assert_eq!(browser.credential.kind, ActorKind::Person);
    let response = board.join_raw(agent_code).await;
    assert_eq!(response.status(), StatusCode::OK);
    let joined: JoinResponse = response.json().await.unwrap();
    assert_eq!(joined.credential.kind, ActorKind::Agent);
    assert_eq!(joined.credential.principal, "builder");
    assert!(joined.token.starts_with("obt_"));
    assert_eq!(pending(), 0);

    // Nor does a credential work in the other one's place.
    let as_cookie = board
        .http
        .get(board.at("/v1/whoami"))
        .header("cookie", board.cookie(&joined.token))
        .send()
        .await
        .unwrap();
    assert_eq!(as_cookie.status(), StatusCode::UNAUTHORIZED);
    let as_bearer = board
        .http
        .get(board.at("/v1/whoami"))
        .bearer_auth(&browser.secret)
        .send()
        .await
        .unwrap();
    assert_eq!(as_bearer.status(), StatusCode::UNAUTHORIZED);
    board.stop().await;
}

// ---------------------------------------------------------------------------
// Writing with a session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cookie_write_needs_the_sessions_csrf_token() {
    let board = Net::start().await;
    let ada = board.sign_in("ada@example.com").await;
    let grace = board.sign_in("grace@example.com").await;
    // A second session of the same person has its own token.
    let ada_phone = board.sign_in("ada@example.com").await;
    assert_ne!(ada.csrf, grace.csrf);
    assert_ne!(ada.csrf, ada_phone.csrf);

    // With the cookie and the header, the person is the author.
    let posted = board
        .post_as(
            &ada,
            json!({"channel": "dev", "text": "hello from a browser"}),
        )
        .await;
    assert_eq!(posted.message.by.kind, ActorKind::Person);
    assert_eq!(posted.message.by.handle, "ada@example.com");
    assert_eq!(
        posted.message.credential_id.as_deref(),
        Some(ada.credential.id.as_str())
    );
    let message = format!("{MESSAGES}/{}", posted.message.id);
    let reply = json!({"thread_id": posted.message.id, "text": "must not arrive"});
    assert_eq!(board.event_count(&ada).await, 1);

    // Without the header, with a wrong one, with another session's, with
    // the cookie value itself, and with the header sent twice: refused.
    let wrong_tokens: [Option<String>; 8] = [
        None,
        Some(String::new()),
        Some("0".repeat(64)),
        Some(grace.csrf.clone()),
        Some(ada_phone.csrf.clone()),
        Some(ada.secret.clone()),
        Some(ada.csrf.to_uppercase()),
        Some(format!("{} ", &ada.csrf[..63])),
    ];
    for token in &wrong_tokens {
        let mut request = board.cookie_only(Method::POST, MESSAGES, &ada).json(&reply);
        if let Some(token) = token {
            request = request.header(CSRF_HEADER, token);
        }
        csrf_failed(request.send().await.unwrap()).await;
    }

    // Every kind of write is covered, to any path, before routing.
    let labels = format!("{message}/labels");
    let reaction = format!("{message}/reactions/thumbsup");
    for (method, path, body) in [
        (Method::POST, MESSAGES, Some(reply.clone())),
        (Method::PATCH, &message[..], Some(json!({"text": "edited"}))),
        (Method::DELETE, &message[..], None),
        (
            Method::POST,
            &labels[..],
            Some(json!({"set": {"status": "open"}})),
        ),
        (Method::PUT, &reaction[..], None),
        (Method::DELETE, &reaction[..], None),
        (Method::POST, "/v1/logout", None),
        (Method::POST, "/v1/nowhere", None),
        (Method::OPTIONS, MESSAGES, None),
    ] {
        let mut request = board.cookie_only(method.clone(), path, &ada);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
        csrf_failed(response).await;
    }

    // Nothing changed: one event, the message as it was, the session live.
    assert_eq!(board.event_count(&ada).await, 1);
    let thread: Thread = board
        .cookie_only(
            Method::GET,
            &format!("/v1/boards/general/threads/{}", posted.message.id),
            &ada,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(thread.root.text, "hello from a browser");
    assert_eq!(thread.root.revision, 1);
    assert!(thread.root.labels.is_empty() && thread.root.reactions.is_empty());
    assert!(thread.replies.is_empty());
    assert_eq!(board.status_of(&ada).await, StatusCode::OK);

    // Reads need no header, the stream and the long-poll included.
    for path in [
        "/v1/boards",
        "/v1/boards/general/channels",
        "/v1/boards/general/threads?channel=dev",
        "/v1/boards/general/events?after=start&wait=1",
        "/v1/web/session",
    ] {
        let response = board
            .cookie_only(Method::GET, path, &ada)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    let stream = board
        .cookie_only(Method::GET, "/v1/boards/general/events/stream", &ada)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    drop(stream);

    // With the header every write works, and each session uses its own.
    let edited: Message = board
        .browser(Method::PATCH, &message, &ada)
        .json(&json!({"text": "edited in the browser"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(edited.revision, 2);
    let labelled = board
        .browser(Method::POST, &labels, &grace)
        .json(&json!({"set": {"status": "open"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(labelled.status(), StatusCode::OK);
    let reacted: Message = board
        .browser(Method::PUT, &reaction, &ada_phone)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reacted.reactions.len(), 1);
    // The author rule holds for people: grace cannot edit ada's message,
    // and ada can from her other browser.
    let response = board
        .browser(Method::DELETE, &message, &grace)
        .send()
        .await
        .unwrap();
    error_of(response, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    let deleted = board
        .browser(Method::DELETE, &message, &ada_phone)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
    board.stop().await;
}

#[tokio::test]
async fn a_write_from_another_origin_is_refused_on_a_network_board() {
    let board = Net::start().await;
    let ada = board.sign_in("ada@example.com").await;
    let token = board.enrol("builder").await;
    let own = board.origin();
    assert!(own.starts_with("https://127.0.0.1:"));
    let port = board.addr.port();
    let text = |text: &str| json!({"channel": "dev", "text": text});

    let foreign = [
        "https://evil.example".to_owned(),
        "null".to_owned(),
        // Another scheme, port or name for the same machine is another
        // origin.
        format!("http://127.0.0.1:{port}"),
        format!("https://127.0.0.1:{}", port.wrapping_add(1).max(1)),
        "https://127.0.0.1".to_owned(),
        format!("https://localhost:{port}"),
        format!("{own}.evil.example"),
        format!("{own}/"),
    ];
    for origin in &foreign {
        // With a session and its token, and with a bearer token.
        let response = board
            .browser(Method::POST, MESSAGES, &ada)
            .header("origin", origin)
            .json(&text("cross-site, with a session"))
            .send()
            .await
            .unwrap();
        csrf_failed(response).await;
        let response = board
            .http
            .post(board.at(MESSAGES))
            .bearer_auth(&token)
            .header("origin", origin)
            .json(&text("cross-site, with a token"))
            .send()
            .await
            .unwrap();
        csrf_failed(response).await;
    }
    assert_eq!(board.event_count(&ada).await, 0);
    let boards: BoardList = board
        .cookie_only(Method::GET, "/v1/boards", &ada)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Only the board every server starts with, and nothing on it.
    assert_eq!(boards.boards.len(), 1);
    assert_eq!(boards.boards[0].last_activity_at, None);

    // The endpoints that take no credential are covered by the same check,
    // and a refused attempt spends no code.
    let person = board.person("grace@example.com");
    let agent =
        operator::create_invite(&board.data_dir(), "helper", &InviteOptions::default()).unwrap();
    for (path, code) in [
        ("/v1/web/login", person.code.as_str()),
        ("/v1/join", agent.invite.code.as_str()),
    ] {
        let response = board
            .http
            .post(board.at(path))
            .header("origin", "https://evil.example")
            .json(&json!({ "code": code }))
            .send()
            .await
            .unwrap();
        csrf_failed(response).await;
    }
    assert_eq!(operator::list_invites(&board.data_dir()).unwrap().len(), 2);
    // A caller without a credential still learns nothing but the 401.
    let response = board
        .http
        .post(board.at(MESSAGES))
        .header("origin", "https://evil.example")
        .json(&text("anonymous"))
        .send()
        .await
        .unwrap();
    error_of(response, StatusCode::UNAUTHORIZED, codes::UNAUTHENTICATED).await;

    // The board's own origin, in any case, and no origin at all are fine.
    for origin in [Some(own.clone()), Some(own.to_uppercase()), None] {
        let mut by_cookie = board
            .browser(Method::POST, MESSAGES, &ada)
            .json(&text("from the page"));
        let mut by_token = board
            .http
            .post(board.at(MESSAGES))
            .bearer_auth(&token)
            .json(&text("from an agent"));
        if let Some(origin) = &origin {
            by_cookie = by_cookie.header("origin", origin);
            by_token = by_token.header("origin", origin);
        }
        assert_eq!(
            by_cookie.send().await.unwrap().status(),
            StatusCode::CREATED
        );
        assert_eq!(by_token.send().await.unwrap().status(), StatusCode::CREATED);
    }
    assert_eq!(board.event_count(&ada).await, 6);
    // Signing in from the board's own page sends its origin.
    let response = board
        .http
        .post(board.at("/v1/web/login"))
        .header("origin", &own)
        .json(&json!({ "code": person.code }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // A read is not a write: its Origin is not looked at.
    let response = board
        .cookie_only(Method::GET, "/v1/boards", &ada)
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    board.stop().await;
}

#[tokio::test]
async fn a_local_board_refuses_foreign_origins_and_describes_its_session() {
    let board = Net::start_as(false, |_| {}).await;
    let port = board.addr.port();
    assert_eq!(board.url, format!("http://127.0.0.1:{port}"));
    let post = |origin: Option<String>| {
        let mut request = board
            .http
            .post(board.at(MESSAGES))
            .json(&json!({"channel": "dev", "text": "hello"}));
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        request.send()
    };

    // No sign-in: the browser is the local principal, and there is no
    // session to protect with a token, so none is issued.
    let response = board
        .http
        .get(board.at("/v1/web/session"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("set-cookie").is_none());
    let text = response.text().await.unwrap();
    let raw: Value = serde_json::from_str(&text).unwrap();
    assert!(raw.get("csrf_token").is_none() && raw.get("credential").is_none());
    let session: Session = serde_json::from_str(&text).unwrap();
    assert_eq!(session.mode, MODE_LOCAL);
    assert_eq!(session.actor.kind, ActorKind::Local);
    assert_eq!(session.actor.handle, "local");
    assert_eq!(session.csrf_token, None);
    assert_eq!(session.credential, None);
    // A cookie changes nothing here.
    let response = board
        .http
        .get(board.at("/v1/web/session"))
        .header("cookie", board.cookie(&format!("obs_{}", "ab".repeat(32))))
        .header("x-board-profile", "desk")
        .send()
        .await
        .unwrap();
    let session: Session = response.json().await.unwrap();
    assert_eq!(session.actor.handle, "local/desk");
    assert_eq!(session.csrf_token, None);

    // A page of another site, or of another port on this machine, cannot
    // write, although the board asks for no credential.
    for origin in [
        "http://evil.example".to_owned(),
        "https://evil.example".to_owned(),
        "null".to_owned(),
        "http://127.0.0.1:3000".to_owned(),
        "http://127.0.0.1".to_owned(),
        format!("https://127.0.0.1:{port}"),
        // The request went to 127.0.0.1, so this is not where it was sent.
        format!("http://localhost:{port}"),
        format!("http://127.0.0.1:{port}.evil.example"),
    ] {
        csrf_failed(post(Some(origin)).await.unwrap()).await;
    }
    let boards: BoardList = board
        .http
        .get(board.at("/v1/boards"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Only the board every server starts with, and nothing on it.
    assert_eq!(boards.boards.len(), 1);
    assert_eq!(
        boards.boards[0].last_activity_at, None,
        "a refused write was stored"
    );

    // The board's own page, and a client that sends no Origin, can.
    for origin in [Some(board.origin()), None] {
        assert_eq!(post(origin).await.unwrap().status(), StatusCode::CREATED);
    }
    // By its other loopback name too, when that is where the request went.
    let response = board
        .http
        .post(format!("http://localhost:{port}{MESSAGES}"))
        .header("origin", format!("http://localhost:{port}"))
        .json(&json!({"channel": "dev", "text": "hello"}))
        .send()
        .await;
    if let Ok(response) = response {
        // `localhost` may resolve to an address the board is not on.
        assert_eq!(response.status(), StatusCode::CREATED);
    }
    // The same check covers every write, and no read.
    let response = board
        .http
        .post(board.at("/v1/logout"))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    csrf_failed(response).await;
    let response = board
        .http
        .get(board.at("/v1/boards"))
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // There is nothing to sign in to.
    let response = board
        .login_raw(&format!("{WEB_CODE_PREFIX}{}", "0".repeat(64)))
        .await;
    assert!(response.headers().get("set-cookie").is_none());
    error_of(response, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;
    board.stop().await;
}

#[tokio::test]
async fn the_authorization_header_beats_the_cookie() {
    let board = Net::start().await;
    let ada = board.sign_in("ada@example.com").await;
    let token = board.enrol("builder").await;
    let whoami = |authorization: Option<String>, cookie: Option<String>| {
        let mut request = board.http.get(board.at("/v1/whoami"));
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        request.send()
    };
    let refusal = whoami(None, None).await.unwrap().bytes().await.unwrap();

    // Both valid: the request is the agent's.
    let response = whoami(Some(format!("Bearer {token}")), Some(ada.cookie()))
        .await
        .unwrap();
    let me: WhoAmI = response.json().await.unwrap();
    assert_eq!(me.actor.handle, "builder");
    assert_eq!(me.actor.kind, ActorKind::Agent);
    // So it is a token request: no CSRF header is needed, there is no CSRF
    // token to hand out, and the agent is the author.
    let session: Session = board
        .http
        .get(board.at("/v1/web/session"))
        .bearer_auth(&token)
        .header("cookie", ada.cookie())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session.actor.handle, "builder");
    assert_eq!(session.csrf_token, None);
    assert_eq!(session.credential.map(|c| c.kind), Some(ActorKind::Agent));
    let response = board
        .http
        .post(board.at(MESSAGES))
        .bearer_auth(&token)
        .header("cookie", ada.cookie())
        .json(&json!({"channel": "dev", "text": "who wrote this?"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let posted: PostMessageResponse = response.json().await.unwrap();
    assert_eq!(posted.message.by.handle, "builder");
    assert_eq!(posted.message.by.kind, ActorKind::Agent);

    // A header that does not authenticate is not rescued by a good cookie.
    for authorization in [
        "Bearer garbage".to_owned(),
        format!("Bearer obt_{}", "ab".repeat(32)),
        format!("Bearer {}", ada.secret),
        "Basic abc".to_owned(),
        "Bearer".to_owned(),
        String::new(),
    ] {
        let response = whoami(Some(authorization.clone()), Some(ada.cookie()))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{authorization}"
        );
        assert_eq!(response.bytes().await.unwrap(), refusal);
    }
    // A good header is not spoiled by a bad cookie, and the cookie alone
    // is the person.
    let response = whoami(
        Some(format!("Bearer {token}")),
        Some(board.cookie("garbage")),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let me: WhoAmI = whoami(None, Some(ada.cookie()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.actor.handle, "ada@example.com");
    assert_eq!(me.credential.map(|c| c.id), Some(ada.credential.id.clone()));
    board.stop().await;
}

// ---------------------------------------------------------------------------
// Sessions are credentials
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logout_revokes_the_session_and_clears_the_cookie() {
    let board = Net::start().await;
    let ada = board.sign_in("ada@example.com").await;
    let phone = board.sign_in("ada@example.com").await;

    // Logging out is a write: another site cannot do it to a person.
    let response = board
        .cookie_only(Method::POST, "/v1/logout", &ada)
        .send()
        .await
        .unwrap();
    csrf_failed(response).await;
    assert_eq!(board.status_of(&ada).await, StatusCode::OK);

    let response = board
        .browser(Method::POST, "/v1/logout", &ada)
        .header("origin", board.origin())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    // The cookie that is cleared is this board's own.
    assert_eq!(
        cookie_set_by(&response),
        format!(
            "__Host-openboard_session_{}=; Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
            board.addr.port()
        )
    );

    // The session is gone for good, whatever the browser still sends, and
    // the person's other browser is untouched.
    assert_eq!(board.status_of(&ada).await, StatusCode::UNAUTHORIZED);
    let again = board
        .browser(Method::POST, "/v1/logout", &ada)
        .send()
        .await
        .unwrap();
    assert!(again.headers().get("set-cookie").is_none());
    error_of(again, StatusCode::UNAUTHORIZED, codes::UNAUTHENTICATED).await;
    assert_eq!(board.status_of(&phone).await, StatusCode::OK);
    let credentials = operator::list_credentials(&board.data_dir()).unwrap();
    assert!(credentials[0].revoked_at.is_some());
    assert!(credentials[1].revoked_at.is_none());

    // An agent's logout is as it was: no cookie is involved.
    let token = board.enrol("builder").await;
    let response = board
        .http
        .post(board.at("/v1/logout"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().get("set-cookie").is_none());

    let log = operator::security_log(&board.data_dir(), None).unwrap();
    let logouts: Vec<_> = log
        .iter()
        .filter(|e| e.kind == log_kinds::LOGOUT)
        .map(|e| {
            (
                e.principal.as_deref(),
                e.credential_id.as_deref(),
                e.detail.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        logouts,
        [
            (
                Some("ada@example.com"),
                Some(ada.credential.id.as_str()),
                Some("session")
            ),
            (Some("builder"), logouts[1].1, None),
        ]
    );
    board.stop().await;
}

/// Opens a stream and a long-poll with the session cookie, revokes the
/// session as the operator, and checks that both end without an event.
async fn revocation_ends_a_sessions_open_waits(by_name: bool) {
    let board = Arc::new(Net::start().await);
    let ada = board.sign_in("ada@example.com").await;
    let token = board.enrol("builder").await;
    board
        .post_with_token(&token, json!({"channel": "a", "text": "before"}))
        .await;

    let poll = {
        let (board, ada) = (Arc::clone(&board), ada.clone());
        tokio::spawn(async move {
            board
                .cookie_only(Method::GET, "/v1/boards/general/events?wait=30", &ada)
                .send()
                .await
                .unwrap()
        })
    };
    let stream = board
        .cookie_only(Method::GET, "/v1/boards/general/events/stream", &ada)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    let stream = tokio::spawn(drain(stream));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!poll.is_finished() && !stream.is_finished());

    let (data_dir, id) = (board.data_dir(), ada.credential.id.clone());
    let revoked = tokio::task::spawn_blocking(move || {
        if by_name {
            operator::revoke_principal(&data_dir, "ADA@example.com")
        } else {
            operator::revoke_credential(&data_dir, &id)
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(revoked.principal, "ada@example.com");
    assert_eq!(
        revoked.credentials,
        std::slice::from_ref(&ada.credential.id)
    );
    // An event the revoked session must never see.
    board
        .post_with_token(&token, json!({"channel": "a", "text": "after"}))
        .await;

    let within = Duration::from_secs(5);
    let sent = tokio::time::timeout(within, stream)
        .await
        .expect("the stream is closed promptly")
        .unwrap();
    assert!(!sent.contains("data:"), "{sent}");
    let response = tokio::time::timeout(within, poll)
        .await
        .expect("the long-poll ends promptly")
        .unwrap();
    let body = response.text().await.unwrap();
    assert!(!body.contains("message_id"), "{body}");
    let error: ErrorBody = serde_json::from_str(&body).unwrap();
    assert_eq!(error.error.code, codes::UNAUTHENTICATED);
    assert_eq!(board.status_of(&ada).await, StatusCode::UNAUTHORIZED);
    Arc::into_inner(board).unwrap().stop().await;
}

#[tokio::test]
async fn revoking_a_session_closes_its_open_stream_and_long_poll() {
    revocation_ends_a_sessions_open_waits(false).await;
}

#[tokio::test]
async fn revoking_a_person_closes_their_open_stream_and_long_poll() {
    revocation_ends_a_sessions_open_waits(true).await;
}

#[tokio::test]
async fn sessions_expire() {
    let board = Net::start().await;
    let invite = operator::create_person_invite(
        &board.data_dir(),
        "brief@example.com",
        &InviteOptions {
            token_ttl_secs: Some(1),
            ..InviteOptions::default()
        },
    )
    .unwrap();
    assert_eq!(invite.session_ttl_secs, 1);
    let response = board.login_raw(&invite.code).await;
    assert!(cookie_set_by(&response).ends_with("; Path=/; Max-Age=1"));
    let secret = cookie_set_by(&response)
        .split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_owned();
    let session: Session = response.json().await.unwrap();
    let brief = Browser {
        name: board.cookie_name(),
        secret,
        csrf: session.csrf_token.unwrap(),
        credential: session.credential.unwrap(),
    };
    assert_eq!(board.status_of(&brief).await, StatusCode::OK);

    // A long-poll that is open when the session expires ends with it.
    let started = std::time::Instant::now();
    let response = board
        .cookie_only(Method::GET, "/v1/boards/general/events?wait=30", &brief)
        .send()
        .await
        .unwrap();
    error_of(response, StatusCode::UNAUTHORIZED, codes::UNAUTHENTICATED).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(board.status_of(&brief).await, StatusCode::UNAUTHORIZED);
    // A write with it fails as unauthenticated, not as a CSRF failure.
    let response = board
        .cookie_only(Method::POST, MESSAGES, &brief)
        .json(&json!({"channel": "dev", "text": "too late"}))
        .send()
        .await
        .unwrap();
    error_of(response, StatusCode::UNAUTHORIZED, codes::UNAUTHENTICATED).await;

    let log = operator::security_log(&board.data_dir(), None).unwrap();
    let failure = log
        .iter()
        .rfind(|entry| entry.kind == log_kinds::AUTH_FAILED)
        .unwrap();
    assert_eq!(failure.detail.as_deref(), Some("expired session"));
    assert_eq!(failure.principal.as_deref(), Some("brief@example.com"));
    assert_eq!(
        failure.credential_id.as_deref(),
        Some(brief.credential.id.as_str())
    );
    board.stop().await;
}

#[tokio::test]
async fn sessions_are_rate_limited_and_capped_like_tokens() {
    let board = Arc::new(
        Net::start_with(|config| {
            config.limits.requests_per_minute = 12;
            config.limits.max_waiters = 1;
        })
        .await,
    );
    let ada = board.sign_in("ada@example.com").await;
    let grace = board.sign_in("grace@example.com").await;

    // One open wait per session.
    let stream = board
        .cookie_only(Method::GET, "/v1/boards/general/events/stream", &ada)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(200)).await;
    for path in [
        "/v1/boards/general/events?wait=5",
        "/v1/boards/general/events/stream",
    ] {
        let response = board
            .cookie_only(Method::GET, path, &ada)
            .send()
            .await
            .unwrap();
        expect_rate_limited(response).await;
    }
    drop(stream);

    // Twelve requests a minute per session, whatever the route or method:
    // ada has made three.
    for _ in 0..9 {
        assert_eq!(board.status_of(&ada).await, StatusCode::OK);
    }
    let response = board
        .cookie_only(Method::GET, "/v1/whoami", &ada)
        .send()
        .await
        .unwrap();
    expect_rate_limited(response).await;
    let response = board
        .browser(Method::POST, MESSAGES, &ada)
        .json(&json!({"channel": "dev", "text": "over the limit"}))
        .send()
        .await
        .unwrap();
    expect_rate_limited(response).await;
    // Another session has its own budget.
    assert_eq!(board.status_of(&grace).await, StatusCode::OK);
    assert_eq!(board.event_count(&grace).await, 0);

    let log = operator::security_log(&board.data_dir(), None).unwrap();
    let limited: Vec<_> = log
        .iter()
        .filter(|e| e.kind == log_kinds::RATE_LIMITED)
        .collect();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].principal.as_deref(), Some("ada@example.com"));
    assert_eq!(
        limited[0].credential_id.as_deref(),
        Some(ada.credential.id.as_str())
    );
    Arc::into_inner(board).unwrap().stop().await;
}

#[tokio::test]
async fn failed_logins_share_the_limit_of_failed_joins() {
    let board = Net::start_with(|config| config.limits.joins_per_minute = 3).await;
    let invite = board.person("ada@example.com");
    let wrong = format!("{WEB_CODE_PREFIX}{}", "0".repeat(64));

    // One failed join and two failed logins use up the address's budget.
    let response = board.join_raw(&"0".repeat(64)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = board.login_raw(&wrong).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A good code does not count, and neither does a refused origin.
    let first = board.login(&invite.code).await;
    let response = board
        .http
        .post(board.at("/v1/web/login"))
        .header("origin", "https://evil.example")
        .json(&json!({ "code": wrong }))
        .send()
        .await
        .unwrap();
    csrf_failed(response).await;
    let response = board.login_raw("not even shaped like a code").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Over the budget, at both endpoints, in whatever shape junk comes.
    expect_rate_limited(board.login_raw(&wrong).await).await;
    expect_rate_limited(board.login_raw(&invite.code).await).await;
    expect_rate_limited(board.join_raw(&"0".repeat(64)).await).await;
    let response = board
        .http
        .post(board.at("/v1/web/login"))
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    expect_rate_limited(response).await;
    // A person behind the same address with a real link still gets in,
    // and the session that exists is not affected.
    let second = board.sign_in("grace@example.com").await;
    assert_eq!(board.status_of(&second).await, StatusCode::OK);
    assert_eq!(board.status_of(&first).await, StatusCode::OK);

    let log = operator::security_log(&board.data_dir(), None).unwrap();
    let count = |kind: &str| log.iter().filter(|entry| entry.kind == kind).count();
    assert_eq!(count(log_kinds::ENROL_FAILED), 1);
    assert_eq!(count(log_kinds::WEB_LOGIN_FAILED), 2);
    assert_eq!(count(log_kinds::WEB_LOGIN_SUCCEEDED), 2);
    assert_eq!(count(log_kinds::RATE_LIMITED), 1);
    let limited = log
        .iter()
        .find(|entry| entry.kind == log_kinds::RATE_LIMITED)
        .unwrap();
    assert_eq!(limited.detail.as_deref(), Some("web login"));
    board.stop().await;
}

// ---------------------------------------------------------------------------
// People and agents on one board
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_agent_mentions_a_person_and_their_long_poll_wakes() {
    let board = Arc::new(Net::start().await);
    let ada = board.sign_in("ada@example.com").await;
    board.sign_in("grace@example.com").await;
    let token = board.enrol("builder").await;

    let waiter = {
        let (board, ada) = (Arc::clone(&board), ada.clone());
        tokio::spawn(async move {
            let response = board
                .cookie_only(
                    Method::GET,
                    "/v1/boards/general/events?wait=30&mentions_self=true",
                    &ada,
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response.json::<EventsResponse>().await.unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Not for ada: no mention, someone else, a longer address, and code.
    for text in [
        "nothing for anyone",
        "over to @grace@example.com",
        "not @ada@example.com.au or @ada@example.co",
        "`@ada@example.com` and mail ada@example.com",
    ] {
        board
            .post_with_token(&token, json!({"channel": "dev", "text": text}))
            .await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished());

    // The address in any case, at the end of a sentence.
    let wanted = board
        .post_with_token(
            &token,
            json!({"channel": "dev", "text": "please look, @Ada@Example.com."}),
        )
        .await
        .message;
    assert_eq!(wanted.mentions.len(), 1);
    assert_eq!(wanted.mentions[0].handle, "ada@example.com");
    assert_eq!(wanted.mentions[0].kind, ActorKind::Person);
    assert_eq!(wanted.mentions[0].profile, None);

    let woke = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the mention wakes the person's long-poll")
        .unwrap();
    assert_eq!(woke.events.len(), 1);
    assert_eq!(woke.events[0].message_id, wanted.id);
    assert_eq!(woke.events[0].by.handle, "builder");

    // The rendered text marks the mention, and the person can answer and
    // mention the agent back.
    let thread: Thread = board
        .cookie_only(
            Method::GET,
            &format!("/v1/boards/general/threads/{}?html=true", wanted.id),
            &ada,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        thread.root.html.as_deref(),
        Some("<p>please look, <span class=\"mention\">@Ada@Example.com</span>.</p>")
    );
    let answer = board
        .post_as(
            &ada,
            json!({"thread_id": wanted.id, "text": "on it, @builder"}),
        )
        .await
        .message;
    assert_eq!(answer.by.kind, ActorKind::Person);
    assert_eq!(answer.mentions[0].handle, "builder");
    assert_eq!(answer.mentions[0].kind, ActorKind::Agent);
    let mine: EventsResponse = board
        .http
        .get(board.at("/v1/boards/general/events?after=start&mentions_self=true"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mine.events.len(), 1);
    assert_eq!(mine.events[0].by.handle, "ada@example.com");
    assert_eq!(mine.events[0].by.kind, ActorKind::Person);
    Arc::into_inner(board).unwrap().stop().await;
}

// ---------------------------------------------------------------------------
// Review fixes
// ---------------------------------------------------------------------------

/// The security log's entries of one type, as their details.
fn logged(data_dir: &Path, kind: &str) -> Vec<Option<String>> {
    operator::security_log(data_dir, None)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == kind)
        .map(|entry| entry.detail)
        .collect()
}

#[tokio::test]
async fn loading_the_page_signed_out_is_not_a_failed_authentication() {
    let budget = 3;
    let board = Net::start_with(|config| config.limits.auth_failures_per_minute = budget).await;
    let ada = board.sign_in("ada@example.com").await;
    let data_dir = board.data_dir();
    let session = |cookie: Option<String>| {
        let mut request = board.http.get(board.at("/v1/web/session"));
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        async move { request.send().await.unwrap() }
    };

    // What the page does when nobody is signed in, many times over, from
    // one address: no `Authorization` header and no session cookie of this
    // board. Other applications' cookies, and another board's session, are
    // not a credential here.
    let mut refusal = None;
    for round in 0..(budget * 10) {
        let cookie = match round % 4 {
            0 => None,
            1 => Some("theme=dark; other=1".to_owned()),
            2 => Some(format!(
                "{SESSION_COOKIE_PREFIX}1=obs_{}; __Host-openboard_session=obs_{}",
                "ab".repeat(32),
                "cd".repeat(32)
            )),
            _ => Some(String::new()),
        };
        let response = session(cookie).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "round {round}");
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(response.headers().get("retry-after").is_none());
        let body = response.bytes().await.unwrap();
        assert_eq!(*refusal.get_or_insert(body.clone()), body, "round {round}");
    }
    let refusal = refusal.unwrap();
    // None of it was counted, and none of it was logged.
    assert_eq!(logged(&data_dir, log_kinds::AUTH_FAILED), []);
    assert_eq!(logged(&data_dir, log_kinds::RATE_LIMITED), []);
    assert_eq!(logged(&data_dir, log_kinds::LOG_SUPPRESSED), []);
    // The person who is signed in is still answered.
    assert_eq!(session(Some(ada.cookie())).await.status(), StatusCode::OK);

    // The rule is that route and that method only. Everywhere else a
    // request without a credential is a failed authentication as before,
    // and its answer is the very same one.
    let elsewhere = [
        board.http.get(board.at("/v1/whoami")),
        board.http.post(board.at("/v1/web/session")),
        board.http.get(board.at("/v1/web/session/")),
    ];
    for request in elsewhere {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        assert_eq!(response.bytes().await.unwrap(), refusal);
    }
    assert_eq!(
        logged(&data_dir, log_kinds::AUTH_FAILED),
        vec![Some("no token".to_owned()); budget as usize]
    );
    // That spent the address's budget, so the next failure is limited.
    let limited = board.http.get(board.at("/v1/whoami")).send().await.unwrap();
    expect_rate_limited(limited).await;
    // The signed-out page load is answered exactly as before, even now: it
    // says nothing about the state of the budget either.
    let response = session(None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get("retry-after").is_none());
    assert_eq!(response.bytes().await.unwrap(), refusal);
    // A credential that is presented on that route and is wrong is a
    // failed authentication like any other: here it meets the limit.
    for wrong in [
        board.cookie(&format!("obs_{}", "ab".repeat(32))),
        board.cookie("garbage"),
        board.cookie(""),
    ] {
        expect_rate_limited(session(Some(wrong)).await).await;
    }
    let with_header = board
        .http
        .get(board.at("/v1/web/session"))
        .header("authorization", "Basic abc")
        .send()
        .await
        .unwrap();
    expect_rate_limited(with_header).await;
    assert_eq!(
        logged(&data_dir, log_kinds::AUTH_FAILED).len(),
        budget as usize
    );
    board.stop().await;
}

#[tokio::test]
async fn a_wrong_credential_on_the_session_route_is_counted_and_logged() {
    let board = Net::start_with(|config| config.limits.auth_failures_per_minute = 4).await;
    let ada = board.sign_in("ada@example.com").await;
    let data_dir = board.data_dir();
    operator::revoke_credential(&data_dir, &ada.credential.id).unwrap();
    let signed_out = board
        .http
        .get(board.at("/v1/web/session"))
        .send()
        .await
        .unwrap();
    let refusal = signed_out.bytes().await.unwrap();

    let requests = [
        board
            .http
            .get(board.at("/v1/web/session"))
            .header("cookie", board.cookie(&format!("obs_{}", "ab".repeat(32)))),
        board
            .http
            .get(board.at("/v1/web/session"))
            .header("cookie", board.cookie("garbage")),
        board
            .http
            .get(board.at("/v1/web/session"))
            .header("cookie", ada.cookie()),
        board
            .http
            .get(board.at("/v1/web/session"))
            .bearer_auth(format!("obt_{}", "ab".repeat(32))),
    ];
    for request in requests {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        // The same answer as the one that was not counted.
        assert_eq!(response.bytes().await.unwrap(), refusal);
    }
    assert_eq!(
        logged(&data_dir, log_kinds::AUTH_FAILED),
        [
            Some("unknown session".to_owned()),
            Some("malformed session".to_owned()),
            Some("revoked session".to_owned()),
            Some("unknown token".to_owned()),
        ]
    );
    let fifth = board
        .http
        .get(board.at("/v1/web/session"))
        .header("cookie", board.cookie("garbage"))
        .send()
        .await
        .unwrap();
    expect_rate_limited(fifth).await;
    board.stop().await;
}

#[tokio::test]
async fn signing_in_again_replaces_the_session_of_the_same_browser() {
    let board = Net::start().await;
    let data_dir = board.data_dir();
    let first = board.sign_in("ada@example.com").await;
    // Ada's other browser, and somebody else.
    let phone = board.sign_in("ada@example.com").await;
    let bob = board.sign_in("bob@example.com").await;
    assert_eq!(logged(&data_dir, log_kinds::LOGOUT), []);

    // The browser that holds `first` opens a new sign-in link.
    let code = board.person("ada@example.com").code;
    let cookies = format!("theme=dark; {}", first.cookie());
    let response = board.login_with_cookies(&code, &cookies).await;
    let second = board.signed_in(response).await;
    assert_ne!(second.credential.id, first.credential.id);
    assert_eq!(board.status_of(&second).await, StatusCode::OK);
    // The earlier session of that browser is gone, and only that one.
    assert_eq!(board.status_of(&first).await, StatusCode::UNAUTHORIZED);
    assert_eq!(board.status_of(&phone).await, StatusCode::OK);
    assert_eq!(board.status_of(&bob).await, StatusCode::OK);
    let revoked = |id: &str| {
        operator::list_credentials(&data_dir)
            .unwrap()
            .into_iter()
            .find(|credential| credential.id == id)
            .unwrap()
            .revoked_at
            .is_some()
    };
    assert!(revoked(&first.credential.id));
    assert!(!revoked(&second.credential.id));
    // It is logged like a logout, for the session that ended.
    let log = operator::security_log(&data_dir, None).unwrap();
    let logouts: Vec<_> = log
        .iter()
        .filter(|entry| entry.kind == log_kinds::LOGOUT)
        .collect();
    assert_eq!(logouts.len(), 1);
    assert_eq!(logouts[0].principal.as_deref(), Some("ada@example.com"));
    assert_eq!(
        logouts[0].credential_id.as_deref(),
        Some(first.credential.id.as_str())
    );
    assert_eq!(
        logouts[0].detail.as_deref(),
        Some("session, replaced by a new sign-in")
    );
    assert!(logouts[0].client.is_some());

    // A cookie of another person is ignored: Ada signing in on a browser
    // that holds Bob's session does not end his.
    let code = board.person("ada@example.com").code;
    let response = board.login_with_cookies(&code, &bob.cookie()).await;
    let third = board.signed_in(response).await;
    assert_eq!(board.status_of(&bob).await, StatusCode::OK);
    assert_eq!(board.status_of(&second).await, StatusCode::OK);
    assert_eq!(board.status_of(&third).await, StatusCode::OK);

    // So is one that is no session, one that is unknown, one that is
    // already revoked, and an agent's token in the cookie.
    let token = board.enrol("builder").await;
    for cookie in [
        board.cookie("garbage"),
        board.cookie(&format!("obs_{}", "ab".repeat(32))),
        first.cookie(),
        board.cookie(&token),
        String::new(),
    ] {
        let code = board.person("ada@example.com").code;
        let response = board.login_with_cookies(&code, &cookie).await;
        let another = board.signed_in(response).await;
        assert_eq!(board.status_of(&another).await, StatusCode::OK);
    }
    let whoami = board
        .http
        .get(board.at("/v1/whoami"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(whoami.status(), StatusCode::OK);
    assert_eq!(logged(&data_dir, log_kinds::LOGOUT).len(), 1);

    // A sign-in that fails ends nothing, whatever cookie it carries.
    for code in [
        "obw1_nope",
        &format!("{WEB_CODE_PREFIX}{}", "ab".repeat(32)),
    ] {
        let response = board.login_with_cookies(code, &second.cookie()).await;
        assert!(response.headers().get("set-cookie").is_none());
        error_of(response, StatusCode::FORBIDDEN, codes::INVITE_INVALID).await;
    }
    assert_eq!(board.status_of(&second).await, StatusCode::OK);
    // And the first of two cookies of that name is the one that counts,
    // as it does when a request is authenticated.
    let code = board.person("ada@example.com").code;
    let cookies = format!("{}; {}", second.cookie(), third.cookie());
    let response = board.login_with_cookies(&code, &cookies).await;
    let last = board.signed_in(response).await;
    assert_eq!(board.status_of(&second).await, StatusCode::UNAUTHORIZED);
    assert_eq!(board.status_of(&third).await, StatusCode::OK);
    assert_eq!(board.status_of(&last).await, StatusCode::OK);
    assert_eq!(board.status_of(&phone).await, StatusCode::OK);
    assert_eq!(logged(&data_dir, log_kinds::LOGOUT).len(), 2);
    board.stop().await;
}

/// A browser's cookies for one host. Like a browser, it keeps them by
/// name, whatever port set them, and sends all of them to every port.
#[derive(Default)]
struct Jar(Vec<(String, String)>);

impl Jar {
    /// Applies a response's `Set-Cookie`.
    fn store(&mut self, response: &Response) {
        let cookie = cookie_set_by(response);
        let (name, value) = cookie.split(';').next().unwrap().split_once('=').unwrap();
        self.0.retain(|(held, _)| held != name);
        if !cookie.ends_with("Max-Age=0") {
            self.0.push((name.to_owned(), value.to_owned()));
        }
    }

    fn header(&self) -> String {
        let pairs: Vec<String> = self
            .0
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        pairs.join("; ")
    }
}

#[tokio::test]
async fn two_boards_on_one_host_keep_their_sessions_apart() {
    // Two boards on two ports of 127.0.0.1, and one browser.
    let north = Net::start().await;
    let south = Net::start().await;
    assert_ne!(north.addr.port(), south.addr.port());
    assert_eq!(north.addr.ip(), south.addr.ip());
    assert_ne!(north.cookie_name(), south.cookie_name());
    let mut jar = Jar::default();
    let whoami = |board: &Net, jar: &Jar| {
        let request = board
            .http
            .get(board.at("/v1/whoami"))
            .header("cookie", jar.header());
        async move { request.send().await.unwrap() }
    };

    let code = north.person("ada@example.com").code;
    let response = north.login_with_cookies(&code, &jar.header()).await;
    assert_eq!(response.status(), StatusCode::OK);
    jar.store(&response);
    let on_north: Session = response.json().await.unwrap();
    assert_eq!(whoami(&north, &jar).await.status(), StatusCode::OK);

    // Signing in to the second board, with the first one's cookie in the
    // jar, sets a cookie of another name and leaves the first alone.
    let code = south.person("ada@example.com").code;
    let response = south.login_with_cookies(&code, &jar.header()).await;
    assert_eq!(response.status(), StatusCode::OK);
    jar.store(&response);
    let on_south: Session = response.json().await.unwrap();
    assert_eq!(jar.0.len(), 2, "one cookie per board: {:?}", jar.0);
    assert_eq!(jar.0[0].0, north.cookie_name());
    assert_eq!(jar.0[1].0, south.cookie_name());

    // Both sessions work, each on its own board, with the whole jar sent
    // to both.
    for (board, session) in [(&north, &on_north), (&south, &on_south)] {
        let me: WhoAmI = whoami(board, &jar).await.json().await.unwrap();
        assert_eq!(me.actor.handle, "ada@example.com");
        assert_eq!(
            me.credential.map(|credential| credential.id),
            session.credential.as_ref().map(|c| c.id.clone())
        );
        // A write too, with that board's own anti-CSRF token.
        let response = board
            .http
            .post(board.at(MESSAGES))
            .header("cookie", jar.header())
            .header(CSRF_HEADER, session.csrf_token.as_deref().unwrap())
            .json(&json!({"channel": "dev", "text": "hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }
    // Neither board took the other's cookie for a failed authentication.
    for board in [&north, &south] {
        assert_eq!(logged(&board.data_dir(), log_kinds::AUTH_FAILED), []);
        assert_eq!(logged(&board.data_dir(), log_kinds::LOGOUT), []);
    }

    // The other board's cookie alone is no credential at all: the page
    // load is not counted, and a route that needs a credential says that
    // none came, not that a wrong one did.
    let only_south = format!("{}={}", jar.0[1].0, jar.0[1].1);
    let response = north
        .http
        .get(north.at("/v1/web/session"))
        .header("cookie", &only_south)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(logged(&north.data_dir(), log_kinds::AUTH_FAILED), []);
    let response = north
        .http
        .get(north.at("/v1/whoami"))
        .header("cookie", &only_south)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        logged(&north.data_dir(), log_kinds::AUTH_FAILED),
        [Some("no token".to_owned())]
    );
    // And a session of the first board under the second one's name is not
    // one of the second's.
    let crossed = format!("{}={}", south.cookie_name(), jar.0[0].1);
    let response = south
        .http
        .get(south.at("/v1/whoami"))
        .header("cookie", &crossed)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        logged(&south.data_dir(), log_kinds::AUTH_FAILED),
        [Some("unknown session".to_owned())]
    );

    // Signing out of one board clears that board's cookie only.
    let response = north
        .http
        .post(north.at("/v1/logout"))
        .header("cookie", jar.header())
        .header(CSRF_HEADER, on_north.csrf_token.as_deref().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    jar.store(&response);
    assert_eq!(jar.0.len(), 1);
    assert_eq!(jar.0[0].0, south.cookie_name());
    assert_eq!(whoami(&south, &jar).await.status(), StatusCode::OK);
    assert_eq!(
        whoami(&north, &jar).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // The name follows the port the browser used, which is the one in the
    // `Host` header: behind a forwarded port it is not the port the server
    // listens on. Without a port it is 443.
    for (host, name) in [
        ("board.example:8443", "__Host-openboard_session_8443"),
        ("board.example", "__Host-openboard_session_443"),
        ("[::1]:9000", "__Host-openboard_session_9000"),
    ] {
        let code = south.person("ada@example.com").code;
        let response = south
            .http
            .post(south.at("/v1/web/login"))
            .header("host", host)
            .json(&json!({ "code": code }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{host}");
        let cookie = cookie_set_by(&response);
        let (set, secret) = cookie.split(';').next().unwrap().split_once('=').unwrap();
        assert_eq!(set, name, "{host}");
        // It is found again under that name on that host, and under no
        // other.
        for (cookie_name, expected) in [
            (name.to_owned(), StatusCode::OK),
            (south.cookie_name(), StatusCode::UNAUTHORIZED),
        ] {
            let response = south
                .http
                .get(south.at("/v1/whoami"))
                .header("host", host)
                .header("cookie", format!("{cookie_name}={secret}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{host} {cookie_name}");
        }
    }
    // A Host whose port is not a number names no cookie: nothing a caller
    // writes there ends up in a cookie's name, and the code is not spent.
    let code = south.person("ada@example.com").code;
    for host in [
        "board.example:x=1; Path=/evil",
        "board.example:80a",
        "board.example:",
        "board.example:99999",
    ] {
        let response = south
            .http
            .post(south.at("/v1/web/login"))
            .header("host", host)
            .json(&json!({ "code": code }))
            .send()
            .await
            .unwrap();
        assert!(response.headers().get("set-cookie").is_none(), "{host}");
        error_of(response, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;
    }
    let response = south.login_raw(&code).await;
    assert_eq!(response.status(), StatusCode::OK);

    north.stop().await;
    south.stop().await;
}
