//! End-to-end tests: a real server on port 0, exercised over HTTP.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openboard_api::{
    codes, ChannelList, ErrorBody, Event, EventsResponse, Health, PostMessageResponse, Thread,
    ThreadList, IDEMPOTENCY_HEADER, PROFILE_HEADER,
};
use openboard_server::authz::{Action, Decision, Policy, Principal};
use openboard_server::{verify_log, Config, ServeError, DB_FILE_NAME};
use reqwest::{Client, Response, StatusCode};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const ZEROS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

struct Board {
    dir: TempDir,
    addr: SocketAddr,
    http: Client,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), ServeError>>,
}

impl Board {
    async fn start() -> Self {
        Self::start_with(|_| {}, None).await
    }

    async fn start_with(tweak: impl FnOnce(&mut Config), policy: Option<Arc<dyn Policy>>) -> Self {
        // The network tests in this crate enable reqwest's TLS support,
        // which then wants a crypto provider even for plain HTTP clients.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = TempDir::new().unwrap();
        let mut config = Config::new(dir.path().join("data"));
        config.port = 0;
        tweak(&mut config);
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
            http: Client::builder().no_proxy().build().unwrap(),
            stop: Some(stop),
            task,
        }
    }

    fn data_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("data")
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
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

    async fn post_raw(&self, board: &str, profile: Option<&str>, body: Value) -> Response {
        let mut request = self
            .http
            .post(self.url(&format!("/v1/boards/{board}/messages")))
            .json(&body);
        if let Some(profile) = profile {
            request = request.header(PROFILE_HEADER, profile);
        }
        request.send().await.unwrap()
    }

    async fn post(&self, board: &str, profile: Option<&str>, body: Value) -> PostMessageResponse {
        let response = self.post_raw(board, profile, body).await;
        assert!(response.status().is_success(), "{}", response.status());
        response.json().await.unwrap()
    }

    async fn thread(&self, channel: &str, text: &str) -> PostMessageResponse {
        self.post("general", None, json!({"channel": channel, "text": text}))
            .await
    }

    async fn get(&self, path: &str) -> Response {
        self.http.get(self.url(path)).send().await.unwrap()
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        let response = self.get(path).await;
        assert_eq!(response.status(), StatusCode::OK, "GET {path}");
        response.json().await.unwrap()
    }

    async fn events(&self, query: &str) -> EventsResponse {
        self.get_json(&format!("/v1/boards/general/events?{query}"))
            .await
    }
}

async fn expect_error(response: Response, status: StatusCode, code: &str) -> ErrorBody {
    assert_eq!(response.status(), status, "status for expected {code}");
    let body: ErrorBody = response.json().await.unwrap();
    assert_eq!(body.error.code, code);
    assert!(!body.error.message.is_empty());
    body
}

fn message_ids(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|event| event.message_id.clone())
        .collect()
}

#[tokio::test]
async fn health_reports_local_mode() {
    let board = Board::start().await;
    let health: Health = board.get_json("/v1/health").await;
    assert_eq!(health.status, "ok");
    assert_eq!(health.mode, "local");
    assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
    board.stop().await;
}

#[tokio::test]
async fn post_reply_and_read() {
    let board = Board::start().await;

    let root = board.thread("research/web", "findings").await;
    assert!(!root.duplicate);
    assert_eq!(root.message.board, "general");
    assert_eq!(root.message.channel, "research/web");
    assert_eq!(root.message.thread_id, root.message.id);
    assert_eq!(root.message.text, "findings");
    assert_eq!(root.message.revision, 1);
    assert_eq!(root.message.by.handle, "local");
    assert_eq!(root.message.by.profile, None);
    assert!(root.message.created_at.ends_with('Z'));

    let reply = board
        .post(
            "general",
            None,
            json!({"thread_id": root.message.id, "text": "thanks"}),
        )
        .await;
    assert_eq!(reply.message.thread_id, root.message.id);
    assert_eq!(reply.message.channel, "research/web");
    assert_ne!(reply.message.id, root.message.id);

    // A reply to a reply attaches to the root.
    let nested = board
        .post(
            "general",
            None,
            json!({"thread_id": reply.message.id, "text": "and more"}),
        )
        .await;
    assert_eq!(nested.message.thread_id, root.message.id);

    let thread: Thread = board
        .get_json(&format!("/v1/boards/general/threads/{}", root.message.id))
        .await;
    assert_eq!(thread.root, root.message);
    assert_eq!(thread.replies, vec![reply.message.clone(), nested.message]);

    // The board name is case-insensitive and keeps its first spelling.
    let same: Thread = board
        .get_json(&format!("/v1/boards/GENERAL/threads/{}", root.message.id))
        .await;
    assert_eq!(same.root.board, "general");

    // Other boards do not see it.
    let elsewhere = board
        .get(&format!("/v1/boards/other/threads/{}", root.message.id))
        .await;
    expect_error(elsewhere, StatusCode::NOT_FOUND, codes::NOT_FOUND).await;
    board.stop().await;
}

#[tokio::test]
async fn list_threads_pages_newest_activity_first() {
    let board = Board::start().await;
    let mut roots = Vec::new();
    for n in 0..5 {
        roots.push(board.thread("dev", &format!("thread {n}")).await.message);
    }
    board.thread("dev/sub_1", "below").await;
    board.thread("devx", "a sibling, not a child").await;
    board
        .thread("dev/subx1", "underscore is not a wildcard")
        .await;
    // A reply moves the oldest thread to the top.
    board
        .post(
            "general",
            None,
            json!({"thread_id": roots[0].id, "text": "bump"}),
        )
        .await;

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut path = "/v1/boards/general/threads?channel=dev&limit=2".to_owned();
        if let Some(cursor) = &cursor {
            path.push_str(&format!("&cursor={cursor}"));
        }
        let page: ThreadList = board.get_json(&path).await;
        assert!(page.threads.len() <= 2);
        pages += 1;
        seen.extend(page.threads);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    let order: Vec<&str> = seen.iter().map(|t| t.root.text.as_str()).collect();
    assert_eq!(
        order,
        ["thread 0", "thread 4", "thread 3", "thread 2", "thread 1"]
    );
    assert_eq!(seen[0].reply_count, 1);
    assert_eq!(seen[1].reply_count, 0);
    assert!(seen[0].last_activity_at >= seen[0].root.created_at);

    // A trailing slash names the subtree: the channel and everything below.
    let subtree: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=dev/")
        .await;
    assert_eq!(subtree.threads.len(), 7);
    assert!(subtree.next_cursor.is_none());
    let exact: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=dev/sub_1")
        .await;
    assert_eq!(exact.threads.len(), 1);
    let wildcard: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=dev/sub_1/")
        .await;
    assert_eq!(wildcard.threads.len(), 1);

    let channels: ChannelList = board.get_json("/v1/boards/general/channels").await;
    let summary: Vec<(&str, u64)> = channels
        .channels
        .iter()
        .map(|c| (c.path.as_str(), c.thread_count))
        .collect();
    assert_eq!(
        summary,
        [("dev", 5), ("dev/sub_1", 1), ("dev/subx1", 1), ("devx", 1)]
    );
    assert_eq!(
        channels.channels[0].last_activity_at,
        seen[0].last_activity_at
    );

    // A cursor from another board is refused.
    board
        .post("other", None, json!({"channel": "dev", "text": "x"}))
        .await;
    let foreign = board
        .get(&format!(
            "/v1/boards/other/threads?channel=dev&cursor={}",
            cursor.unwrap()
        ))
        .await;
    expect_error(foreign, StatusCode::BAD_REQUEST, codes::INVALID_CURSOR).await;
    board.stop().await;
}

