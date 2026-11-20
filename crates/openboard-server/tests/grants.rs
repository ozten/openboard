//! Grants on a network board (milestone M4): default deny, subtree access,
//! delegation, private channels, admin visibility and `explain`, end to
//! end over HTTPS. `docs/permissions.md` is the specification; the
//! evaluator's own tests are in `src/grants.rs`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use openboard_api::auth::{parse_fingerprint, JoinResponse};
use openboard_api::grants::{ChannelCreated, Explained, GrantChanged, GrantList};
use openboard_api::{codes, ErrorBody, PostMessageResponse, Thread};
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

fn pinned_client(fingerprint: &str) -> Client {
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

struct Board {
    dir: TempDir,
    addr: SocketAddr,
    http: Client,
    _stop: oneshot::Sender<()>,
    _task: JoinHandle<Result<(), ServeError>>,
}

impl Board {
    /// A fresh network board: closed, so nobody is admitted until granted.
    async fn closed() -> Self {
        Self::start(false).await
    }

    async fn start(open: bool) -> Self {
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

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn at(&self, path: &str) -> String {
        format!("https://127.0.0.1:{}{path}", self.addr.port())
    }

    async fn enrol(&self, name: &str) -> String {
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
    fn grant(&self, pattern: &str, who: &str, verbs: &str) -> String {
        operator::grant(&self.data_dir(), "general", pattern, who, verbs, None)
            .unwrap()
            .id
    }

    fn get(&self, token: &str, path: &str) -> RequestBuilder {
        self.http.get(self.at(path)).bearer_auth(token)
    }

    fn post_json(&self, token: &str, path: &str, body: Value) -> RequestBuilder {
        self.http.post(self.at(path)).bearer_auth(token).json(&body)
    }

    async fn post(&self, token: &str, channel: &str, text: &str) -> Response {
        self.post_json(
            token,
            "/v1/boards/general/messages",
            json!({ "channel": channel, "text": text }),
        )
        .send()
        .await
        .unwrap()
    }

    async fn posted(&self, token: &str, channel: &str, text: &str) -> PostMessageResponse {
        let response = self.post(token, channel, text).await;
        assert!(
            response.status().is_success(),
            "posting to {channel}: {}",
            response.status()
        );
        response.json().await.unwrap()
    }

    /// The channels `token` sees in the board's channel listing.
    async fn channels_seen(&self, token: &str) -> Vec<String> {
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

    async fn add_grant(&self, token: &str, pattern: &str, who: &str, verbs: &str) -> Response {
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

async fn error_of(response: Response, status: StatusCode, code: &str) -> ErrorBody {
    assert_eq!(response.status(), status);
    let body: ErrorBody = response.json().await.unwrap();
    assert_eq!(body.error.code, code, "{body:?}");
    body
}

#[tokio::test]
async fn a_closed_board_admits_nobody_until_the_operator_grants() {
    let board = Board::closed().await;
    let agent = board.enrol("agent-a").await;
    error_of(
        board.post(&agent, "notes", "hello").await,
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    let read = board
        .get(&agent, "/v1/boards/general/threads?channel=notes")
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::FORBIDDEN);
    // A board the agent cannot create, either.
    let other = board
        .post_json(
            &agent,
            "/v1/boards/other/messages",
            json!({ "channel": "notes", "text": "hi" }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::FORBIDDEN);

    board.grant("*", "agent-a", "read,post,create");
    board.posted(&agent, "notes", "hello").await;
    let other = board
        .post_json(
            &agent,
            "/v1/boards/other/messages",
            json!({ "channel": "notes", "text": "hi" }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        other.status(),
        StatusCode::FORBIDDEN,
        "still closed to new boards"
    );
}

#[tokio::test]
async fn a_worker_narrowed_to_a_subtree_sees_and_writes_nothing_else() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let worker = board.enrol("worker").await;
    board.grant("*", "admin", "all");
    board.grant("build/", "worker", "read,post,react,label,task.claim");
    board.posted(&admin, "build/api", "build work").await;
    board.posted(&admin, "docs", "docs work").await;
    board.posted(&admin, "buildings", "not under build/").await;

    assert_eq!(board.channels_seen(&worker).await, ["build/api"]);
    assert_eq!(
        board.channels_seen(&admin).await,
        ["build/api", "buildings", "docs"]
    );
    board.posted(&worker, "build/api", "on it").await;
    error_of(
        board.post(&worker, "docs", "sneaky").await,
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    // It can claim work but not create it.
    let created = board
        .post_json(
            &worker,
            "/v1/boards/general/tasks",
            json!({ "channel": "build/api", "text": "a task" }),
        )
        .send()
        .await
        .unwrap();
    error_of(created, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
}

#[tokio::test]
async fn granting_needs_authority_that_holds_every_verb_given() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let lead = board.enrol("lead").await;
    let worker = board.enrol("worker").await;
    board.grant("*", "admin", "all");

    // Nobody but the admin may grant.
    error_of(
        board.add_grant(&lead, "build/", "worker", "read").await,
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
    // The admin delegates build/ to the lead.
    let added = board
        .add_grant(&admin, "build/", "lead", "read,post,task,grant.manage")
        .await;
    assert_eq!(added.status(), StatusCode::CREATED);
    let added: GrantChanged = added.json().await.unwrap();
    assert_eq!(added.grant.grantee, "lead");
    assert!(added.notices.is_empty());

    // Within build/ and its own verbs: yes.
    let ok = board
        .add_grant(&lead, "build/api", "worker", "read,post,task.claim")
        .await;
    assert_eq!(ok.status(), StatusCode::CREATED);
    // A verb it does not hold, or outside build/: no.
    error_of(
        board
            .add_grant(&lead, "build/", "worker", "read,delete")
            .await,
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
    error_of(
        board.add_grant(&lead, "docs/", "worker", "read").await,
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
    // A grant to everyone can only close.
    error_of(
        board.add_grant(&admin, "*", "everyone", "read").await,
        StatusCode::BAD_REQUEST,
        codes::INVALID_GRANT,
    )
    .await;
    error_of(
        board
            .add_grant(&admin, "*", "nobody-by-this-name", "read")
            .await,
        StatusCode::NOT_FOUND,
        codes::NOT_FOUND,
    )
    .await;

    // Each sees the grants it manages.
    let lists = |token: String| {
        let request = board.get(&token, "/v1/boards/general/grants");
        async move {
            let list: GrantList = request.send().await.unwrap().json().await.unwrap();
            list.grants
                .into_iter()
                .map(|grant| format!("{} {} {}", grant.pattern, grant.grantee, grant.verbs))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        lists(lead.clone()).await,
        [
            "build/ lead read,post,task.create,task.claim,grant.manage",
            "build/api worker read,post,task.claim"
        ]
    );
    assert_eq!(lists(admin.clone()).await.len(), 3);
    assert!(lists(worker.clone()).await.is_empty());

    // The lead removes its own grant to the worker; the worker cannot.
    let worker_grant = added_id(&board, &lead, "build/api").await;
    let removed = board
        .http
        .delete(board.at(&format!("/v1/boards/general/grants/{worker_grant}")))
        .bearer_auth(&worker)
        .send()
        .await
        .unwrap();
    error_of(removed, StatusCode::FORBIDDEN, codes::NO_AUTHORITY).await;
    let removed = board
        .http
        .delete(board.at(&format!("/v1/boards/general/grants/{worker_grant}")))
        .bearer_auth(&lead)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::OK);
}

async fn added_id(board: &Board, token: &str, pattern: &str) -> String {
    let list: GrantList = board
        .get(token, "/v1/boards/general/grants")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    list.grants
        .into_iter()
        .find(|grant| grant.pattern == pattern)
        .unwrap()
        .id
}

#[tokio::test]
async fn a_private_channel_admits_its_creator_and_whom_it_admits() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let ada = board.enrol("ada").await;
    let bob = board.enrol("bob").await;
    board.grant("*", "admin", "all");
    board.grant("*", "agents", "read,post,create");

    let made = board
        .post_json(
            &ada,
            "/v1/boards/general/channels",
            json!({ "path": "plans", "private": true }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(made.status(), StatusCode::CREATED);
    let made: ChannelCreated = made.json().await.unwrap();
    assert_eq!(made.grants.len(), 2);
    board.posted(&ada, "plans/q3", "secret plan").await;
    assert_eq!(board.channels_seen(&bob).await, Vec::<String>::new());
    error_of(
        board.post(&bob, "plans", "let me in").await,
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;

    // Not where it exists or a grant reaches.
    let again = board
        .post_json(
            &bob,
            "/v1/boards/general/channels",
            json!({ "path": "plans", "private": true }),
        )
        .send()
        .await
        .unwrap();
    error_of(again, StatusCode::CONFLICT, codes::CHANNEL_EXISTS).await;

    // The owner admits bob.
    assert_eq!(
        board
            .add_grant(&ada, "plans/", "bob", "read,post")
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(board.channels_seen(&bob).await, ["plans", "plans/q3"]);

    // The admin can let themselves in, and it shows in the channel (Z9).
    assert_eq!(board.channels_seen(&admin).await, Vec::<String>::new());
    let me = board.add_grant(&admin, "plans/", "admin", "read").await;
    assert_eq!(me.status(), StatusCode::CREATED);
    let me: GrantChanged = me.json().await.unwrap();
    assert_eq!(me.notices.len(), 2, "plans and plans/q3 both exist");
    let thread: Thread = board
        .get(
            &bob,
            &format!("/v1/boards/general/threads/{}", me.notices[0]),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        thread.root.text.contains("Admin access"),
        "{}",
        thread.root.text
    );
    assert_eq!(
        thread
            .root
            .labels
            .get(openboard_api::grants::NOTICE_LABEL)
            .map(String::as_str),
        Some(openboard_api::grants::NOTICE_ACCESS_GRANTED)
    );
    assert_eq!(thread.root.by.handle, "admin");
}

#[tokio::test]
async fn explain_names_the_deciding_grant_and_guards_others_access() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let worker = board.enrol("worker").await;
    board.grant("*", "admin", "all");
    let narrow = board.grant("build/", "worker", "read,post");

    let explained: Explained = board
        .get(&worker, "/v1/boards/general/explain?channel=build/api")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(explained.verbs, "read,post");
    assert_eq!(explained.principal, "worker");
    assert_eq!(explained.deciding[0].id, narrow);

    let nothing: Explained = board
        .get(&worker, "/v1/boards/general/explain?channel=docs")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(nothing.verbs, "none");
    assert!(nothing.deciding.is_empty());

    let refused = board
        .get(&worker, "/v1/boards/general/explain?channel=docs&for=admin")
        .send()
        .await
        .unwrap();
    error_of(refused, StatusCode::FORBIDDEN, codes::NO_AUTHORITY).await;
    let about: Explained = board
        .get(
            &admin,
            "/v1/boards/general/explain?channel=build/x&for=worker",
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(about.principal, "worker");
    assert_eq!(about.verbs, "read,post");
}

#[tokio::test]
async fn task_manage_lets_an_admin_change_tasks_it_did_not_create() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let lead = board.enrol("lead").await;
    let other = board.enrol("other").await;
    board.grant("*", "admin", "all");
    board.grant("*", "agents", "read,post,create,task");
    let created: Value = board
        .post_json(
            &lead,
            "/v1/boards/general/tasks",
            json!({ "channel": "build", "text": "do it" }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["task"]["id"].as_str().unwrap().to_owned();
    let cancel = |token: &str| {
        board.post_json(
            token,
            &format!("/v1/boards/general/tasks/{id}/cancel"),
            json!({ "reason": "not needed" }),
        )
    };
    error_of(
        cancel(&other).send().await.unwrap(),
        StatusCode::FORBIDDEN,
        codes::NOT_TASK_CREATOR,
    )
    .await;
    assert_eq!(
        cancel(&admin).send().await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_open_board_admits_everyone_and_its_creators_own_their_boards() {
    let board = Board::start(true).await;
    let ada = board.enrol("ada").await;
    let bob = board.enrol("bob").await;
    board.posted(&ada, "notes", "general is open").await;
    let created = board
        .post_json(
            &ada,
            "/v1/boards/ada-board/messages",
            json!({ "channel": "notes", "text": "mine" }),
        )
        .send()
        .await
        .unwrap();
    assert!(created.status().is_success());
    // Bob can read it, and Ada holds grant.manage there.
    let list: GrantList = board
        .get(&ada, "/v1/boards/ada-board/grants")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list
        .grants
        .iter()
        .any(|grant| grant.grantee == "ada" && grant.verbs == "all"));
    let read = board
        .get(&bob, "/v1/boards/ada-board/threads?channel=notes")
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::OK);
    error_of(
        board
            .post_json(
                &bob,
                "/v1/boards/ada-board/grants",
                json!({ "pattern": "*", "grantee": "bob", "verbs": "all" }),
            )
            .send()
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
}

// ---------------------------------------------------------------------------
// Derived tokens (A9, A10)
// ---------------------------------------------------------------------------

impl Board {
    async fn mint(&self, token: &str, body: Value) -> Response {
        self.post_json(token, "/v1/tokens", body)
            .send()
            .await
            .unwrap()
    }

    async fn minted(&self, token: &str, can: &[&str]) -> openboard_api::tokens::TokenMinted {
        let response = self
            .mint(token, json!({ "board": "general", "can": can }))
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        response.json().await.unwrap()
    }
}

#[tokio::test]
async fn a_derived_token_only_narrows_and_dies_with_its_parent() {
    let board = Board::closed().await;
    let lead = board.enrol("lead").await;
    board.grant("*", "lead", "all");
    board.posted(&lead, "docs", "not for the worker").await;
    board.posted(&lead, "build/api", "work here").await;

    let worker = board
        .minted(&lead, &["build/=read,post,react,label,task.claim"])
        .await;
    assert_eq!(worker.info.can, ["build/=read,post,react,label,task.claim"]);
    assert!(worker.info.credential.parent.is_some());
    let token = worker.token.clone();

    board.posted(&token, "build/api", "within scope").await;
    error_of(
        board.post(&token, "docs", "outside").await,
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    assert_eq!(board.channels_seen(&token).await, ["build/api"]);
    // It holds what the lead holds there, no more: no task.create.
    let created = board
        .post_json(
            &token,
            "/v1/boards/general/tasks",
            json!({ "channel": "build/api", "text": "a task" }),
        )
        .send()
        .await
        .unwrap();
    error_of(created, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    // A derived token cannot change grants, even with grant.manage in scope.
    let wide = board.minted(&lead, &["*=all"]).await;
    error_of(
        board.add_grant(&wide.token, "build/", "lead", "read").await,
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
    // Nor create a board or a private channel.
    let other = board
        .post_json(
            &wide.token,
            "/v1/boards/general/channels",
            json!({ "path": "secret", "private": true }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::FORBIDDEN);
    // A token for general is nothing on another board.
    operator::grant(&board.data_dir(), "other", "*", "lead", "all", None).unwrap();
    let elsewhere = board
        .get(&wide.token, "/v1/boards/other/threads?channel=notes")
        .send()
        .await
        .unwrap();
    assert_eq!(elsewhere.status(), StatusCode::FORBIDDEN);

    // Minting from a derived token: the child is still within the parent.
    let grandchild = board.minted(&token, &["*=read,post"]).await;
    error_of(
        board.post(&grandchild.token, "docs", "still outside").await,
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    board
        .posted(&grandchild.token, "build/api", "inside both")
        .await;

    // The lead lists and revokes; everything below goes with it.
    let listed: openboard_api::tokens::TokenList = board
        .get(&lead, "/v1/tokens")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed.tokens.len(), 3);
    let mine: openboard_api::tokens::TokenList = board
        .get(&token, "/v1/tokens")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        mine.tokens.len(),
        1,
        "a derived token sees only its own children"
    );
    let revoked = board
        .http
        .delete(board.at(&format!("/v1/tokens/{}", worker.info.credential.id)))
        .bearer_auth(&lead)
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::OK);
    for dead in [&token, &grandchild.token] {
        let response = board.get(dead, "/v1/whoami").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    board.posted(&lead, "docs", "the parent lives on").await;
    board
        .posted(&wide.token, "docs", "and its other child")
        .await;
}

#[tokio::test]
async fn a_derived_token_cannot_outlive_its_parent_and_binds_its_profile() {
    let board = Board::closed().await;
    let lead = board.enrol("lead").await;
    board.grant("*", "lead", "all");
    let short = board
        .mint(
            &lead,
            json!({ "board": "general", "can": ["*=all"], "ttl_secs": 60 }),
        )
        .await;
    let short: openboard_api::tokens::TokenMinted = short.json().await.unwrap();
    let child = board
        .mint(
            &short.token,
            json!({ "board": "general", "can": ["*=all"], "ttl_secs": 86_400, "profile": "fx" }),
        )
        .await;
    assert_eq!(child.status(), StatusCode::CREATED);
    let child: openboard_api::tokens::TokenMinted = child.json().await.unwrap();
    assert!(child.info.credential.expires_at <= short.info.credential.expires_at);
    assert_eq!(child.info.credential.profile.as_deref(), Some("fx"));

    let posted = board.posted(&child.token, "notes", "as fx").await;
    assert_eq!(posted.message.by.profile.as_deref(), Some("fx"));
    let other = board
        .post_json(
            &child.token,
            "/v1/boards/general/messages",
            json!({ "channel": "notes", "text": "as ops" }),
        )
        .header(openboard_api::PROFILE_HEADER, "ops")
        .send()
        .await
        .unwrap();
    error_of(other, StatusCode::BAD_REQUEST, codes::INVALID_PROFILE).await;

    for bad in [
        json!({ "board": "general", "can": [] }),
        json!({ "board": "general", "can": ["build/"] }),
        json!({ "board": "general", "can": ["build/=fly"] }),
        json!({ "board": "general", "can": ["*=read"], "ttl_secs": 0 }),
    ] {
        let response = board.mint(&lead, bad.clone()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

// ---------------------------------------------------------------------------
// Profile limits (A8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_limited_profile_narrows_only_on_its_board_and_only_when_declared() {
    let board = Board::closed().await;
    let ada = board.enrol("ada").await;
    board.grant("*", "ada", "all");
    operator::grant(&board.data_dir(), "other", "*", "ada", "all", None).unwrap();
    board.posted(&ada, "build/api", "work").await;
    board.posted(&ada, "docs", "words").await;

    let set = board
        .http
        .put(board.at("/v1/boards/general/profiles/fx"))
        .bearer_auth(&ada)
        .json(&json!({ "can": ["build/=read,post"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(set.status(), StatusCode::OK);
    let as_fx = |channel: &str| {
        board
            .post_json(
                &ada,
                "/v1/boards/general/messages",
                json!({ "channel": channel, "text": "as fx" }),
            )
            .header(openboard_api::PROFILE_HEADER, "fx")
    };
    assert!(as_fx("build/api")
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    error_of(
        as_fx("docs").send().await.unwrap(),
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    // Undeclared, or declared on another board: Ada's own access.
    board.posted(&ada, "docs", "as ada").await;
    let elsewhere = board
        .post_json(
            &ada,
            "/v1/boards/other/messages",
            json!({ "channel": "docs", "text": "fx elsewhere" }),
        )
        .header(openboard_api::PROFILE_HEADER, "fx")
        .send()
        .await
        .unwrap();
    assert!(elsewhere.status().is_success());
    let explained: Explained = board
        .get(&ada, "/v1/boards/general/explain?channel=build/api")
        .header(openboard_api::PROFILE_HEADER, "fx")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(explained.verbs, "read,post");
    assert!(explained.narrowed);

    // A token bound to fx carries the limit; it cannot lift it.
    let bound = board
        .mint(
            &ada,
            json!({ "board": "general", "can": ["*=all"], "profile": "fx" }),
        )
        .await;
    let bound: openboard_api::tokens::TokenMinted = bound.json().await.unwrap();
    error_of(
        board.post(&bound.token, "docs", "via token").await,
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    let lift = board
        .http
        .delete(board.at("/v1/boards/general/profiles/fx"))
        .bearer_auth(&bound.token)
        .send()
        .await
        .unwrap();
    error_of(lift, StatusCode::FORBIDDEN, codes::DERIVED_TOKEN).await;

    let lift = board
        .http
        .delete(board.at("/v1/boards/general/profiles/fx"))
        .bearer_auth(&ada)
        .send()
        .await
        .unwrap();
    assert_eq!(lift.status(), StatusCode::NO_CONTENT);
    board.posted(&bound.token, "docs", "lifted").await;
}

// ---------------------------------------------------------------------------
// Delivery-time checks (Z14) and claims across a subtree
// ---------------------------------------------------------------------------

#[tokio::test]
async fn removing_a_grant_ends_open_waits_and_streams() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let worker = board.enrol("worker").await;
    board.grant("*", "admin", "all");
    let grant = board.grant("build/", "worker", "read,post");
    board.posted(&admin, "build/api", "work").await;

    let poll = board
        .get(
            &worker,
            "/v1/boards/general/events?channel=build/api&wait=30",
        )
        .send();
    let stream = board
        .get(&worker, "/v1/boards/general/events/stream?channel=build/")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    let data_dir = board.data_dir();
    let revoke = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        tokio::task::spawn_blocking(move || operator::ungrant(&data_dir, &grant))
            .await
            .unwrap()
            .unwrap();
    };
    let (response, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(poll, revoke) })
            .await
            .expect("the long-poll ends promptly");
    error_of(response.unwrap(), StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    let sent = tokio::time::timeout(Duration::from_secs(5), stream.text())
        .await
        .expect("the stream ends promptly")
        .unwrap();
    assert!(!sent.contains("data:"), "{sent}");
}

#[tokio::test]
async fn a_narrowed_grant_filters_what_an_open_wait_delivers() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let worker = board.enrol("worker").await;
    board.grant("*", "admin", "all");
    board.grant("*", "worker", "read");
    board.posted(&admin, "docs", "open for now").await;
    let start: Value = board
        .get(&worker, "/v1/boards/general/events")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cursor = start["cursor"].as_str().unwrap().to_owned();
    let poll = board
        .get(
            &worker,
            &format!("/v1/boards/general/events?after={cursor}&wait=10"),
        )
        .send();
    let narrow = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let response = board.add_grant(&admin, "docs", "worker", "none").await;
        assert_eq!(response.status(), StatusCode::CREATED);
        // Past the next recheck.
        tokio::time::sleep(Duration::from_millis(300)).await;
        board.posted(&admin, "docs", "closed now").await;
        board.posted(&admin, "build", "still open").await;
    };
    let (response, ()) = tokio::join!(poll, narrow);
    let events: Value = response.unwrap().json().await.unwrap();
    let channels: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["channel"].as_str().unwrap())
        .collect();
    assert!(!channels.contains(&"docs"), "{events}");
}

#[tokio::test]
async fn a_claim_across_a_subtree_takes_only_what_the_caller_may_claim() {
    let board = Board::closed().await;
    let lead = board.enrol("lead").await;
    let worker = board.enrol("worker").await;
    board.grant("*", "lead", "all");
    board.grant("build/", "worker", "read,post");
    board.grant("build/api", "worker", "read,post,task.claim");
    for channel in ["build/web", "build/api"] {
        let created = board
            .post_json(
                &lead,
                "/v1/boards/general/tasks",
                json!({ "channel": channel, "text": format!("work in {channel}") }),
            )
            .send()
            .await
            .unwrap();
        assert!(created.status().is_success(), "{}", created.status());
    }
    let claimed: Value = board
        .post_json(
            &worker,
            "/v1/boards/general/tasks/claim",
            json!({ "channel": "build/", "from": ["lead"] }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        claimed["claim"]["task"]["channel"], "build/api",
        "{claimed}"
    );
    let again: Value = board
        .post_json(
            &worker,
            "/v1/boards/general/tasks/claim",
            json!({ "channel": "build/", "from": ["lead"] }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(again["claim"].is_null(), "{again}");
    // Where it holds no task.claim at all, the claim is refused outright.
    let refused = board
        .post_json(
            &worker,
            "/v1/boards/general/tasks/claim",
            json!({ "channel": "build/web" }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
}
