//! Presence: `GET /v1/boards/{board}/presence`, the roster of who is around.
//! `docs/plans/2026-11-27-presence-roster-design.md` is the design.

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::pinned_client;
use openboard_api::auth::JoinResponse;
use openboard_api::{PresenceList, PresencePrincipal, PresenceState, PROFILE_HEADER};
use openboard_server::operator::{self, InviteOptions};
use openboard_server::{Config, Limits, ServeError};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct Server {
    dir: TempDir,
    addr: SocketAddr,
    http: Client,
    network: bool,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), ServeError>>>,
}

#[derive(Clone, Copy)]
struct Windows {
    live: Duration,
    recent: Duration,
}

const DEFAULT_WINDOWS: Windows = Windows {
    live: Duration::from_secs(120),
    recent: Duration::from_secs(900),
};

impl Server {
    async fn network(open: bool, windows: Windows) -> Self {
        Self::launch(TempDir::new().unwrap(), true, open, windows).await
    }

    async fn local() -> Self {
        Self::launch(TempDir::new().unwrap(), false, false, DEFAULT_WINDOWS).await
    }

    async fn launch(dir: TempDir, network: bool, open: bool, windows: Windows) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut config = Config::new(dir.path().join("data"));
        config.port = 0;
        config.network = network;
        config.open = open;
        config.limits = Limits {
            requests_per_minute: 100_000,
            joins_per_minute: 100_000,
            auth_failures_per_minute: 100_000,
            credential_recheck: Duration::from_millis(100),
            last_used_interval: Duration::from_millis(50),
            presence_live: windows.live,
            presence_recent: windows.recent,
            ..Limits::default()
        };
        let server = openboard_server::bind(config).await.unwrap();
        let addr = server.local_addr();
        let http = match server.fingerprint() {
            Some(fingerprint) => pinned_client(fingerprint),
            None => Client::builder().no_proxy().build().unwrap(),
        };
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        Self {
            dir,
            addr,
            http,
            network,
            stop: Some(stop),
            task: Some(task),
        }
    }

    /// Stops the server and starts a new one on the same data directory.
    async fn restart(mut self, windows: Windows) -> Self {
        self.stop.take().unwrap().send(()).unwrap();
        let task = self.task.take().unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let (network, dir) = (
            self.network,
            std::mem::replace(&mut self.dir, TempDir::new().unwrap()),
        );
        Self::launch(dir, network, false, windows).await
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn url(&self, path: &str) -> String {
        let scheme = if self.network { "https" } else { "http" };
        format!("{scheme}://127.0.0.1:{}{path}", self.addr.port())
    }

    fn get(&self, token: Option<&str>, path: &str) -> RequestBuilder {
        let request = self.http.get(self.url(path));
        match token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    fn post(&self, token: Option<&str>, path: &str, body: serde_json::Value) -> RequestBuilder {
        let request = self.http.post(self.url(path)).json(&body);
        match token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    async fn enrol(&self, name: &str) -> String {
        let invite = operator::create_invite(&self.data_dir(), name, &InviteOptions::default())
            .unwrap()
            .invite;
        let response = self
            .http
            .post(self.url("/v1/join"))
            .json(&json!({ "code": invite.code }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<JoinResponse>().await.unwrap().token
    }

    async fn say(&self, token: Option<&str>, profile: Option<&str>, channel: &str) {
        let mut request = self.post(
            token,
            "/v1/boards/general/messages",
            json!({ "channel": channel, "text": "hello" }),
        );
        if let Some(profile) = profile {
            request = request.header(PROFILE_HEADER, profile);
        }
        let response = request.send().await.unwrap();
        assert!(response.status().is_success(), "{}", response.status());
    }

    async fn roster(&self, token: Option<&str>) -> PresenceList {
        let response = self
            .get(token, "/v1/boards/general/presence")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }
}

fn find<'a>(roster: &'a PresenceList, handle: &str) -> Option<&'a PresencePrincipal> {
    roster
        .principals
        .iter()
        .find(|principal| principal.handle == handle)
}

fn grant(data_dir: &Path, pattern: &str, who: &str, verbs: &str) {
    operator::grant(data_dir, "general", pattern, who, verbs, None).unwrap();
}

/// Polls until `check` holds, for up to five seconds.
async fn eventually<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..100 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the condition did not hold within five seconds");
}

#[tokio::test]
async fn a_waiting_agent_is_live_and_says_where_it_listens() {
    let server = Server::network(true, DEFAULT_WINDOWS).await;
    let worker = server.enrol("worker").await;
    let viewer = server.enrol("viewer").await;

    let waiting = server
        .get(
            Some(&worker),
            "/v1/boards/general/events?wait=2&channel=ops/deploy",
        )
        .send();
    let waiting = tokio::spawn(waiting);
    eventually(|| async {
        let roster = server.roster(Some(&viewer)).await;
        find(&roster, "worker").is_some_and(|worker| {
            worker.state == PresenceState::Live
                && worker.seats[0].listening.len() == 1
                && worker.seats[0].listening[0].channel.as_deref() == Some("ops/deploy")
                && worker.seats[0].listening[0].kind == "wait"
        })
    })
    .await;

    // The wait ends; the worker stays live for the live window, listening
    // to nothing.
    waiting.await.unwrap().unwrap();
    let roster = server.roster(Some(&viewer)).await;
    let worker = find(&roster, "worker").unwrap();
    assert_eq!(worker.state, PresenceState::Live);
    assert!(worker.seats[0].listening.is_empty());
    assert_eq!(worker.kind, openboard_api::ActorKind::Agent);
    // The viewer sees itself, and the windows are reported.
    assert!(find(&roster, "viewer").is_some());
    assert_eq!(roster.windows.live_secs, 120);
    assert_eq!(roster.windows.recent_secs, 900);
    assert_eq!(roster.windows.listed_days, 30);
}

#[tokio::test]
async fn streams_and_task_claims_count_as_listening() {
    let server = Server::network(true, DEFAULT_WINDOWS).await;
    let worker = server.enrol("worker").await;

    let stream = server
        .get(Some(&worker), "/v1/boards/general/events/stream")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    let claim = tokio::spawn(
        server
            .post(
                Some(&worker),
                "/v1/boards/general/tasks/claim",
                json!({ "channel": "proj/x/", "wait": 2 }),
            )
            .send(),
    );
    eventually(|| async {
        let roster = server.roster(Some(&worker)).await;
        let Some(worker) = find(&roster, "worker") else {
            return false;
        };
        let mut kinds: Vec<(Option<String>, String)> = worker.seats[0]
            .listening
            .iter()
            .map(|listen| (listen.channel.clone(), listen.kind.clone()))
            .collect();
        kinds.sort();
        kinds
            == vec![
                (None, "stream".to_owned()),
                (Some("proj/x/".to_owned()), "claim_wait".to_owned()),
            ]
    })
    .await;
    drop(stream);
    let _ = claim.await.unwrap().unwrap();
    eventually(|| async {
        let roster = server.roster(Some(&worker)).await;
        find(&roster, "worker").is_some_and(|worker| worker.seats[0].listening.is_empty())
    })
    .await;
}

#[tokio::test]
async fn live_turns_recent_then_away() {
    let windows = Windows {
        live: Duration::from_millis(300),
        recent: Duration::from_millis(900),
    };
    let server = Server::network(true, windows).await;
    let agent = server.enrol("agent").await;
    let viewer = server.enrol("viewer").await;
    server.say(Some(&agent), None, "chat").await;

    let state = |roster: &PresenceList| find(roster, "agent").map(|agent| agent.state);
    assert_eq!(
        state(&server.roster(Some(&viewer)).await),
        Some(PresenceState::Live)
    );
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(
        state(&server.roster(Some(&viewer)).await),
        Some(PresenceState::Recent)
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    let roster = server.roster(Some(&viewer)).await;
    assert_eq!(state(&roster), Some(PresenceState::Away));
    // The viewer, who just asked, is live and listed first.
    assert_eq!(roster.principals[0].handle, "viewer");
}

#[tokio::test]
async fn profiles_are_seats_of_one_principal() {
    let server = Server::network(true, DEFAULT_WINDOWS).await;
    let smith = server.enrol("smith").await;
    server.say(Some(&smith), None, "chat").await;
    server.say(Some(&smith), Some("ops"), "chat").await;
    server.say(Some(&smith), Some("implementer"), "chat").await;

    let roster = server.roster(Some(&smith)).await;
    let smith = find(&roster, "smith").unwrap();
    let handles: Vec<&str> = smith
        .seats
        .iter()
        .map(|seat| seat.handle.as_str())
        .collect();
    assert_eq!(handles, ["smith", "smith/implementer", "smith/ops"]);
    assert_eq!(smith.seats[2].profile.as_deref(), Some("ops"));
    assert_eq!(roster.principals.len(), 1);
}

#[tokio::test]
async fn last_seen_times_survive_a_restart() {
    let server = Server::network(true, DEFAULT_WINDOWS).await;
    let agent = server.enrol("agent").await;
    let viewer = server.enrol("viewer").await;
    server.say(Some(&agent), None, "chat").await;
    let before = find(&server.roster(Some(&viewer)).await, "agent")
        .unwrap()
        .last_seen_at
        .clone();
    // The write is in the background; give it a moment.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let windows = Windows {
        live: Duration::from_millis(1),
        recent: Duration::from_millis(2),
    };
    let server = server.restart(windows).await;
    let roster = server.roster(Some(&viewer)).await;
    let agent = find(&roster, "agent").expect("the agent is remembered");
    assert_eq!(agent.state, PresenceState::Away);
    assert_eq!(agent.last_seen_at, before);
    assert!(agent.seats[0].listening.is_empty());
}

#[tokio::test]
async fn a_revoked_agent_is_listed_as_disabled() {
    let server = Server::network(true, DEFAULT_WINDOWS).await;
    let agent = server.enrol("agent").await;
    let viewer = server.enrol("viewer").await;
    server.say(Some(&agent), None, "chat").await;
    assert_eq!(
        find(&server.roster(Some(&viewer)).await, "agent")
            .unwrap()
            .disabled,
        None
    );

    operator::revoke_principal(&server.data_dir(), "agent").unwrap();
    let roster = server.roster(Some(&viewer)).await;
    assert_eq!(
        find(&roster, "agent").unwrap().disabled.as_deref(),
        Some("revoked")
    );
}

#[tokio::test]
async fn channels_a_viewer_cannot_read_are_withheld() {
    let server = Server::network(false, DEFAULT_WINDOWS).await;
    let ops = server.enrol("ops").await;
    let viewer = server.enrol("viewer").await;
    let outsider = server.enrol("outsider").await;
    grant(&server.data_dir(), "secret/", "ops", "read,post");
    grant(&server.data_dir(), "pub", "ops", "read,post");
    grant(&server.data_dir(), "pub", "viewer", "read,post");
    // Can read nothing on the board.
    let _ = outsider;

    let secret = tokio::spawn(
        server
            .get(
                Some(&ops),
                "/v1/boards/general/events?wait=3&channel=secret/plans",
            )
            .send(),
    );
    let public = tokio::spawn(
        server
            .get(Some(&ops), "/v1/boards/general/events?wait=3&channel=pub")
            .send(),
    );
    eventually(|| async {
        let roster = server.roster(Some(&ops)).await;
        find(&roster, "ops").is_some_and(|ops| ops.seats[0].listening.len() == 2)
    })
    .await;

    // The viewer sees that ops is live, and only the channel it can read.
    let roster = server.roster(Some(&viewer)).await;
    let seen = find(&roster, "ops").unwrap();
    assert_eq!(seen.state, PresenceState::Live);
    let channels: Vec<Option<&str>> = seen.seats[0]
        .listening
        .iter()
        .map(|listen| listen.channel.as_deref())
        .collect();
    assert_eq!(channels, [Some("pub")]);

    // A caller who can read nothing on the board gets no roster, the same
    // answer as for a board that does not exist, and is not on the roster.
    let refused = server
        .get(Some(&outsider), "/v1/boards/general/presence")
        .send()
        .await
        .unwrap();
    let missing = server
        .get(Some(&outsider), "/v1/boards/nowhere/presence")
        .send()
        .await
        .unwrap();
    assert!(!refused.status().is_success());
    assert_eq!(refused.status(), missing.status());
    let refused: serde_json::Value = refused.json().await.unwrap();
    let missing: serde_json::Value = missing.json().await.unwrap();
    assert_eq!(refused["error"]["code"], missing["error"]["code"]);
    assert!(find(&server.roster(Some(&viewer)).await, "outsider").is_none());

    let _ = secret.await;
    let _ = public.await;
}

#[tokio::test]
async fn a_local_board_lists_its_profiles_under_local() {
    let server = Server::local().await;
    server.say(None, Some("worker-1"), "chat").await;
    server.say(None, Some("worker-2"), "chat").await;
    let roster = server.roster(None).await;
    assert_eq!(roster.principals.len(), 1);
    let local = &roster.principals[0];
    assert_eq!(local.handle, "local");
    assert_eq!(local.kind, openboard_api::ActorKind::Local);
    let handles: Vec<&str> = local
        .seats
        .iter()
        .map(|seat| seat.handle.as_str())
        .collect();
    // The roster request itself is a seat too: `local`, with no profile.
    assert_eq!(handles, ["local", "local/worker-1", "local/worker-2"]);
}