#[tokio::test]
async fn actor_comes_from_the_server_and_the_profile_header() {
    let board = Board::start().await;
    let forged = json!({
        "channel": "dev",
        "text": "hi",
        "by": {"id": "act_forged", "kind": "person", "handle": "root"},
    });
    let plain = board.post("general", None, forged.clone()).await.message;
    assert_eq!(plain.by.handle, "local");
    assert_eq!(serde_json::to_value(plain.by.kind).unwrap(), json!("local"));
    assert_ne!(plain.by.id, "act_forged");

    let first = board
        .post("general", Some("fx"), forged.clone())
        .await
        .message;
    let second = board.post("other", Some("fx"), forged).await.message;
    assert_eq!(first.by.handle, "local/fx");
    assert_eq!(first.by.profile.as_deref(), Some("fx"));
    assert_eq!(first.by.id, second.by.id);
    assert_ne!(first.by.id, plain.by.id);
    board.stop().await;
}

#[tokio::test]
async fn idempotent_retry_and_conflict() {
    let board = Board::start().await;
    let body = json!({"channel": "dev", "text": "once", "idempotency_key": "k1"});

    let first = board.post_raw("general", None, body.clone()).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first: PostMessageResponse = first.json().await.unwrap();
    assert!(!first.duplicate);

    let retry = board.post_raw("general", None, body.clone()).await;
    assert_eq!(retry.status(), StatusCode::OK);
    let retry: PostMessageResponse = retry.json().await.unwrap();
    assert!(retry.duplicate);
    assert_eq!(retry.message, first.message);
    assert_eq!(retry.cursor, first.cursor);

    let conflict = board
        .post_raw(
            "general",
            None,
            json!({"channel": "dev", "text": "different", "idempotency_key": "k1"}),
        )
        .await;
    expect_error(conflict, StatusCode::CONFLICT, codes::IDEMPOTENCY_CONFLICT).await;

    // Keys are scoped per actor and per board.
    let other_actor = board.post("general", Some("fx"), body.clone()).await;
    assert!(!other_actor.duplicate);
    let other_board = board.post("other", None, body.clone()).await;
    assert!(!other_board.duplicate);

    // The header carries a key too, and the body field wins over it.
    let send = |key: &'static str, body: Value| {
        board
            .http
            .post(board.url("/v1/boards/general/messages"))
            .header(IDEMPOTENCY_HEADER, key)
            .json(&body)
            .send()
    };
    let plain = json!({"channel": "dev", "text": "via header"});
    let by_header: PostMessageResponse = send("h1", plain.clone())
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let again: PostMessageResponse = send("h1", plain).await.unwrap().json().await.unwrap();
    assert!(again.duplicate);
    assert_eq!(again.message.id, by_header.message.id);
    let body_wins: PostMessageResponse = send("h1", body).await.unwrap().json().await.unwrap();
    assert!(body_wins.duplicate);
    assert_eq!(body_wins.message.id, first.message.id);

    // No retry created a second message or a second event.
    let events = board.events("after=start").await;
    assert_eq!(
        message_ids(&events.events),
        [
            first.message.id,
            other_actor.message.id,
            by_header.message.id
        ]
    );
    let threads: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=dev")
        .await;
    assert_eq!(threads.threads.len(), 3);
    board.stop().await;
}

