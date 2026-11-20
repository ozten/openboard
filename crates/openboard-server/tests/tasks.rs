//! End-to-end tests of tasks on a local board: the state machine, claims
//! and fencing, leases, dependencies, batches, ready claims, ownership,
//! the events and labels that mirror task state, and the concurrency gate
//! (T2): many competing claims on one task. A real server on port 0,
//! exercised over HTTP.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use openboard_api::tasks::{
    Claim, ClaimReadyResponse, CreateTaskResponse, CreateTasksResponse, TaskList, TaskResponse,
    TaskView, TASK_LABEL,
};
use openboard_api::{
    codes, ErrorBody, EventsResponse, Message, Thread, ThreadList, PROFILE_HEADER,
};
use openboard_server::authz::{Action, Decision, Policy, Principal};
use openboard_server::{verify_log, Config, ServeError};
use reqwest::{Client, Method, Response, StatusCode};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct Board {
    dir: TempDir,
    addr: SocketAddr,
    http: Client,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), ServeError>>,
}

impl Board {
    async fn start() -> Self {
        Self::start_with(None).await
    }

    async fn start_with(policy: Option<Arc<dyn Policy>>) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = TempDir::new().unwrap();
        let mut config = Config::new(dir.path().join("data"));
        config.port = 0;
        let server = match policy {
            Some(policy) => openboard_server::bind_with_policy(config, policy).await,
            None => openboard_server::bind(config).await,
        }
        .unwrap();
        let addr = server.local_addr();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        Self {
            dir,
            addr,
            http: Client::builder()
                .no_proxy()
                .pool_max_idle_per_host(64)
                .build()
                .unwrap(),
            stop: Some(stop),
            task,
        }
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

    async fn send(
        &self,
        method: Method,
        path: &str,
        profile: Option<&str>,
        body: Option<Value>,
    ) -> Response {
        let mut request = self
            .http
            .request(method, format!("http://{}{path}", self.addr));
        if let Some(profile) = profile {
            request = request.header(PROFILE_HEADER, profile);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.unwrap()
    }

    async fn post(&self, path: &str, profile: Option<&str>, body: Value) -> Response {
        self.send(
            Method::POST,
            &format!("/v1/boards/general{path}"),
            profile,
            Some(body),
        )
        .await
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        let response = self
            .send(
                Method::GET,
                &format!("/v1/boards/general{path}"),
                None,
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK, "GET {path}");
        response.json().await.unwrap()
    }

    /// Creates a task as `profile` and returns its root.
    async fn task(&self, profile: &str, body: Value) -> Message {
        let response = self.post("/tasks", Some(profile), body).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        response.json::<CreateTaskResponse>().await.unwrap().task
    }

    async fn act(&self, profile: &str, id: &str, action: &str, body: Value) -> Response {
        self.post(&format!("/tasks/{id}/{action}"), Some(profile), body)
            .await
    }

    async fn claim(&self, profile: &str, id: &str) -> Claim {
        let response = self.act(profile, id, "claim", json!({})).await;
        assert_eq!(response.status(), StatusCode::OK, "claim {id} as {profile}");
        response.json().await.unwrap()
    }

    async fn settle(&self, profile: &str, claim: &Claim, action: &str, extra: Value) -> Message {
        let mut body = json!({"epoch": claim.epoch, "token": claim.token});
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            body.extend(extra.clone());
        }
        let response = self.act(profile, &claim.task.id, action, body).await;
        assert_eq!(response.status(), StatusCode::OK, "{action}");
        response.json::<TaskResponse>().await.unwrap().task
    }

    async fn show(&self, id: &str) -> TaskView {
        self.get(&format!("/tasks/{id}")).await
    }

    async fn kinds(&self) -> Vec<String> {
        let log: EventsResponse = self.get("/events?after=start&limit=1000").await;
        log.events.into_iter().map(|event| event.kind).collect()
    }
}

fn state(message: &Message) -> &str {
    &message.task.as_ref().expect("a task").state
}