#[tokio::test]
async fn validation_errors() {
    let board = Board::start_with(|config| config.max_message_chars = 10, None).await;
    let bad = StatusCode::BAD_REQUEST;

    for channel in ["Dev", "a//b", "a/", "/a", "a b", ""] {
        let response = board
            .post_raw("general", None, json!({"channel": channel, "text": "x"}))
            .await;
        expect_error(response, bad, codes::INVALID_CHANNEL).await;
    }
    let long_board = "b".repeat(65);
    let response = board
        .post_raw(&long_board, None, json!({"channel": "a", "text": "x"}))
        .await;
    expect_error(response, bad, codes::INVALID_BOARD).await;
    let response = board.get("/v1/boards/a%2Fb/channels").await;
    expect_error(response, bad, codes::INVALID_BOARD).await;

    for profile in ["9lives", "has_underscore", &"p".repeat(33)] {
        let response = board
            .post_raw(
                "general",
                Some(profile),
                json!({"channel": "a", "text": "x"}),
            )
            .await;
        expect_error(response, bad, codes::INVALID_PROFILE).await;
    }

    for body in [
        json!({"channel": "a", "text": ""}),
        json!({"channel": "a", "text": "  \n"}),
        json!({"text": "no target"}),
        json!({"channel": "a", "thread_id": "msg_1", "text": "two"}),
        json!({"channel": "a"}),
        json!({"channel": "a", "text": 7}),
        json!({"channel": "a", "text": "x", "idempotency_key": ""}),
    ] {
        let response = board.post_raw("general", None, body).await;
        expect_error(response, bad, codes::INVALID_REQUEST).await;
    }
    let malformed = board
        .http
        .post(board.url("/v1/boards/general/messages"))
        .header("content-type", "application/json")
        .body("{\"channel\": \"a\", \"text\": \"secret words")
        .send()
        .await
        .unwrap();
    let error = expect_error(malformed, bad, codes::INVALID_REQUEST).await;
    assert!(!error.error.message.contains("secret"));
    let not_json = board
        .http
        .post(board.url("/v1/boards/general/messages"))
        .header("content-type", "text/plain")
        .body("{\"channel\": \"a\", \"text\": \"x\"}")
        .send()
        .await
        .unwrap();
    expect_error(
        not_json,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        codes::INVALID_REQUEST,
    )
    .await;

    // The limit counts characters, not bytes.
    let ten = "é".repeat(10);
    assert_eq!(ten.len(), 20);
    board.thread("a", &ten).await;
    let eleven = board
        .post_raw(
            "general",
            None,
            json!({"channel": "a", "text": "é".repeat(11)}),
        )
        .await;
    expect_error(
        eleven,
        StatusCode::PAYLOAD_TOO_LARGE,
        codes::MESSAGE_TOO_LARGE,
    )
    .await;
    let huge = board
        .post_raw(
            "general",
            None,
            json!({"channel": "a", "text": "x".repeat(200_000)}),
        )
        .await;
    expect_error(
        huge,
        StatusCode::PAYLOAD_TOO_LARGE,
        codes::MESSAGE_TOO_LARGE,
    )
    .await;

    let missing = board
        .post_raw(
            "general",
            None,
            json!({"thread_id": "msg_nope", "text": "x"}),
        )
        .await;
    expect_error(missing, StatusCode::NOT_FOUND, codes::NOT_FOUND).await;
    let missing = board.get("/v1/boards/general/threads/msg_nope").await;
    expect_error(missing, StatusCode::NOT_FOUND, codes::NOT_FOUND).await;

    for path in [
        "/v1/boards/general/threads",
        "/v1/boards/general/threads?channel=a&limit=abc",
        "/v1/boards/general/threads?channel=a&limit=0",
        "/v1/boards/general/events?wait=soon",
        "/v1/boards/general/events?limit=0",
    ] {
        expect_error(board.get(path).await, bad, codes::INVALID_REQUEST).await;
    }
    for path in [
        "/v1/boards/general/threads?channel=A",
        "/v1/boards/general/events?channel=a//b",
        "/v1/boards/general/events/stream?channel=A/",
    ] {
        expect_error(board.get(path).await, bad, codes::INVALID_CHANNEL).await;
    }

    for path in ["/v1", "/v1/nope", "/v2/health", "/v1/boards/general"] {
        expect_error(
            board.get(path).await,
            StatusCode::NOT_FOUND,
            codes::NOT_FOUND,
        )
        .await;
    }
    let wrong_method = board
        .http
        .delete(board.url("/v1/boards/general/messages"))
        .send()
        .await
        .unwrap();
    expect_error(
        wrong_method,
        StatusCode::METHOD_NOT_ALLOWED,
        codes::INVALID_REQUEST,
    )
    .await;

    // Over-large page sizes are clamped, not refused.
    let _: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=a&limit=100000")
        .await;
    board.stop().await;
}

#[tokio::test]
async fn cursors_are_board_scoped() {
    let board = Board::start().await;
    let here = board.thread("a", "one").await;
    let there = board
        .post("other", None, json!({"channel": "a", "text": "one"}))
        .await;
    assert_ne!(here.cursor, there.cursor);

    let bad = StatusCode::BAD_REQUEST;
    let foreign = board
        .get(&format!("/v1/boards/general/events?after={}", there.cursor))
        .await;
    expect_error(foreign, bad, codes::INVALID_CURSOR).await;
    for garbage in ["garbage", "", "0", "-1", "ev__1"] {
        let response = board
            .get(&format!("/v1/boards/general/events?after={garbage}"))
            .await;
        expect_error(response, bad, codes::INVALID_CURSOR).await;
        let response = board
            .get(&format!("/v1/boards/general/events/stream?after={garbage}"))
            .await;
        expect_error(response, bad, codes::INVALID_CURSOR).await;
    }
    // A well-formed cursor for a position this board has not reached.
    let ahead = format!("{}f", here.cursor);
    let response = board
        .get(&format!("/v1/boards/general/events?after={ahead}"))
        .await;
    expect_error(response, bad, codes::INVALID_CURSOR).await;

    let resumed = board.events(&format!("after={}", here.cursor)).await;
    assert!(resumed.events.is_empty());
    assert_eq!(resumed.cursor, here.cursor);
    board.stop().await;
}

#[tokio::test]
async fn events_default_to_from_now_and_start_reads_everything() {
    let board = Board::start().await;
    let empty = board.events("after=start").await;
    assert!(empty.events.is_empty());

    let first = board.thread("a", "one").await;
    let from_now = board.events("").await;
    assert!(from_now.events.is_empty());
    assert_eq!(from_now.cursor, first.cursor);

    let second = board.thread("a", "two").await;
    let next = board.events(&format!("after={}", from_now.cursor)).await;
    assert_eq!(message_ids(&next.events), [second.message.id.as_str()]);
    assert_eq!(next.cursor, second.cursor);

    // The cursor handed out before anything was posted still works.
    let all = board.events(&format!("after={}", empty.cursor)).await;
    assert_eq!(all.events.len(), 2);
    let event = &all.events[0];
    assert_eq!(event.cursor, first.cursor);
    assert_eq!(event.kind, "message.posted");
    assert_eq!(event.board, "general");
    assert_eq!(event.channel, "a");
    assert_eq!(event.thread_id, first.message.id);
    assert_eq!(event.message_id, first.message.id);
    assert_eq!(event.by, first.message.by);
    assert_eq!(event.at, first.message.created_at);
    assert_eq!(event.prev_hash, ZEROS);
    assert_eq!(all.events[1].prev_hash, event.hash);

    // Events never carry message text.
    let raw = board.get("/v1/boards/general/events?after=start").await;
    let raw = raw.text().await.unwrap();
    assert!(!raw.contains("\"text\""));
    assert!(!raw.contains("one") && !raw.contains("two"));

    let by_thread = board
        .events(&format!("after=start&thread={}", second.message.id))
        .await;
    assert_eq!(message_ids(&by_thread.events), [second.message.id]);
    board.stop().await;
}

#[tokio::test]
async fn long_poll_returns_when_another_task_posts() {
    let board = Arc::new(Board::start().await);

    let waiter = {
        let board = Arc::clone(&board);
        tokio::spawn(async move {
            let started = Instant::now();
            let response = board.events("wait=30&channel=research/").await;
            (response, started.elapsed())
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !waiter.is_finished(),
        "the long-poll must block while nothing matches"
    );
    // A post that does not match must not end the wait.
    board.thread("chatter", "noise").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !waiter.is_finished(),
        "a filtered event must not end the wait"
    );

    let posted = board.thread("research/web", "found it").await;
    let (response, elapsed) = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .expect("the long-poll wakes on a matching post")
        .unwrap();
    assert!(elapsed < Duration::from_secs(10));
    assert_eq!(message_ids(&response.events), [posted.message.id]);
    assert_eq!(response.cursor, posted.cursor);

    // With nothing to deliver, the wait runs out and returns a usable cursor.
    let started = Instant::now();
    let timed_out = board.events("wait=1").await;
    assert!(timed_out.events.is_empty());
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert_eq!(timed_out.cursor, posted.cursor);

    let board = Arc::into_inner(board).unwrap();
    board.stop().await;
}

#[tokio::test]
async fn shutdown_releases_long_polls_and_streams() {
    let board = Board::start().await;
    let posted = board.thread("a", "one").await;
    let waiter = {
        let request = board
            .http
            .get(board.url("/v1/boards/general/events?wait=60"));
        tokio::spawn(async move {
            let response = request.send().await.unwrap();
            response.json::<EventsResponse>().await.unwrap()
        })
    };
    let mut stream = board.get("/v1/boards/general/events/stream").await;
    assert_eq!(stream.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!waiter.is_finished());

    // `stop` itself asserts that the server exits promptly.
    board.stop().await;
    let response = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .expect("shutdown ends the long-poll")
        .unwrap();
    assert!(response.events.is_empty());
    assert_eq!(response.cursor, posted.cursor);
    let end = tokio::time::timeout(Duration::from_secs(10), stream.chunk())
        .await
        .expect("shutdown ends the stream");
    assert!(matches!(end, Ok(None) | Err(_)));
}

#[tokio::test]
async fn not_from_skips_the_callers_own_posts() {
    let board = Board::start().await;
    let mine = json!({"channel": "team", "text": "from a"});
    let theirs = json!({"channel": "team", "text": "from b"});
    board.post("general", Some("a"), mine.clone()).await;
    let b1 = board.post("general", Some("b"), theirs.clone()).await;
    let a2 = board.post("general", Some("a"), mine).await;
    let plain = board.thread("team", "no profile").await;

    let seen = board.events("after=start&not_from=local/a").await;
    assert_eq!(
        message_ids(&seen.events),
        [b1.message.id.clone(), plain.message.id.clone()]
    );
    assert!(seen.events.iter().all(|event| event.by.handle != "local/a"));
    // `local` and `local/a` are different actors.
    let not_plain = board.events("after=start&not_from=local").await;
    assert_eq!(not_plain.events.len(), 3);

    // The caller's own post does not wake its wait; another actor's does.
    let board = Arc::new(board);
    let waiter = {
        let board = Arc::clone(&board);
        let after = seen.cursor.clone();
        tokio::spawn(async move {
            board
                .events(&format!("after={after}&not_from=local/a&wait=30"))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    board
        .post(
            "general",
            Some("a"),
            json!({"thread_id": a2.message.id, "text": "me again"}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!waiter.is_finished());
    let reply = board
        .post(
            "general",
            Some("b"),
            json!({"thread_id": a2.message.id, "text": "hello a"}),
        )
        .await;
    let woke = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message_ids(&woke.events), [reply.message.id]);
    Arc::into_inner(board).unwrap().stop().await;
}

#[tokio::test]
async fn cursor_resume_never_misses_or_repeats_across_filtered_events() {
    let board = Board::start().await;
    let start = board.events("").await.cursor;

    // Interleave wanted and unwanted events in uneven runs.
    let mut wanted = Vec::new();
    for round in 0..12 {
        for _ in 0..(round % 4) {
            board.thread("noise", "skip me").await;
        }
        wanted.push(board.thread("signal", "keep me").await.message.id);
        if round % 3 == 0 {
            wanted.push(board.thread("signal/deep", "keep me too").await.message.id);
        }
    }

    for limit in [1, 2, 5, 100] {
        let mut cursor = start.clone();
        let mut seen = Vec::new();
        let mut rounds = 0;
        loop {
            let page = board
                .events(&format!("after={cursor}&channel=signal/&limit={limit}"))
                .await;
            assert!(page.events.len() <= limit);
            let done = page.events.is_empty();
            seen.extend(message_ids(&page.events));
            cursor = page.cursor;
            rounds += 1;
            assert!(rounds < 100, "the loop must terminate");
            if done {
                break;
            }
        }
        assert_eq!(seen, wanted, "limit {limit}");

        // The final cursor skipped the trailing noise. Only later events follow.
        board.thread("noise", "after the loop").await;
        let quiet = board
            .events(&format!("after={cursor}&channel=signal/"))
            .await;
        assert!(quiet.events.is_empty());
        let late = board.thread("signal", "late").await;
        let resumed = board
            .events(&format!("after={}&channel=signal/", quiet.cursor))
            .await;
        assert_eq!(message_ids(&resumed.events), [late.message.id.as_str()]);
        let nothing = board
            .events(&format!("after={}&channel=signal/", resumed.cursor))
            .await;
        assert!(nothing.events.is_empty());
        wanted.push(late.message.id);
    }
    board.stop().await;
}

/// Reads server-sent events until `count` data frames have arrived.
async fn read_frames(response: &mut Response, count: usize) -> Vec<(String, Event)> {
    let mut buffer = String::new();
    let mut frames = Vec::new();
    while frames.len() < count {
        let chunk = tokio::time::timeout(Duration::from_secs(10), response.chunk())
            .await
            .expect("the stream delivers in time")
            .unwrap()
            .expect("the stream stays open");
        buffer.push_str(std::str::from_utf8(&chunk).unwrap());
        while let Some(end) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..end + 2).collect();
            let field = |name: &str| {
                frame
                    .lines()
                    .find_map(|line| line.strip_prefix(name))
                    .map(|value| value.trim_start().to_owned())
            };
            if let (Some(id), Some(data)) = (field("id:"), field("data:")) {
                frames.push((id, serde_json::from_str(&data).unwrap()));
            }
        }
    }
    frames
}

#[tokio::test]
async fn stream_delivers_events_over_sse() {
    let board = Board::start().await;
    let before = board.thread("a", "before the stream").await;

    let mut live = board
        .get("/v1/boards/general/events/stream?channel=a&not_from=local/quiet")
        .await;
    assert_eq!(live.status(), StatusCode::OK);
    let content_type = live.headers()["content-type"].to_str().unwrap().to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );

    board.thread("b", "wrong channel").await;
    board
        .post(
            "general",
            Some("quiet"),
            json!({"channel": "a", "text": "filtered actor"}),
        )
        .await;
    let posted = board.thread("a", "live").await;
    let frames = read_frames(&mut live, 1).await;
    assert_eq!(frames[0].0, posted.cursor);
    assert_eq!(frames[0].1.cursor, posted.cursor);
    assert_eq!(frames[0].1.message_id, posted.message.id);
    assert_eq!(frames[0].1.kind, "message.posted");

    // `after=start` replays the log, and Last-Event-ID resumes like `after`.
    let mut replay = board
        .get("/v1/boards/general/events/stream?after=start&channel=a&not_from=local/quiet")
        .await;
    let frames = read_frames(&mut replay, 2).await;
    assert_eq!(frames[0].1.message_id, before.message.id);
    assert_eq!(frames[1].1.message_id, posted.message.id);

    let mut resumed = board
        .http
        .get(board.url("/v1/boards/general/events/stream?channel=a&not_from=local/quiet"))
        .header("last-event-id", &before.cursor)
        .send()
        .await
        .unwrap();
    let frames = read_frames(&mut resumed, 1).await;
    assert_eq!(frames[0].1.message_id, posted.message.id);

    drop((live, replay, resumed));
    board.stop().await;
}

#[tokio::test]
async fn non_loopback_bind_is_refused() {
    for ip in ["0.0.0.0", "192.168.1.10", "::", "2001:db8::1"] {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let mut config = Config::new(&data_dir);
        config.bind = ip.parse().unwrap();
        config.port = 0;
        let error = openboard_server::bind(config.clone()).await.unwrap_err();
        assert!(matches!(error, ServeError::NonLoopbackBind(refused) if refused == config.bind));
        assert!(error.to_string().contains("loopback"), "{error}");
        assert!(!data_dir.exists(), "a refused bind must not touch the disk");
        let error = openboard_server::serve(config, async {}).await.unwrap_err();
        assert!(matches!(error, ServeError::NonLoopbackBind(_)));
    }

    // IPv6 loopback is allowed where the machine has it.
    let dir = TempDir::new().unwrap();
    let mut config = Config::new(dir.path());
    config.bind = "::1".parse().unwrap();
    config.port = 0;
    match openboard_server::bind(config).await {
        Ok(server) => assert!(server.local_addr().ip().is_loopback()),
        Err(error) => assert!(matches!(error, ServeError::Bind { .. }), "{error}"),
    }
    assert_eq!(Config::default().bind, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(Config::default().port, openboard_api::DEFAULT_PORT);
}

#[tokio::test]
async fn requests_for_a_non_loopback_host_are_refused() {
    let board = Board::start().await;
    let response = board
        .http
        .get(board.url("/v1/health"))
        .header("host", "board.example.com")
        .send()
        .await
        .unwrap();
    expect_error(response, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    let response = board
        .http
        .get(board.url("/v1/health"))
        .header("host", format!("localhost:{}", board.addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    board.stop().await;
}

#[tokio::test]
async fn a_host_that_only_starts_like_a_loopback_one_is_refused() {
    let board = Board::start().await;
    let port = board.addr.port();
    // Names an attacker can register and point at 127.0.0.1.
    for host in [
        "[::1].evil.com".to_owned(),
        format!("[::1].evil.com:{port}"),
        format!("[::1]:{port}.evil.com"),
        "[::1]evil.com".to_owned(),
        "localhost.evil.com".to_owned(),
        format!("127.0.0.1.evil.com:{port}"),
        "[::1]:".to_owned(),
        "localhost:".to_owned(),
    ] {
        for (method, path) in [
            (reqwest::Method::GET, "/v1/boards/general/channels"),
            (reqwest::Method::POST, "/v1/boards/general/messages"),
        ] {
            let response = board
                .http
                .request(method, board.url(path))
                .header("host", &host)
                .json(&serde_json::json!({"channel": "dev", "text": "rebound"}))
                .send()
                .await
                .unwrap();
            expect_error(response, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
        }
    }
    for host in [format!("[::1]:{port}"), "[::1]".to_owned()] {
        let response = board
            .http
            .get(board.url("/v1/health"))
            .header("host", &host)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{host}");
    }
    // Nothing was written by the refused posts.
    let channels: openboard_api::ChannelList = board
        .http
        .get(board.url("/v1/boards/general/channels"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(channels.channels.is_empty());
    board.stop().await;
}

fn open_db(dir: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join(DB_FILE_NAME)).unwrap()
}

#[tokio::test]
async fn verify_log_passes_then_catches_tampering() {
    let board = Board::start().await;
    for n in 0..4 {
        board.thread("a", &format!("message {n}")).await;
    }
    board
        .post(
            "second",
            Some("fx"),
            json!({"channel": "b", "text": "elsewhere"}),
        )
        .await;
    let data_dir = board.data_dir();

    // Healthy, and checkable beside a running server.
    let report = verify_log(&data_dir).unwrap();
    assert!(report.is_ok(), "{report:?}");
    let counts: Vec<(&str, u64)> = report
        .boards
        .iter()
        .map(|b| (b.board.as_str(), b.verified_events))
        .collect();
    assert_eq!(counts, [("general", 4), ("second", 1)]);
    let _dir = board.stop().await;
    assert!(verify_log(&data_dir).unwrap().is_ok());

    // The schema refuses edits to the log outright.
    let db = open_db(&data_dir);
    assert!(db
        .execute("UPDATE events SET channel = 'x' WHERE seq = 3", [])
        .is_err());
    assert!(db.execute("DELETE FROM events WHERE seq = 3", []).is_err());

    // Someone with file access removes that guard and rewrites an entry.
    db.execute_batch(
        "DROP TRIGGER events_are_append_only_update;
         DROP TRIGGER events_are_append_only_delete;
         UPDATE events SET channel = 'elsewhere'
         WHERE seq = 3 AND board_id = (SELECT id FROM boards WHERE name = 'general');",
    )
    .unwrap();
    let report = verify_log(&data_dir).unwrap();
    assert!(!report.is_ok());
    let general = &report.boards[0];
    assert_eq!(general.verified_events, 2);
    let broken = general.broken.as_ref().unwrap();
    assert_eq!(broken.position, 3);
    assert!(broken.reason.contains("hash"), "{}", broken.reason);
    assert!(
        report.boards[1].broken.is_none(),
        "other boards still verify"
    );

    // Recomputing nothing but restoring the field heals it.
    db.execute(
        "UPDATE events SET channel = 'a' WHERE seq = 3 AND channel = 'elsewhere'",
        [],
    )
    .unwrap();
    assert!(verify_log(&data_dir).unwrap().is_ok());

    // Rewriting who wrote an entry breaks the chain from that actor's first event.
    db.execute(
        "UPDATE actors SET handle = 'someone-else' WHERE profile IS NULL",
        [],
    )
    .unwrap();
    let report = verify_log(&data_dir).unwrap();
    assert_eq!(report.boards[0].broken.as_ref().unwrap().position, 1);
    db.execute(
        "UPDATE actors SET handle = 'local' WHERE profile IS NULL",
        [],
    )
    .unwrap();

    // Dropping the newest entry is caught by the recorded head.
    db.execute("DELETE FROM events WHERE seq = 4", []).unwrap();
    let report = verify_log(&data_dir).unwrap();
    let broken = report.boards[0].broken.as_ref().unwrap();
    assert_eq!((report.boards[0].verified_events, broken.position), (3, 4));

    // Dropping one from the middle is caught as a gap.
    db.execute("DELETE FROM events WHERE seq = 2", []).unwrap();
    let report = verify_log(&data_dir).unwrap();
    let broken = report.boards[0].broken.as_ref().unwrap();
    assert_eq!((report.boards[0].verified_events, broken.position), (1, 2));
    assert!(broken.reason.contains("missing"));

    // No database is an error, not a pass.
    let empty = TempDir::new().unwrap();
    assert!(verify_log(empty.path()).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_posts_produce_a_gap_free_chained_log() {
    const POSTERS: usize = 50;
    let board = Arc::new(Board::start().await);
    let mut tasks = Vec::new();
    for n in 0..POSTERS {
        let board = Arc::clone(&board);
        tasks.push(tokio::spawn(async move {
            let profile = format!("worker-{}", n % 7);
            let body = json!({"channel": format!("load/{}", n % 3), "text": format!("post {n}")});
            board.post("general", Some(&profile), body).await
        }));
    }
    let mut posted = Vec::new();
    for task in tasks {
        posted.push(task.await.unwrap());
    }

    let log = board.events("after=start&limit=1000").await;
    assert_eq!(log.events.len(), POSTERS);
    let mut prev = ZEROS.to_owned();
    for event in &log.events {
        assert_eq!(event.prev_hash, prev, "each entry links to the one before");
        assert_eq!(event.hash.len(), 64);
        prev = event.hash.clone();
    }
    let unique = |mut values: Vec<String>| {
        values.sort();
        values.dedup();
        values.len()
    };
    assert_eq!(
        unique(log.events.iter().map(|e| e.cursor.clone()).collect()),
        POSTERS
    );
    assert_eq!(
        unique(log.events.iter().map(|e| e.hash.clone()).collect()),
        POSTERS
    );
    // Every post has exactly one event, at the cursor the post returned.
    for response in &posted {
        let event = log
            .events
            .iter()
            .find(|event| event.message_id == response.message.id)
            .expect("every post has an event");
        assert_eq!(event.cursor, response.cursor);
        assert_eq!(event.by, response.message.by);
    }
    assert_eq!(log.cursor, log.events.last().unwrap().cursor);

    let data_dir = board.data_dir();
    let report = verify_log(&data_dir).unwrap();
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(report.boards[0].verified_events, POSTERS as u64);

    // Positions run 1..=50 with no gap, and each event shares its message's row.
    let db = open_db(&data_dir);
    let (count, min, max): (i64, i64, i64) = db
        .query_row(
            "SELECT count(*), min(seq), max(seq) FROM events",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((count, min, max), (POSTERS as i64, 1, POSTERS as i64));
    let unmatched: i64 = db
        .query_row(
            "SELECT count(*) FROM events e
             LEFT JOIN messages m ON m.id = e.message_id AND m.seq = e.seq
             WHERE m.id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unmatched, 0);
    drop(db);
    Arc::into_inner(board).unwrap().stop().await;
}

/// Denies everything under the `secret` channel subtree and the creation of
/// the board named `locked`. Everything else is allowed.
struct NoSecrets;

impl Policy for NoSecrets {
    fn decide(
        &self,
        _principal: &Principal,
        action: Action,
        board: Option<&str>,
        channel: Option<&str>,
    ) -> Decision {
        let secret = channel.is_some_and(|path| path == "secret" || path.starts_with("secret/"));
        let locked = action == Action::CreateBoard && board == Some("locked");
        if secret || locked {
            Decision::Deny
        } else {
            Decision::Allow
        }
    }
}

/// Allows reads and nothing else.
struct ReadOnly;

impl Policy for ReadOnly {
    fn decide(&self, _: &Principal, action: Action, _: Option<&str>, _: Option<&str>) -> Decision {
        match action {
            Action::Read | Action::CreateBoard => Decision::Allow,
            _ => Decision::Deny,
        }
    }
}

#[tokio::test]
async fn a_denying_policy_is_enforced_on_every_path() {
    // Seed a board with the open policy, including a channel that the
    // stricter policy will hide.
    let open = Board::start().await;
    let hidden = open.thread("secret/plans", "hidden").await;
    let visible = open.thread("public", "visible").await;
    open.post(
        "general",
        None,
        json!({"thread_id": hidden.message.id, "text": "hidden reply"}),
    )
    .await;
    let dir = open.stop().await;
    let data_dir = dir.path().join("data");

    let board = Board::start_with(
        |config| config.data_dir = data_dir.clone(),
        Some(Arc::new(NoSecrets)),
    )
    .await;
    let forbidden = StatusCode::FORBIDDEN;

    // Writes.
    let response = board
        .post_raw(
            "general",
            None,
            json!({"channel": "secret/new", "text": "x"}),
        )
        .await;
    expect_error(response, forbidden, codes::FORBIDDEN).await;
    let response = board
        .post_raw(
            "general",
            None,
            json!({"thread_id": hidden.message.id, "text": "x"}),
        )
        .await;
    expect_error(response, StatusCode::NOT_FOUND, codes::NOT_FOUND).await;
    let response = board
        .post_raw("locked", None, json!({"channel": "public", "text": "x"}))
        .await;
    expect_error(response, forbidden, codes::FORBIDDEN).await;
    // Reading the board that could not be created finds nothing, and the
    // refused post left nothing behind.
    expect_error(
        board.get("/v1/boards/locked/channels").await,
        StatusCode::NOT_FOUND,
        codes::NOT_FOUND,
    )
    .await;
    assert_eq!(board_names(&data_dir), ["general"]);

    // Reads.
    let response = board
        .get(&format!("/v1/boards/general/threads/{}", hidden.message.id))
        .await;
    expect_error(response, StatusCode::NOT_FOUND, codes::NOT_FOUND).await;
    let response = board
        .get("/v1/boards/general/threads?channel=secret/plans")
        .await;
    expect_error(response, forbidden, codes::FORBIDDEN).await;
    let response = board
        .get("/v1/boards/general/events?channel=secret/plans")
        .await;
    expect_error(response, forbidden, codes::FORBIDDEN).await;
    let response = board
        .get("/v1/boards/general/events/stream?channel=secret/plans")
        .await;
    expect_error(response, forbidden, codes::FORBIDDEN).await;

    // Listings and subtree queries leak nothing from the hidden channel.
    let channels: ChannelList = board.get_json("/v1/boards/general/channels").await;
    let paths: Vec<&str> = channels.channels.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["public"]);
    let subtree: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=secret/")
        .await;
    assert!(subtree.threads.is_empty());

    // Events: the two hidden events are filtered from the poll and the
    // stream, and the cursor still moves past them.
    let log = board.events("after=start").await;
    assert_eq!(message_ids(&log.events), [visible.message.id.as_str()]);
    let paged = board.events("after=start&limit=1").await;
    assert_eq!(
        paged.events.len()
            + board
                .events(&format!("after={}&limit=1", paged.cursor))
                .await
                .events
                .len(),
        1
    );
    let subtree = board.events("after=start&channel=secret/").await;
    assert!(subtree.events.is_empty());

    let mut stream = board
        .get("/v1/boards/general/events/stream?after=start")
        .await;
    let later = board.thread("public", "later").await;
    let frames = read_frames(&mut stream, 2).await;
    let streamed: Vec<&str> = frames.iter().map(|(_, e)| e.message_id.as_str()).collect();
    assert_eq!(
        streamed,
        [visible.message.id.as_str(), later.message.id.as_str()]
    );
    drop(stream);
    board.stop().await;

    // A policy without `post` or `create` refuses writes but still reads.
    let board = Board::start_with(
        |config| config.data_dir = data_dir.clone(),
        Some(Arc::new(ReadOnly)),
    )
    .await;
    let response = board
        .post_raw("general", None, json!({"channel": "public", "text": "x"}))
        .await;
    expect_error(response, forbidden, codes::FORBIDDEN).await;
    let response = board
        .post_raw(
            "general",
            None,
            json!({"thread_id": visible.message.id, "text": "x"}),
        )
        .await;
    expect_error(response, forbidden, codes::FORBIDDEN).await;
    expect_error(board.get("/v1/health").await, forbidden, codes::FORBIDDEN).await;
    let threads: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=public")
        .await;
    assert_eq!(threads.threads.len(), 2);
    board.stop().await;
    assert!(verify_log(&data_dir).unwrap().is_ok());
    drop(dir);
}

#[tokio::test]
async fn a_post_whose_client_gave_up_still_wakes_waiters() {
    let board = Arc::new(Board::start().await);
    let waiter = {
        let board = Arc::clone(&board);
        tokio::spawn(async move { board.events("wait=30").await })
    };
    let mut stream = board.get("/v1/boards/general/events/stream").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished());

    // Hold the database's write lock so the post stalls inside the store,
    // and let the client give up while it is stalled.
    let db = open_db(&board.data_dir());
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let impatient = Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let body = json!({"channel": "a", "text": "slow", "idempotency_key": "slow-1"});
    let gave_up = impatient
        .post(board.url("/v1/boards/general/messages"))
        .json(&body)
        .send()
        .await
        .unwrap_err();
    assert!(gave_up.is_timeout(), "{gave_up}");
    drop(impatient);
    // Give the server time to see the closed connection.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished());
    db.execute_batch("ROLLBACK").unwrap();
    drop(db);

    // The post commits once the lock is free, and both waiters hear of it.
    let woke = tokio::time::timeout(Duration::from_secs(4), waiter)
        .await
        .expect("the long-poll wakes although the posting client is gone")
        .unwrap();
    assert_eq!(woke.events.len(), 1);
    let frames = read_frames(&mut stream, 1).await;
    assert_eq!(frames[0].1.message_id, woke.events[0].message_id);

    // The retry finds the message that was stored.
    let retry = board.post("general", None, body).await;
    assert!(retry.duplicate);
    assert_eq!(retry.message.id, woke.events[0].message_id);
    drop(stream);
    Arc::into_inner(board).unwrap().stop().await;
}

#[tokio::test]
async fn last_event_id_wins_over_a_stale_after() {
    let board = Board::start().await;
    let first = board.thread("a", "one").await;
    let second = board.thread("a", "two").await;
    let third = board.thread("a", "three").await;

    // A reconnecting EventSource repeats its original URL, `after` included,
    // and adds the ID of the last event it saw.
    for stale in ["start", first.cursor.as_str()] {
        let mut resumed = board
            .http
            .get(board.url(&format!("/v1/boards/general/events/stream?after={stale}")))
            .header("last-event-id", &second.cursor)
            .send()
            .await
            .unwrap();
        assert_eq!(resumed.status(), StatusCode::OK);
        let frames = read_frames(&mut resumed, 1).await;
        assert_eq!(frames[0].1.message_id, third.message.id, "after={stale}");
    }

    // Without the header, `after` still applies.
    let mut plain = board
        .get(&format!(
            "/v1/boards/general/events/stream?after={}",
            first.cursor
        ))
        .await;
    let frames = read_frames(&mut plain, 1).await;
    assert_eq!(frames[0].1.message_id, second.message.id);
    drop(plain);
    board.stop().await;
}

fn board_names(dir: &std::path::Path) -> Vec<String> {
    let db = open_db(dir);
    let mut stmt = db.prepare("SELECT name FROM boards ORDER BY id").unwrap();
    let names = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    names
}

#[tokio::test]
async fn boards_are_created_only_by_a_successful_post() {
    let board = Board::start().await;
    let data_dir = board.data_dir();

    // The default board exists from the start, so reads and waits work on a
    // fresh data directory.
    assert_eq!(board_names(&data_dir), ["general"]);
    let channels: ChannelList = board.get_json("/v1/boards/general/channels").await;
    assert!(channels.channels.is_empty());
    let threads: ThreadList = board.get_json("/v1/boards/general/threads?channel=a").await;
    assert!(threads.threads.is_empty());
    assert!(board.events("wait=1").await.events.is_empty());
    let stream = board.get("/v1/boards/general/events/stream").await;
    assert_eq!(stream.status(), StatusCode::OK);
    drop(stream);

    // Reading a board that does not exist is an error and creates nothing.
    for path in [
        "/v1/boards/typo/channels",
        "/v1/boards/typo/threads?channel=a",
        "/v1/boards/typo/threads/msg_nope",
        "/v1/boards/typo/events",
        "/v1/boards/typo/events?after=start&wait=1",
        "/v1/boards/typo/events/stream",
    ] {
        let error = expect_error(
            board.get(path).await,
            StatusCode::NOT_FOUND,
            codes::NOT_FOUND,
        )
        .await;
        assert!(error.error.message.contains("board"), "{path}: {error:?}");
        assert!(
            error.error.next.unwrap().contains("first post"),
            "GET {path}"
        );
    }

    // Neither does a post that is refused.
    let not_json = board
        .http
        .post(board.url("/v1/boards/typo/messages"))
        .header("content-type", "text/plain")
        .body("{\"channel\": \"a\", \"text\": \"x\"}")
        .send()
        .await
        .unwrap();
    expect_error(
        not_json,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        codes::INVALID_REQUEST,
    )
    .await;
    let bad = StatusCode::BAD_REQUEST;
    for body in [
        json!({"channel": "a", "text": ""}),
        json!({"text": "no target"}),
        json!({"channel": "a", "text": "x", "idempotency_key": ""}),
    ] {
        let response = board.post_raw("typo", None, body).await;
        expect_error(response, bad, codes::INVALID_REQUEST).await;
    }
    let response = board
        .post_raw(
            "typo",
            None,
            json!({"channel": "Not A Channel", "text": "x"}),
        )
        .await;
    expect_error(response, bad, codes::INVALID_CHANNEL).await;
    let response = board
        .post_raw("typo", Some("9lives"), json!({"channel": "a", "text": "x"}))
        .await;
    expect_error(response, bad, codes::INVALID_PROFILE).await;
    let response = board
        .post_raw("typo", None, json!({"thread_id": "msg_nope", "text": "x"}))
        .await;
    expect_error(response, StatusCode::NOT_FOUND, codes::NOT_FOUND).await;
    assert_eq!(board_names(&data_dir), ["general"]);

    // An invalid name is still reported as such.
    let response = board.get("/v1/boards/a%2Fb/events").await;
    expect_error(response, bad, codes::INVALID_BOARD).await;
    let response = board
        .post_raw("a%2Fb", None, json!({"channel": "a", "text": "x"}))
        .await;
    expect_error(response, bad, codes::INVALID_BOARD).await;

    // The first valid post creates the board, with the spelling it used.
    let posted = board
        .post("Fresh", None, json!({"channel": "a", "text": "hello"}))
        .await;
    assert_eq!(posted.message.board, "Fresh");
    assert_eq!(board_names(&data_dir), ["general", "Fresh"]);
    let channels: ChannelList = board.get_json("/v1/boards/fresh/channels").await;
    assert_eq!(channels.channels.len(), 1);
    let log: EventsResponse = board.get_json("/v1/boards/fresh/events?after=start").await;
    assert_eq!(message_ids(&log.events), [posted.message.id.as_str()]);
    assert_eq!(log.cursor, posted.cursor);
    assert_eq!(log.events[0].prev_hash, ZEROS);
    assert!(verify_log(&data_dir).unwrap().is_ok());
    board.stop().await;
}

#[tokio::test]
async fn a_second_server_on_the_same_data_directory_is_refused() {
    let board = Board::start().await;
    board.thread("a", "one").await;
    let data_dir = board.data_dir();
    let mut config = Config::new(&data_dir);
    config.port = 0;

    let error = openboard_server::bind(config.clone()).await.unwrap_err();
    assert!(
        matches!(&error, ServeError::DataDirInUse(dir) if *dir == data_dir),
        "{error:?}"
    );
    assert!(error.to_string().contains("already"), "{error}");
    // The log check takes no lock and still runs beside the live server.
    assert!(verify_log(&data_dir).unwrap().is_ok());
    // The refused start left the running board intact.
    board.thread("a", "two").await;

    let _dir = board.stop().await;
    let again = openboard_server::bind(config.clone()).await.unwrap();
    let error = openboard_server::bind(config.clone()).await.unwrap_err();
    assert!(matches!(error, ServeError::DataDirInUse(_)), "{error:?}");
    drop(again);
    openboard_server::bind(config).await.unwrap();
}

#[tokio::test]
async fn the_events_thread_filter_accepts_any_message_id_in_the_thread() {
    let board = Board::start().await;
    let root = board.thread("a", "root").await;
    let other = board.thread("a", "another thread").await;
    let reply = board
        .post(
            "general",
            None,
            json!({"thread_id": root.message.id, "text": "reply"}),
        )
        .await;
    let expected = [root.message.id.clone(), reply.message.id.clone()];

    for id in [&root.message.id, &reply.message.id] {
        let log = board.events(&format!("after=start&thread={id}")).await;
        assert_eq!(message_ids(&log.events), expected, "thread={id}");
        let mut stream = board
            .get(&format!(
                "/v1/boards/general/events/stream?after=start&thread={id}"
            ))
            .await;
        let frames = read_frames(&mut stream, 2).await;
        assert_eq!(frames[1].1.message_id, reply.message.id);
    }
    let log = board
        .events(&format!("after=start&thread={}", other.message.id))
        .await;
    assert_eq!(message_ids(&log.events), [other.message.id.as_str()]);

    // An ID that names no message is an error, not an endless silent wait.
    for path in [
        "/v1/boards/general/events?thread=msg_nope",
        "/v1/boards/general/events/stream?thread=msg_nope",
    ] {
        expect_error(
            board.get(path).await,
            StatusCode::NOT_FOUND,
            codes::NOT_FOUND,
        )
        .await;
    }
    board.stop().await;
}