async fn expect_error(response: Response, status: StatusCode, code: &str) -> ErrorBody {
    let got = response.status();
    let body: ErrorBody = response.json().await.unwrap();
    assert_eq!((got, body.error.code.as_str()), (status, code), "{body:?}");
    assert!(!body.error.message.is_empty());
    body
}

#[tokio::test]
async fn a_task_moves_from_ready_through_claimed_to_completed() {
    let board = Board::start().await;
    let task = board
        .task(
            "lead",
            json!({"channel": "build", "text": "Fix the parser", "labels": {"role": "worker"}}),
        )
        .await;
    assert_eq!(state(&task), "ready");
    assert_eq!(task.labels[TASK_LABEL], "ready");
    assert_eq!(task.labels["role"], "worker");
    let info = task.task.as_ref().unwrap();
    assert_eq!(info.owner.handle, "local/lead");
    assert_eq!(info.epoch, 0);
    assert!(info.holder.is_none());

    let claim = board.claim("worker-1", &task.id).await;
    assert_eq!(claim.epoch, 1);
    assert!(claim.token.starts_with("tkn_"));
    assert_eq!(claim.lease_secs, 300);
    assert_eq!(state(&claim.task), "claimed");
    assert_eq!(claim.task.labels[TASK_LABEL], "claimed");
    assert_eq!(
        claim
            .task
            .task
            .as_ref()
            .unwrap()
            .holder
            .as_ref()
            .unwrap()
            .handle,
        "local/worker-1"
    );
    // The trust summary says who wrote the brief.
    assert_eq!(claim.trust.brief_by.handle, "local/lead");
    assert_eq!(claim.trust.owner.handle, "local/lead");
    assert!(!claim.trust.brief_edited);
    assert!(!claim.trust.bridged);

    // A second claim of a held task is refused.
    let again = board.act("worker-2", &task.id, "claim", json!({})).await;
    expect_error(again, StatusCode::CONFLICT, codes::TASK_CLAIMED).await;

    let renewed = board
        .act(
            "worker-1",
            &task.id,
            "renew",
            json!({"epoch": claim.epoch, "token": claim.token, "lease_secs": 60}),
        )
        .await;
    assert_eq!(renewed.status(), StatusCode::OK);
    let renewed: Claim = renewed.json().await.unwrap();
    assert_eq!(renewed.epoch, 1);
    assert_eq!(renewed.lease_secs, 60);

    let done = board
        .settle(
            "worker-1",
            &claim,
            "complete",
            json!({"result": "tests pass"}),
        )
        .await;
    assert_eq!(state(&done), "completed");
    assert_eq!(done.labels[TASK_LABEL], "completed");
    let info = done.task.as_ref().unwrap();
    assert_eq!(info.result.as_deref(), Some("tests pass"));
    assert!(info.holder.is_none());
    assert_eq!(info.changed_by.handle, "local/worker-1");

    // A claim that has ended settles nothing.
    for action in ["complete", "renew", "release"] {
        let late = board
            .act(
                "worker-1",
                &task.id,
                action,
                json!({"epoch": claim.epoch, "token": claim.token}),
            )
            .await;
        expect_error(late, StatusCode::CONFLICT, codes::CLAIM_LOST).await;
    }
    let late = board
        .act(
            "worker-1",
            &task.id,
            "fail",
            json!({"epoch": claim.epoch, "token": claim.token, "reason": "too late"}),
        )
        .await;
    expect_error(late, StatusCode::CONFLICT, codes::CLAIM_LOST).await;
    let finished = board.act("worker-2", &task.id, "claim", json!({})).await;
    expect_error(finished, StatusCode::CONFLICT, codes::TASK_FINISHED).await;

    assert_eq!(
        board.kinds().await,
        [
            "message.posted",
            "task.created",
            "task.claimed",
            "task.completed"
        ]
    );
    // The thread read carries the task and its trust summary.
    let thread: Thread = board.get(&format!("/threads/{}", task.id)).await;
    assert_eq!(state(&thread.root), "completed");
    assert!(thread.trust.is_some());
    assert!(thread.cursor.is_some());
    let dir = board.stop().await;
    assert!(verify_log(&dir.path().join("data")).unwrap().is_ok());
}

#[tokio::test]
async fn a_stale_or_foreign_claim_is_refused() {
    let board = Board::start().await;
    let task = board
        .task("lead", json!({"channel": "build", "text": "one"}))
        .await;
    let first = board.claim("worker-1", &task.id).await;
    let released = board.settle("worker-1", &first, "release", json!({})).await;
    assert_eq!(state(&released), "ready");
    let second = board.claim("worker-2", &task.id).await;
    assert_eq!(second.epoch, 2);
    assert_ne!(first.token, second.token);

    // The first claim's epoch is stale.
    let stale = board
        .act(
            "worker-1",
            &task.id,
            "complete",
            json!({"epoch": first.epoch, "token": first.token}),
        )
        .await;
    expect_error(stale, StatusCode::CONFLICT, codes::CLAIM_LOST).await;
    // The right epoch with a wrong token, or from another actor, is not held.
    let wrong = board
        .act(
            "worker-2",
            &task.id,
            "complete",
            json!({"epoch": 2, "token": "tkn_nope"}),
        )
        .await;
    expect_error(wrong, StatusCode::CONFLICT, codes::CLAIM_NOT_HELD).await;
    let foreign = board
        .act(
            "worker-1",
            &task.id,
            "complete",
            json!({"epoch": 2, "token": second.token}),
        )
        .await;
    expect_error(foreign, StatusCode::CONFLICT, codes::CLAIM_NOT_HELD).await;
    let ahead = board
        .act(
            "worker-2",
            &task.id,
            "complete",
            json!({"epoch": 3, "token": second.token}),
        )
        .await;
    expect_error(ahead, StatusCode::CONFLICT, codes::CLAIM_NOT_HELD).await;
    // Nothing changed.
    assert_eq!(state(&board.show(&task.id).await.task), "claimed");
    // A settle without the claim is a malformed request.
    let bare = board.act("worker-2", &task.id, "complete", json!({})).await;
    expect_error(bare, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;

    let failed = board
        .settle("worker-2", &second, "fail", json!({"reason": "flaky"}))
        .await;
    assert_eq!(state(&failed), "failed");
    assert_eq!(
        failed.task.as_ref().unwrap().reason.as_deref(),
        Some("flaky")
    );
    let reopened = board.act("lead", &task.id, "reopen", json!({})).await;
    assert_eq!(reopened.status(), StatusCode::OK);
    let reopened: TaskResponse = reopened.json().await.unwrap();
    assert_eq!(state(&reopened.task), "ready");
    assert!(reopened.task.task.as_ref().unwrap().reason.is_none());
    board.stop().await;
}

#[tokio::test]
async fn an_expired_lease_returns_the_task_to_the_queue() {
    let board = Board::start().await;
    let task = board
        .task("lead", json!({"channel": "build", "text": "slow"}))
        .await;
    let response = board
        .act("worker-1", &task.id, "claim", json!({"lease_secs": 5}))
        .await;
    let claim: Claim = response.json().await.unwrap();
    assert_eq!(claim.lease_secs, 5);
    let too_short = board
        .act("worker-2", &task.id, "claim", json!({"lease_secs": 1}))
        .await;
    expect_error(too_short, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;

    // The board says so in the log, without anyone asking.
    let woke: EventsResponse = board
        .get(&format!(
            "/events?after={}&types=task.expired&wait=20",
            claim.cursor
        ))
        .await;
    assert_eq!(woke.events.len(), 1, "the lease expired within the wait");
    assert_eq!(woke.events[0].message_id, task.id);
    assert_eq!(woke.events[0].by.handle, "local/worker-1");
    assert_eq!(state(&board.show(&task.id).await.task), "ready");

    let late = board
        .act(
            "worker-1",
            &task.id,
            "complete",
            json!({"epoch": claim.epoch, "token": claim.token}),
        )
        .await;
    expect_error(late, StatusCode::CONFLICT, codes::CLAIM_LOST).await;
    let next = board.claim("worker-2", &task.id).await;
    assert_eq!(next.epoch, 2);
    board.stop().await;
}

#[tokio::test]
async fn a_batch_creates_tasks_with_dependencies_that_ready_in_turn() {
    let board = Board::start().await;
    let plan = json!({"channel": "build", "idempotency_key": "plan-1", "tasks": [
        {"key": "fix", "text": "fix CVE-123", "labels": {"step": "fix"}},
        {"key": "review", "text": "review the fix", "labels": {"step": "review"}, "needs": [{"key": "fix"}]},
        {"text": "ship it", "needs": [{"key": "review"}, {"key": "fix"}], "for": "shipper"}
    ]});
    let response = board.post("/tasks/batch", Some("lead"), plan.clone()).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: CreateTasksResponse = response.json().await.unwrap();
    let [fix, review, ship] = &created.tasks[..] else {
        panic!("three tasks");
    };
    assert_eq!(state(fix), "ready");
    assert_eq!(state(review), "waiting");
    assert_eq!(state(ship), "waiting");
    assert_eq!(
        review.task.as_ref().unwrap().needs,
        std::slice::from_ref(&fix.id)
    );
    let mut ship_needs = vec![fix.id.clone(), review.id.clone()];
    ship_needs.sort();
    assert_eq!(ship.task.as_ref().unwrap().needs, ship_needs);
    assert_eq!(
        ship.task.as_ref().unwrap().assignee.as_deref(),
        Some("local/shipper")
    );

    // The same key and plan is a retry; another plan with it is a conflict.
    let again = board.post("/tasks/batch", Some("lead"), plan.clone()).await;
    assert_eq!(again.status(), StatusCode::OK);
    let again: CreateTasksResponse = again.json().await.unwrap();
    assert!(again.duplicate);
    assert_eq!(
        again.tasks.iter().map(|t| &t.id).collect::<Vec<_>>(),
        created.tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
    );
    let mut other = plan.clone();
    other["tasks"][0]["text"] = json!("something else");
    let conflict = board.post("/tasks/batch", Some("lead"), other).await;
    expect_error(conflict, StatusCode::CONFLICT, codes::IDEMPOTENCY_CONFLICT).await;

    // A waiting task cannot be claimed.
    let waiting = board.act("worker-1", &review.id, "claim", json!({})).await;
    expect_error(waiting, StatusCode::CONFLICT, codes::TASK_WAITING).await;

    let claim = board.claim("worker-1", &fix.id).await;
    let after_fix = claim.cursor.clone();
    board
        .settle("worker-1", &claim, "complete", json!({}))
        .await;
    assert_eq!(state(&board.show(&review.id).await.task), "ready");
    assert_eq!(state(&board.show(&ship.id).await.task), "waiting");
    let readied: EventsResponse = board
        .get(&format!("/events?after={after_fix}&types=task.ready"))
        .await;
    assert_eq!(readied.events.len(), 1);
    assert_eq!(readied.events[0].message_id, review.id);

    // A failed prerequisite readies nothing.
    let claim = board.claim("worker-1", &review.id).await;
    board
        .settle("worker-1", &claim, "fail", json!({"reason": "no"}))
        .await;
    assert_eq!(state(&board.show(&ship.id).await.task), "waiting");
    board.stop().await;
}

#[tokio::test]
async fn bad_needs_and_bad_batches_are_refused() {
    let board = Board::start().await;
    let cycle = json!({"channel": "build", "tasks": [
        {"key": "a", "text": "a", "needs": [{"key": "b"}]},
        {"key": "b", "text": "b", "needs": [{"key": "a"}]}
    ]});
    let refused = board.post("/tasks/batch", Some("lead"), cycle).await;
    expect_error(refused, StatusCode::BAD_REQUEST, codes::INVALID_NEEDS).await;
    for plan in [
        json!({"channel": "build", "tasks": [{"text": "a", "needs": [{"key": "nope"}]}]}),
        json!({"channel": "build", "tasks": [{"key": "a", "text": "a"}, {"key": "a", "text": "b"}]}),
        json!({"channel": "build", "tasks": [{"text": "a", "needs": ["msg_000000000000000000000000"]}]}),
    ] {
        let refused = board.post("/tasks/batch", Some("lead"), plan).await;
        expect_error(refused, StatusCode::BAD_REQUEST, codes::INVALID_NEEDS).await;
    }
    let unknown = board
        .post(
            "/tasks/batch",
            Some("lead"),
            json!({"channel": "build", "tasks": [{"text": "a", "noSync": true}]}),
        )
        .await;
    expect_error(unknown, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;
    let many: Vec<Value> = (0..16).map(|n| json!({"text": format!("t{n}")})).collect();
    let big = board
        .post(
            "/tasks/batch",
            Some("lead"),
            json!({"channel": "build", "tasks": many}),
        )
        .await;
    expect_error(big, StatusCode::BAD_REQUEST, codes::BATCH_TOO_LARGE).await;
    // Nothing of the refused batches was created.
    let listed: TaskList = board.get("/tasks?state=all").await;
    assert!(listed.tasks.is_empty());
    assert!(board.kinds().await.is_empty());

    // Changing needs cannot make a cycle either, nor name the task itself.
    let a = board
        .task("lead", json!({"channel": "build", "text": "a"}))
        .await;
    let b = board
        .task(
            "lead",
            json!({"channel": "build", "text": "b", "needs": [a.id]}),
        )
        .await;
    assert_eq!(state(&b), "waiting");
    let cyclic = board
        .act("lead", &a.id, "needs", json!({"needs": [b.id]}))
        .await;
    expect_error(cyclic, StatusCode::BAD_REQUEST, codes::INVALID_NEEDS).await;
    let itself = board
        .act("lead", &a.id, "needs", json!({"needs": [a.id]}))
        .await;
    expect_error(itself, StatusCode::BAD_REQUEST, codes::INVALID_NEEDS).await;
    // Clearing b's needs readies it.
    let cleared = board
        .act("lead", &b.id, "needs", json!({"needs": []}))
        .await;
    let cleared: TaskResponse = cleared.json().await.unwrap();
    assert_eq!(state(&cleared.task), "ready");
    board.stop().await;
}

#[tokio::test]
async fn a_ready_claim_takes_the_oldest_task_kept_for_the_caller_first() {
    let board = Board::start().await;
    let first = board
        .task(
            "lead",
            json!({"channel": "build/a", "text": "first", "labels": {"role": "worker"}}),
        )
        .await;
    let _second = board
        .task(
            "lead",
            json!({"channel": "build/b", "text": "second", "labels": {"role": "reviewer"}}),
        )
        .await;
    let kept = board
        .task(
            "lead",
            json!({"channel": "build/a", "text": "kept", "for": "worker-2"}),
        )
        .await;
    assert_eq!(
        kept.task.as_ref().unwrap().assignee.as_deref(),
        Some("local/worker-2")
    );
    // Kept for worker-2: nobody else claims it by ID.
    let taken = board.act("worker-1", &kept.id, "claim", json!({})).await;
    expect_error(taken, StatusCode::CONFLICT, codes::TASK_ASSIGNED).await;

    let claim_ready = |profile: &'static str, body: Value| {
        let board = &board;
        async move {
            let response = board.post("/tasks/claim", Some(profile), body).await;
            assert_eq!(response.status(), StatusCode::OK);
            response.json::<ClaimReadyResponse>().await.unwrap().claim
        }
    };
    // worker-2 gets the task kept for it, though it is the newest.
    let got = claim_ready("worker-2", json!({"channel": "build/"}))
        .await
        .unwrap();
    assert_eq!(got.task.id, kept.id);
    // A label condition narrows the choice.
    let got = claim_ready(
        "worker-1",
        json!({"channel": "build/", "labels": ["role=reviewer"]}),
    )
    .await
    .unwrap();
    assert_eq!(got.task.text, "second");
    let got = claim_ready("worker-1", json!({"channel": "build/"}))
        .await
        .unwrap();
    assert_eq!(got.task.id, first.id);
    // Nothing is left: no claim, at once.
    assert!(claim_ready("worker-1", json!({"channel": "build/"}))
        .await
        .is_none());

    // A claim that waits gets a task created while it waits.
    let waiter = {
        let http = board.http.clone();
        let url = format!("http://{}/v1/boards/general/tasks/claim", board.addr);
        tokio::spawn(async move {
            http.post(url)
                .header(PROFILE_HEADER, "worker-3")
                .json(&json!({"channel": "later", "wait": 20}))
                .send()
                .await
                .unwrap()
                .json::<ClaimReadyResponse>()
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let late = board
        .task("lead", json!({"channel": "later", "text": "arrives"}))
        .await;
    let woke = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(woke.claim.unwrap().task.id, late.id);
    board.stop().await;
}

#[tokio::test]
async fn blocking_waits_for_the_owner_and_editing_the_brief_ends_a_claim() {
    let board = Board::start().await;
    let task = board
        .task("lead", json!({"channel": "build", "text": "decide"}))
        .await;
    let claim = board.claim("worker-1", &task.id).await;
    let blocked = board
        .settle(
            "worker-1",
            &claim,
            "block",
            json!({"reason": "which database?", "kind": "needs_input"}),
        )
        .await;
    assert_eq!(state(&blocked), "blocked");
    let info = blocked.task.as_ref().unwrap();
    assert_eq!(info.block_kind.as_deref(), Some("needs_input"));
    assert_eq!(info.reason.as_deref(), Some("which database?"));
    // The block ended the claim.
    let late = board
        .act(
            "worker-1",
            &task.id,
            "renew",
            json!({"epoch": claim.epoch, "token": claim.token}),
        )
        .await;
    expect_error(late, StatusCode::CONFLICT, codes::CLAIM_LOST).await;
    let bad_kind = board
        .act(
            "lead",
            &task.id,
            "block",
            json!({"reason": "x", "kind": "Needs Input"}),
        )
        .await;
    expect_error(bad_kind, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;

    let unblocked = board.act("lead", &task.id, "unblock", json!({})).await;
    let unblocked: TaskResponse = unblocked.json().await.unwrap();
    assert_eq!(state(&unblocked.task), "ready");
    let again = board.act("lead", &task.id, "unblock", json!({})).await;
    expect_error(again, StatusCode::CONFLICT, codes::TASK_STATE).await;

    // The owner edits the brief while it is claimed: the claim ends.
    let claim = board.claim("worker-1", &task.id).await;
    let edited = board
        .send(
            Method::PATCH,
            &format!("/v1/boards/general/messages/{}", task.id),
            Some("lead"),
            Some(json!({"text": "decide, now with more detail"})),
        )
        .await;
    assert_eq!(edited.status(), StatusCode::OK);
    let view = board.show(&task.id).await;
    assert_eq!(state(&view.task), "ready");
    assert!(view.trust.brief_edited);
    let late = board
        .act(
            "worker-1",
            &task.id,
            "complete",
            json!({"epoch": claim.epoch, "token": claim.token}),
        )
        .await;
    expect_error(late, StatusCode::CONFLICT, codes::CLAIM_LOST).await;
    let claim = board.claim("worker-1", &task.id).await;
    assert!(claim.trust.brief_edited);

    // Deleting the brief cancels the task.
    let deleted = board
        .send(
            Method::DELETE,
            &format!("/v1/boards/general/messages/{}", task.id),
            Some("lead"),
            None,
        )
        .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    let listed: TaskList = board.get("/tasks?state=canceled").await;
    assert_eq!(listed.tasks.len(), 1);
    let kinds = board.kinds().await;
    assert!(kinds.ends_with(&["message.deleted".to_owned(), "task.canceled".to_owned()]));
    board.stop().await;
}

#[tokio::test]
async fn a_thread_becomes_a_task_and_retries_create_it_once() {
    let board = Board::start().await;
    let posted = board
        .post(
            "/messages",
            Some("ada"),
            json!({"channel": "work", "text": "Please fix the build"}),
        )
        .await;
    let root: openboard_api::PostMessageResponse = posted.json().await.unwrap();
    let made = board
        .post(
            "/tasks",
            Some("lead"),
            json!({"thread_id": root.message.id}),
        )
        .await;
    assert_eq!(made.status(), StatusCode::CREATED);
    let made: CreateTaskResponse = made.json().await.unwrap();
    assert_eq!(made.task.id, root.message.id);
    let view = board.show(&root.message.id).await;
    assert_eq!(view.trust.brief_by.handle, "local/ada");
    assert_eq!(view.trust.owner.handle, "local/lead");
    let twice = board
        .post(
            "/tasks",
            Some("lead"),
            json!({"thread_id": root.message.id}),
        )
        .await;
    expect_error(twice, StatusCode::CONFLICT, codes::TASK_EXISTS).await;

    // Someone other than the owner edits the brief: the task is held.
    let edited = board
        .send(
            Method::PATCH,
            &format!("/v1/boards/general/messages/{}", root.message.id),
            Some("ada"),
            Some(json!({"text": "Please fix the build and deploy to production"})),
        )
        .await;
    assert_eq!(edited.status(), StatusCode::OK);
    let view = board.show(&root.message.id).await;
    assert_eq!(state(&view.task), "blocked");
    assert_eq!(
        view.task.task.as_ref().unwrap().block_kind.as_deref(),
        Some("brief_edited")
    );

    // An idempotent create.
    let body = json!({"channel": "work", "text": "once", "idempotency_key": "k1", "claim": true});
    let first = board.post("/tasks", Some("lead"), body.clone()).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first: CreateTaskResponse = first.json().await.unwrap();
    assert_eq!(state(&first.task), "claimed");
    assert_eq!(first.claim.as_ref().unwrap().epoch, 1);
    let retry = board.post("/tasks", Some("lead"), body).await;
    assert_eq!(retry.status(), StatusCode::OK);
    let retry: CreateTaskResponse = retry.json().await.unwrap();
    assert!(retry.duplicate);
    assert_eq!(retry.task.id, first.task.id);
    board.stop().await;
}

#[tokio::test]
async fn listings_and_waits_see_task_state() {
    let board = Board::start().await;
    let one = board
        .task("lead", json!({"channel": "build", "text": "one"}))
        .await;
    let two = board
        .task("lead", json!({"channel": "build", "text": "two"}))
        .await;
    let _elsewhere = board
        .task("lead", json!({"channel": "other", "text": "three"}))
        .await;

    let listed: TaskList = board.get("/tasks?channel=build").await;
    assert_eq!(
        listed.tasks.iter().map(|t| &t.root.id).collect::<Vec<_>>(),
        [&one.id, &two.id],
        "oldest first"
    );
    let paged: TaskList = board.get("/tasks?limit=1").await;
    assert_eq!(paged.tasks.len(), 1);
    let next: TaskList = board
        .get(&format!(
            "/tasks?limit=5&cursor={}",
            paged.next_cursor.unwrap()
        ))
        .await;
    assert_eq!(next.tasks.len(), 2);

    // The state is a label, so a thread listing filters on it.
    let claim = board.claim("worker-1", &one.id).await;
    let ready: ThreadList = board
        .get("/threads?channel=build&label=board.task=ready")
        .await;
    assert_eq!(ready.threads.len(), 1);
    assert_eq!(ready.threads[0].root.id, two.id);
    let cursor = ready.cursor.expect("a listing carries the event position");

    // A label condition on a wait wakes on the task reaching that state.
    let waiter = {
        let http = board.http.clone();
        let url = format!(
            "http://{}/v1/boards/general/events?channel=build/&after={cursor}&wait=20&label=board.task%3Dcompleted%7Cblocked%7Cfailed",
            board.addr
        );
        tokio::spawn(async move {
            http.get(url)
                .send()
                .await
                .unwrap()
                .json::<EventsResponse>()
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    board
        .settle("worker-1", &claim, "complete", json!({"result": "ok"}))
        .await;
    let woke = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .unwrap()
        .unwrap();
    // The claim's own event is in a thread now completed, so it matches as
    // well; the last event is the completion.
    let last = woke.events.last().unwrap();
    assert_eq!(last.kind, "task.completed");
    assert_eq!(last.message_id, one.id);

    let unknown = board
        .send(
            Method::GET,
            "/v1/boards/general/tasks?state=done",
            None,
            None,
        )
        .await;
    expect_error(unknown, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;
    let reserved = board
        .post(
            &format!("/messages/{}/labels", one.id),
            None,
            json!({"set": {"board.task": "ready"}}),
        )
        .await;
    expect_error(reserved, StatusCode::BAD_REQUEST, codes::INVALID_LABEL).await;
    board.stop().await;
}

/// Allows everything except managing tasks, as a network board does until
/// grants exist.
struct NoManage;

impl Policy for NoManage {
    fn decide(&self, _: &Principal, action: Action, _: Option<&str>, _: Option<&str>) -> Decision {
        if action == Action::ManageTasks {
            Decision::Deny
        } else {
            Decision::Allow
        }
    }
}

#[tokio::test]
async fn only_the_owner_changes_a_task() {
    let board = Board::start_with(Some(Arc::new(NoManage))).await;
    let task = board
        .task("lead", json!({"channel": "build", "text": "mine"}))
        .await;
    for (action, body) in [
        ("block", json!({"reason": "no"})),
        ("cancel", json!({"reason": "no"})),
        ("assign", json!({"pool": true})),
        ("needs", json!({"needs": []})),
    ] {
        let refused = board.act("worker-1", &task.id, action, body).await;
        expect_error(refused, StatusCode::FORBIDDEN, codes::TASK_NOT_OWNED).await;
    }
    // Without a profile, the principal owns what its profiles created.
    let assigned = board
        .post(
            &format!("/tasks/{}/assign", task.id),
            None,
            json!({"assignee": "worker-1"}),
        )
        .await;
    assert_eq!(assigned.status(), StatusCode::OK);
    let canceled = board
        .act("lead", &task.id, "cancel", json!({"reason": "not needed"}))
        .await;
    let canceled: TaskResponse = canceled.json().await.unwrap();
    assert_eq!(state(&canceled.task), "canceled");
    assert_eq!(
        canceled.task.task.as_ref().unwrap().reason.as_deref(),
        Some("not needed")
    );
    board.stop().await;
}

/// The concurrency gate (T2): a thousand competing claims on one task, of
/// which exactly one wins and every other is told the task is claimed.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_thousand_competing_claims_on_one_task_have_one_winner() {
    let board = Arc::new(Board::start().await);
    let task = board
        .task("lead", json!({"channel": "build", "text": "contended"}))
        .await;
    // Bounded so the test stays under the usual open-file limit; every
    // request is still a separate claim through the whole server.
    let gate = Arc::new(tokio::sync::Semaphore::new(48));
    let mut handles = Vec::new();
    for n in 0..1000 {
        let (board, gate, id) = (Arc::clone(&board), Arc::clone(&gate), task.id.clone());
        handles.push(tokio::spawn(async move {
            let _permit = gate.acquire().await.unwrap();
            let profile = format!("w{n}");
            let response = board.act(&profile, &id, "claim", json!({})).await;
            let status = response.status();
            let body: Value = response.json().await.unwrap();
            (status, body)
        }));
    }
    let mut wins = 0;
    let mut claimed = 0;
    for handle in handles {
        let (status, body) = handle.await.unwrap();
        match status {
            StatusCode::OK => {
                wins += 1;
                assert_eq!(body["epoch"], 1);
            }
            StatusCode::CONFLICT => {
                assert_eq!(body["error"]["code"], codes::TASK_CLAIMED);
                claimed += 1;
            }
            other => panic!("unexpected {other}: {body}"),
        }
    }
    assert_eq!((wins, claimed), (1, 999));
    let kinds = board.kinds().await;
    assert_eq!(kinds.iter().filter(|k| *k == "task.claimed").count(), 1);
    let board = Arc::into_inner(board).unwrap();
    board.stop().await;
}
