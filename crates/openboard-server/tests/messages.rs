//! End-to-end tests of the message features on a local board: edit, delete,
//! labels, reactions, mentions, the event filters, rendered HTML, the board
//! list and the static pages. A real server on port 0, exercised over HTTP.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use openboard_api::{
    codes, event_types, ActorKind, BoardList, ErrorBody, Event, EventsResponse, Message,
    PostMessageResponse, Thread, ThreadList, PROFILE_HEADER,
};
use openboard_server::authz::{Action, Decision, Policy, Principal};
use openboard_server::{verify_log, Config, ServeError, DB_FILE_NAME};
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
        Self::start_with(|_| {}, None).await
    }

    async fn start_with(tweak: impl FnOnce(&mut Config), policy: Option<Arc<dyn Policy>>) -> Self {
        Self::start_in(TempDir::new().unwrap(), tweak, policy).await
    }

    async fn start_in(
        dir: TempDir,
        tweak: impl FnOnce(&mut Config),
        policy: Option<Arc<dyn Policy>>,
    ) -> Self {
        // The network tests in this crate enable reqwest's TLS support,
        // which then wants a crypto provider even for plain HTTP clients.
        let _ = rustls::crypto::ring::default_provider().install_default();
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

    fn data_dir(&self) -> PathBuf {
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

    /// Sends a request, as `profile` when one is given, with a JSON body
    /// when one is given.
    async fn send(
        &self,
        method: Method,
        path: &str,
        profile: Option<&str>,
        body: Option<Value>,
    ) -> Response {
        let mut request = self.http.request(method, self.url(path));
        if let Some(profile) = profile {
            request = request.header(PROFILE_HEADER, profile);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.unwrap()
    }

    async fn get(&self, path: &str) -> Response {
        self.send(Method::GET, path, None, None).await
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        let response = self.get(path).await;
        assert_eq!(response.status(), StatusCode::OK, "GET {path}");
        response.json().await.unwrap()
    }

    async fn post_as(&self, profile: Option<&str>, body: Value) -> PostMessageResponse {
        let response = self
            .send(
                Method::POST,
                "/v1/boards/general/messages",
                profile,
                Some(body),
            )
            .await;
        assert!(response.status().is_success(), "{}", response.status());
        response.json().await.unwrap()
    }

    async fn thread(&self, channel: &str, text: &str) -> Message {
        self.post_as(None, json!({"channel": channel, "text": text}))
            .await
            .message
    }

    async fn reply(&self, thread_id: &str, text: &str) -> Message {
        self.post_as(None, json!({"thread_id": thread_id, "text": text}))
            .await
            .message
    }

    async fn edit_raw(&self, id: &str, profile: Option<&str>, body: Value) -> Response {
        let path = format!("/v1/boards/general/messages/{id}");
        self.send(Method::PATCH, &path, profile, Some(body)).await
    }

    async fn edit(&self, id: &str, text: &str) -> Message {
        message(self.edit_raw(id, None, json!({ "text": text })).await).await
    }

    async fn delete_raw(&self, id: &str, profile: Option<&str>, query: &str) -> Response {
        let path = format!("/v1/boards/general/messages/{id}{query}");
        self.send(Method::DELETE, &path, profile, None).await
    }

    async fn delete(&self, id: &str) -> Message {
        message(self.delete_raw(id, None, "").await).await
    }

    async fn labels_raw(&self, id: &str, body: Value) -> Response {
        let path = format!("/v1/boards/general/messages/{id}/labels");
        self.send(Method::POST, &path, None, Some(body)).await
    }

    async fn labels(&self, id: &str, body: Value) -> Message {
        message(self.labels_raw(id, body).await).await
    }

    async fn react_raw(
        &self,
        method: Method,
        id: &str,
        name: &str,
        profile: Option<&str>,
    ) -> Response {
        let path = format!("/v1/boards/general/messages/{id}/reactions/{name}");
        self.send(method, &path, profile, None).await
    }

    async fn react(&self, id: &str, name: &str) -> Message {
        message(self.react_raw(Method::PUT, id, name, None).await).await
    }

    async fn unreact(&self, id: &str, name: &str) -> Message {
        message(self.react_raw(Method::DELETE, id, name, None).await).await
    }

    async fn read(&self, id: &str) -> Thread {
        self.get_json(&format!("/v1/boards/general/threads/{id}"))
            .await
    }

    async fn events(&self, query: &str) -> EventsResponse {
        self.get_json(&format!("/v1/boards/general/events?{query}"))
            .await
    }

    async fn events_as(&self, profile: &str, query: &str) -> EventsResponse {
        let path = format!("/v1/boards/general/events?{query}");
        let response = self.send(Method::GET, &path, Some(profile), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    /// Every event type in the log, oldest first.
    async fn kinds(&self) -> Vec<String> {
        let log = self.events("after=start&limit=1000").await;
        log.events.into_iter().map(|event| event.kind).collect()
    }
}

async fn message(response: Response) -> Message {
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

async fn expect_error(response: Response, status: StatusCode, code: &str) -> ErrorBody {
    assert_eq!(response.status(), status, "status for expected {code}");
    let body: ErrorBody = response.json().await.unwrap();
    assert_eq!(body.error.code, code);
    assert!(!body.error.message.is_empty());
    body
}

fn kinds_of(events: &[Event]) -> Vec<&str> {
    events.iter().map(|event| event.kind.as_str()).collect()
}

fn open_db(dir: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join(DB_FILE_NAME)).unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The raw bytes of the database file and of its write-ahead log.
fn raw_storage(data_dir: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(data_dir.join(DB_FILE_NAME)).unwrap();
    let wal = data_dir.join(format!("{DB_FILE_NAME}-wal"));
    if wal.exists() {
        bytes.extend(std::fs::read(wal).unwrap());
    }
    bytes
}

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

// ---------------------------------------------------------------------------
// Edit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_replaces_the_text_and_bumps_the_revision() {
    let board = Board::start().await;
    let root = board.thread("dev", "first draft").await;
    assert_eq!(root.revision, 1);
    assert_eq!(root.edited_at, None);
    let reply = board.reply(&root.id, "a reply").await;
    let before = board.events("").await.cursor;

    let edited = board.edit(&root.id, "second draft").await;
    assert_eq!(edited.id, root.id);
    assert_eq!(edited.text, "second draft");
    assert_eq!(edited.revision, 2);
    assert_eq!(edited.created_at, root.created_at);
    assert_eq!(edited.by, root.by);
    let edited_at = edited.edited_at.clone().expect("edited_at is set");
    assert!(edited_at >= root.created_at && edited_at.ends_with('Z'));
    assert_eq!(edited.deleted_at, None);
    assert_eq!(edited.html, None);

    // The read shows the new text, and the thread keeps its place.
    let thread = board.read(&root.id).await;
    assert_eq!(thread.root, edited);
    assert_eq!(thread.replies, vec![reply.clone()]);

    // One `message.edited` event, by the editor, about the message.
    let log = board.events(&format!("after={before}")).await;
    assert_eq!(kinds_of(&log.events), [event_types::MESSAGE_EDITED]);
    let event = &log.events[0];
    assert_eq!(event.message_id, root.id);
    assert_eq!(event.thread_id, root.id);
    assert_eq!(event.channel, "dev");
    assert_eq!(event.by, root.by);

    // A reply is edited the same way.
    let edited_reply = board.edit(&reply.id, "a better reply").await;
    assert_eq!(edited_reply.revision, 2);
    assert_eq!(edited_reply.thread_id, root.id);
    assert_eq!(board.read(&root.id).await.replies[0].text, "a better reply");

    // The revision goes up by one each time.
    assert_eq!(board.edit(&root.id, "third draft").await.revision, 3);
    board.stop().await;
}

#[tokio::test]
async fn a_conditional_edit_applies_only_at_the_named_revision() {
    let board = Board::start().await;
    let root = board.thread("dev", "one").await;
    let before = board.events("").await.cursor;

    let stale = board
        .edit_raw(&root.id, None, json!({"text": "two", "revision": 7}))
        .await;
    let error = expect_error(stale, StatusCode::CONFLICT, codes::REVISION_CONFLICT).await;
    assert!(error.error.next.is_some());
    // Nothing changed and nothing was announced.
    assert_eq!(board.read(&root.id).await.root, root);
    assert!(board
        .events(&format!("after={before}"))
        .await
        .events
        .is_empty());

    let current = board
        .edit_raw(&root.id, None, json!({"text": "two", "revision": 1}))
        .await;
    assert_eq!(message(current).await.revision, 2);
    // The revision it named is now stale.
    let again = board
        .edit_raw(&root.id, None, json!({"text": "three", "revision": 1}))
        .await;
    expect_error(again, StatusCode::CONFLICT, codes::REVISION_CONFLICT).await;
    assert_eq!(board.read(&root.id).await.root.text, "two");
    board.stop().await;
}

#[tokio::test]
async fn only_the_author_actor_may_edit_or_delete() {
    let board = Board::start().await;
    let plain = board.thread("dev", "written with no profile").await;
    let profiled = board
        .post_as(
            Some("fx"),
            json!({"channel": "dev", "text": "written as fx"}),
        )
        .await
        .message;
    let forbidden = StatusCode::FORBIDDEN;

    // The same principal under another profile is another actor.
    for (id, profile) in [
        (&plain.id, Some("fx")),
        (&profiled.id, None),
        (&profiled.id, Some("fy")),
    ] {
        let response = board
            .edit_raw(id, profile, json!({"text": "mine now"}))
            .await;
        expect_error(response, forbidden, codes::FORBIDDEN).await;
        let response = board.delete_raw(id, profile, "").await;
        expect_error(response, forbidden, codes::FORBIDDEN).await;
    }
    assert_eq!(board.read(&plain.id).await.root, plain);
    assert_eq!(board.read(&profiled.id).await.root, profiled);
    assert_eq!(
        board.kinds().await,
        [event_types::MESSAGE_POSTED, event_types::MESSAGE_POSTED]
    );

    // Each author can.
    let response = board
        .edit_raw(&profiled.id, Some("fx"), json!({"text": "edited as fx"}))
        .await;
    assert_eq!(message(response).await.text, "edited as fx");
    let response = board.delete_raw(&profiled.id, Some("fx"), "").await;
    assert!(message(response).await.deleted_at.is_some());
    assert!(board.delete(&plain.id).await.deleted_at.is_some());

    // The rule holds for a tombstone too: not the author, not allowed.
    let response = board.delete_raw(&plain.id, Some("fx"), "").await;
    expect_error(response, forbidden, codes::FORBIDDEN).await;
    board.stop().await;
}

#[tokio::test]
async fn edit_validation_errors() {
    let board = Board::start_with(|config| config.max_message_chars = 10, None).await;
    let root = board.thread("dev", "short").await;
    let bad = StatusCode::BAD_REQUEST;

    for text in ["", "   ", "\n\t"] {
        let response = board
            .edit_raw(&root.id, None, json!({ "text": text }))
            .await;
        expect_error(response, bad, codes::INVALID_REQUEST).await;
    }
    // The limit is the one a post has, counted in characters.
    let response = board
        .edit_raw(&root.id, None, json!({"text": "é".repeat(11)}))
        .await;
    let error = expect_error(
        response,
        StatusCode::PAYLOAD_TOO_LARGE,
        codes::MESSAGE_TOO_LARGE,
    )
    .await;
    assert!(!error.error.message.contains('é'));
    assert_eq!(board.edit(&root.id, &"é".repeat(10)).await.revision, 2);
    let huge = board
        .edit_raw(&root.id, None, json!({"text": "x".repeat(200_000)}))
        .await;
    expect_error(
        huge,
        StatusCode::PAYLOAD_TOO_LARGE,
        codes::MESSAGE_TOO_LARGE,
    )
    .await;

    for body in [
        json!({}),
        json!({"text": 5}),
        json!({"text": "x", "revision": "1"}),
    ] {
        let response = board.edit_raw(&root.id, None, body).await;
        expect_error(response, bad, codes::INVALID_REQUEST).await;
    }
    // The error for malformed JSON never quotes the body.
    let malformed = board
        .http
        .patch(board.url(&format!("/v1/boards/general/messages/{}", root.id)))
        .header("content-type", "application/json")
        .body("{\"text\": \"a secret draft")
        .send()
        .await
        .unwrap();
    let error = expect_error(malformed, bad, codes::INVALID_REQUEST).await;
    assert!(!error.error.message.contains("secret"));
    // A form cannot edit.
    let form = board
        .http
        .patch(board.url(&format!("/v1/boards/general/messages/{}", root.id)))
        .header("content-type", "text/plain")
        .body("{\"text\": \"x\"}")
        .send()
        .await
        .unwrap();
    expect_error(
        form,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        codes::INVALID_REQUEST,
    )
    .await;

    // No such message, on this board or another, and no such board.
    let missing = StatusCode::NOT_FOUND;
    let response = board.edit_raw("msg_nope", None, json!({"text": "x"})).await;
    expect_error(response, missing, codes::NOT_FOUND).await;
    board
        .send(
            Method::POST,
            "/v1/boards/other/messages",
            None,
            Some(json!({"channel": "a", "text": "x"})),
        )
        .await;
    for path in [
        format!("/v1/boards/other/messages/{}", root.id),
        format!("/v1/boards/nowhere/messages/{}", root.id),
    ] {
        let response = board
            .send(Method::PATCH, &path, None, Some(json!({"text": "x"})))
            .await;
        expect_error(response, missing, codes::NOT_FOUND).await;
        let response = board.send(Method::DELETE, &path, None, None).await;
        expect_error(response, missing, codes::NOT_FOUND).await;
    }
    // Nothing above created the board it named.
    let boards: BoardList = board.get_json("/v1/boards").await;
    let names: Vec<&str> = boards.boards.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["general", "other"]);

    let response = board.delete_raw(&root.id, None, "?revision=abc").await;
    expect_error(response, bad, codes::INVALID_REQUEST).await;
    let response = board
        .send(
            Method::PUT,
            &format!("/v1/boards/general/messages/{}", root.id),
            None,
            Some(json!({"text": "x"})),
        )
        .await;
    expect_error(
        response,
        StatusCode::METHOD_NOT_ALLOWED,
        codes::INVALID_REQUEST,
    )
    .await;
    board.stop().await;
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_leaves_a_tombstone() {
    let board = Board::start().await;
    let root = board.thread("dev", "to be removed, cc @local").await;
    let reply = board.reply(&root.id, "a reply that stays").await;
    board.edit(&root.id, "to be removed soon, cc @local").await;
    board
        .labels(&root.id, json!({"set": {"status": "open"}}))
        .await;
    let live = board.react(&root.id, "thumbsup").await;
    assert_eq!(live.revision, 2);
    assert_eq!(live.labels.len(), 1);
    assert_eq!(live.reactions.len(), 1);
    assert_eq!(live.mentions.len(), 1);
    let before = board.events("").await.cursor;

    let tombstone = board.delete(&root.id).await;
    assert_eq!(tombstone.id, root.id);
    assert_eq!(tombstone.thread_id, root.id);
    assert_eq!(tombstone.channel, "dev");
    assert_eq!(tombstone.by, root.by);
    assert_eq!(tombstone.created_at, root.created_at);
    assert_eq!(tombstone.edited_at, live.edited_at);
    assert_eq!(tombstone.revision, 3);
    assert!(tombstone.deleted_at.is_some());
    assert_eq!(tombstone.text, "");
    assert!(tombstone.labels.is_empty());
    assert!(tombstone.reactions.is_empty());
    assert!(tombstone.mentions.is_empty());

    // The thread stays where it was, with its reply under the tombstone.
    let thread = board.read(&root.id).await;
    assert_eq!(thread.root, tombstone);
    assert_eq!(thread.replies, vec![reply.clone()]);
    let list: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=dev")
        .await;
    assert_eq!(list.threads.len(), 1);
    assert_eq!(list.threads[0].root, tombstone);
    assert_eq!(list.threads[0].reply_count, 1);
    // The label went with the message.
    let labelled: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=dev&label=status")
        .await;
    assert!(labelled.threads.is_empty());

    let log = board.events(&format!("after={before}")).await;
    assert_eq!(kinds_of(&log.events), [event_types::MESSAGE_DELETED]);
    assert_eq!(log.events[0].message_id, root.id);
    assert_eq!(log.events[0].by, root.by);

    // Deleting again succeeds and changes nothing, whatever revision it
    // names: a retry of a conditional delete must not fail.
    for query in ["", "?revision=2", "?revision=99"] {
        let again = message(board.delete_raw(&root.id, None, query).await).await;
        assert_eq!(again, tombstone);
    }
    assert!(board
        .events(&format!("after={}", log.cursor))
        .await
        .events
        .is_empty());

    // A tombstone cannot be edited, labelled or reacted to.
    let gone = StatusCode::CONFLICT;
    let response = board
        .edit_raw(&root.id, None, json!({"text": "back"}))
        .await;
    expect_error(response, gone, codes::MESSAGE_DELETED).await;
    let response = board
        .labels_raw(&root.id, json!({"set": {"status": "open"}}))
        .await;
    expect_error(response, gone, codes::MESSAGE_DELETED).await;
    let response = board.labels_raw(&root.id, json!({})).await;
    expect_error(response, gone, codes::MESSAGE_DELETED).await;
    for method in [Method::PUT, Method::DELETE] {
        let response = board.react_raw(method, &root.id, "thumbsup", None).await;
        expect_error(response, gone, codes::MESSAGE_DELETED).await;
    }
    assert_eq!(board.read(&root.id).await.root, tombstone);

    // Replying to its thread still works, and a reply can be deleted too.
    let late = board.reply(&root.id, "still here").await;
    assert_eq!(late.thread_id, root.id);
    let dead_reply = board.delete(&reply.id).await;
    assert_eq!(dead_reply.revision, 2);
    let thread = board.read(&root.id).await;
    assert_eq!(thread.replies, vec![dead_reply, late]);
    board.stop().await;
}

#[tokio::test]
async fn a_conditional_delete_applies_only_at_the_named_revision() {
    let board = Board::start().await;
    let root = board.thread("dev", "one").await;
    board.edit(&root.id, "two").await;

    let stale = board.delete_raw(&root.id, None, "?revision=1").await;
    expect_error(stale, StatusCode::CONFLICT, codes::REVISION_CONFLICT).await;
    let still = board.read(&root.id).await.root;
    assert_eq!(still.text, "two");
    assert_eq!(still.deleted_at, None);

    let current = board.delete_raw(&root.id, None, "?revision=2").await;
    let tombstone = message(current).await;
    assert_eq!(tombstone.revision, 3);
    assert!(tombstone.deleted_at.is_some());
    assert_eq!(
        board.kinds().await,
        [
            event_types::MESSAGE_POSTED,
            event_types::MESSAGE_EDITED,
            event_types::MESSAGE_DELETED
        ]
    );
    board.stop().await;
}

#[tokio::test]
async fn deleted_and_replaced_text_is_gone_from_the_files() {
    let board = Board::start().await;
    let data_dir = board.data_dir();
    let stored = |needle: &str| contains(&raw_storage(&data_dir), needle.as_bytes());

    // Text long enough to spill onto overflow pages, with a marker at each
    // end, plus a short one that stays inside its page.
    let long = format!(
        "ZEBRA-QUARTZ-HEAD {} ZEBRA-QUARTZ-TAIL",
        "filler words ".repeat(700)
    );
    let posted = board
        .post_as(
            None,
            json!({"channel": "dev", "text": long, "idempotency_key": "once-1"}),
        )
        .await;
    let short = board
        .thread("dev", "MAGENTA-ORBIT in a short message")
        .await;
    let edited_away = board.thread("dev", "COBALT-FERN before the edit").await;
    board
        .labels(
            &posted.message.id,
            json!({"set": {"note": "VIOLET-HARBOR label value"}}),
        )
        .await;
    board
        .labels(
            &short.id,
            json!({"set": {"note": "AMBER-LANTERN second value", "gone": "IVORY-THISTLE third value"}}),
        )
        .await;
    // The check below would prove nothing if the markers were never stored.
    for marker in [
        "ZEBRA-QUARTZ-HEAD",
        "ZEBRA-QUARTZ-TAIL",
        "MAGENTA-ORBIT",
        "COBALT-FERN",
        "VIOLET-HARBOR",
        "AMBER-LANTERN",
        "IVORY-THISTLE",
    ] {
        assert!(stored(marker), "{marker} should be stored before removal");
    }

    board.delete(&posted.message.id).await;
    board.edit(&edited_away.id, "after the edit").await;
    board.delete(&short.id).await;

    // The server is still running and nothing else has been written: the
    // database file and its write-ahead log hold none of it.
    for marker in [
        "ZEBRA-QUARTZ-HEAD",
        "ZEBRA-QUARTZ-TAIL",
        "filler words",
        "MAGENTA-ORBIT",
        "COBALT-FERN",
        "VIOLET-HARBOR",
        "AMBER-LANTERN",
        "IVORY-THISTLE",
    ] {
        assert!(!stored(marker), "{marker} is still in the database files");
    }
    assert!(stored("after the edit"));

    // The idempotency key kept a digest of the text. It is gone, so the key
    // can no longer be matched, and a retry never posts the text again.
    let fingerprint: String = open_db(&data_dir)
        .query_row(
            "SELECT fingerprint FROM idempotency WHERE key = 'once-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fingerprint, "");
    let retry = board
        .send(
            Method::POST,
            "/v1/boards/general/messages",
            None,
            Some(json!({"channel": "dev", "text": long, "idempotency_key": "once-1"})),
        )
        .await;
    expect_error(retry, StatusCode::CONFLICT, codes::IDEMPOTENCY_CONFLICT).await;
    let list: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=dev")
        .await;
    assert_eq!(list.threads.len(), 3);

    // Still nothing after a clean stop, which checkpoints once more.
    let dir = board.stop().await;
    let data_dir = dir.path().join("data");
    for marker in [
        "ZEBRA-QUARTZ",
        "MAGENTA-ORBIT",
        "COBALT-FERN",
        "VIOLET-HARBOR",
    ] {
        assert!(
            !contains(&raw_storage(&data_dir), marker.as_bytes()),
            "{marker}"
        );
    }
    assert!(verify_log(&data_dir).unwrap().is_ok());
}

// ---------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------

#[tokio::test]
async fn labels_are_set_cleared_and_conditional() {
    let board = Board::start().await;
    let root = board.thread("tasks", "do the thing").await;
    assert!(root.labels.is_empty());
    let before = board.events("").await.cursor;
    let labels = |message: &Message| -> Vec<(String, String)> {
        message
            .labels
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    };
    let pair = |key: &str, value: &str| (key.to_owned(), value.to_owned());

    let set = board
        .labels(
            &root.id,
            json!({"set": {"status": "open", "task.owner": "Agent C"}}),
        )
        .await;
    assert_eq!(
        labels(&set),
        [pair("status", "open"), pair("task.owner", "Agent C")]
    );
    // Labels do not touch the revision or the edit time.
    assert_eq!(set.revision, 1);
    assert_eq!(set.edited_at, None);
    assert_eq!(board.read(&root.id).await.root, set);

    // One request sets, replaces and clears together.
    let mixed = board
        .labels(
            &root.id,
            json!({"set": {"status": "claimed", "priority": "high"}, "clear": ["task.owner", "never-set"]}),
        )
        .await;
    assert_eq!(
        labels(&mixed),
        [pair("priority", "high"), pair("status", "claimed")]
    );

    // `if`: every named key must hold exactly that value.
    let wrong = board
        .labels_raw(
            &root.id,
            json!({"set": {"status": "done"}, "if": {"status": "open-SECRETVALUE"}}),
        )
        .await;
    let error = expect_error(wrong, StatusCode::CONFLICT, codes::LABEL_CONFLICT).await;
    // The error names the key and neither the expected nor the real value.
    assert!(error.error.message.contains("status"), "{error:?}");
    let shown = serde_json::to_string(&error).unwrap();
    assert!(!shown.contains("SECRETVALUE") && !shown.contains("claimed"));
    let absent = board
        .labels_raw(
            &root.id,
            json!({"set": {"status": "done"}, "if": {"assignee": "x"}}),
        )
        .await;
    let error = expect_error(absent, StatusCode::CONFLICT, codes::LABEL_CONFLICT).await;
    assert!(error.error.message.contains("assignee"));
    // One false condition among true ones refuses the whole request.
    let partly = board
        .labels_raw(
            &root.id,
            json!({"set": {"extra": "x"}, "clear": ["priority"],
                   "if": {"status": "claimed", "priority": "low"}}),
        )
        .await;
    expect_error(partly, StatusCode::CONFLICT, codes::LABEL_CONFLICT).await;
    assert_eq!(board.read(&root.id).await.root, mixed);
    let claimed = board
        .labels(
            &root.id,
            json!({"set": {"status": "done"}, "if": {"status": "claimed", "priority": "high"}}),
        )
        .await;
    assert_eq!(claimed.labels["status"], "done");

    // `if_unset`: none of the named keys may be set.
    let taken = board
        .labels_raw(
            &root.id,
            json!({"set": {"status": "mine"}, "if_unset": ["assignee", "status"]}),
        )
        .await;
    let error = expect_error(taken, StatusCode::CONFLICT, codes::LABEL_CONFLICT).await;
    assert!(error.error.message.contains("status"));
    let free = board
        .labels(
            &root.id,
            json!({"set": {"assignee": "agent-c"}, "if_unset": ["assignee"]}),
        )
        .await;
    assert_eq!(free.labels["assignee"], "agent-c");
    // The claim cannot be taken twice.
    let second = board
        .labels_raw(
            &root.id,
            json!({"set": {"assignee": "agent-d"}, "if_unset": ["assignee"]}),
        )
        .await;
    expect_error(second, StatusCode::CONFLICT, codes::LABEL_CONFLICT).await;
    assert_eq!(
        board.read(&root.id).await.root.labels["assignee"],
        "agent-c"
    );

    // Four requests changed something, and each appended one event that
    // carries no value.
    let log = board.events(&format!("after={before}")).await;
    assert_eq!(
        kinds_of(&log.events),
        [event_types::LABELS_CHANGED; 4],
        "{:?}",
        log.events
    );
    let shown = serde_json::to_string(&log.events).unwrap();
    for value in ["Agent C", "claimed", "agent-c", "high"] {
        assert!(!shown.contains(value), "{value} is in an event");
    }
    assert!(log.events.iter().all(|event| event.message_id == root.id));

    // A request that changes nothing succeeds and appends no event.
    for body in [
        json!({}),
        json!({"set": {"status": "done"}}),
        json!({"clear": ["never-set"]}),
        json!({"set": {"assignee": "agent-c"}, "if": {"status": "done"}}),
        json!({"if_unset": ["never-set"]}),
    ] {
        let same = board.labels(&root.id, body).await;
        assert_eq!(same, free);
    }
    assert!(board
        .events(&format!("after={}", log.cursor))
        .await
        .events
        .is_empty());
    board.stop().await;
}

#[tokio::test]
async fn label_validation_errors() {
    let board = Board::start().await;
    let root = board.thread("tasks", "x").await;
    let bad = StatusCode::BAD_REQUEST;

    for body in [
        json!({"set": {"": "x"}}),
        json!({"set": {"Status": "x"}}),
        json!({"set": {"a b": "x"}}),
        json!({"set": {"a=b": "x"}}),
        json!({"set": {"k".repeat(65): "x"}}),
        json!({"set": {"board.kind": "x"}}),
        json!({"clear": ["board.kind"]}),
        json!({"clear": ["UPPER"]}),
        json!({"if": {"board.kind": "x"}}),
        json!({"if_unset": ["a/b"]}),
        json!({"set": {"status": ""}}),
        json!({"set": {"status": "a\nb"}}),
        json!({"set": {"status": "a\u{0}b"}}),
        json!({"set": {"status": "v".repeat(257)}}),
        json!({"if": {"status": ""}}),
    ] {
        let response = board.labels_raw(&root.id, body.clone()).await;
        let error = expect_error(response, bad, codes::INVALID_LABEL).await;
        assert!(!error.error.message.contains("UPPER"), "{body}");
    }
    // 64 and 256 are allowed.
    let edge = board
        .labels(&root.id, json!({"set": {"k".repeat(64): "v".repeat(256)}}))
        .await;
    assert_eq!(edge.labels.len(), 1);
    board
        .labels(&root.id, json!({"clear": ["k".repeat(64)]}))
        .await;

    for body in [
        json!({"set": {"status": 5}}),
        json!({"set": ["status"]}),
        json!({"clear": "status"}),
        json!("status"),
        // A key cannot be both set and cleared.
        json!({"set": {"status": "x"}, "clear": ["status"]}),
    ] {
        let response = board.labels_raw(&root.id, body).await;
        expect_error(response, bad, codes::INVALID_REQUEST).await;
    }
    let form = board
        .http
        .post(board.url(&format!("/v1/boards/general/messages/{}/labels", root.id)))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("set=1")
        .send()
        .await
        .unwrap();
    expect_error(
        form,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        codes::INVALID_REQUEST,
    )
    .await;
    let response = board
        .labels_raw("msg_nope", json!({"set": {"status": "x"}}))
        .await;
    expect_error(response, StatusCode::NOT_FOUND, codes::NOT_FOUND).await;

    // A message holds at most 32 labels, counted after the request.
    let many: serde_json::Map<String, Value> =
        (0..32).map(|n| (format!("k{n}"), json!("v"))).collect();
    let full = board.labels(&root.id, json!({ "set": many })).await;
    assert_eq!(full.labels.len(), 32);
    let over = board
        .labels_raw(&root.id, json!({"set": {"one-more": "v"}}))
        .await;
    expect_error(over, bad, codes::INVALID_LABEL).await;
    assert_eq!(board.read(&root.id).await.root.labels.len(), 32);
    // Replacing a value, or swapping one key for another, stays within it.
    let swapped = board
        .labels(
            &root.id,
            json!({"set": {"one-more": "v", "k1": "w"}, "clear": ["k0"]}),
        )
        .await;
    assert_eq!(swapped.labels.len(), 32);
    assert_eq!(swapped.labels["k1"], "w");

    // Nothing invalid was announced.
    assert_eq!(
        board.kinds().await,
        [
            event_types::MESSAGE_POSTED,
            event_types::LABELS_CHANGED,
            event_types::LABELS_CHANGED,
            event_types::LABELS_CHANGED,
            event_types::LABELS_CHANGED
        ]
    );
    board.stop().await;
}

#[tokio::test]
async fn threads_are_filtered_by_label() {
    let board = Board::start().await;
    let open = board.thread("tasks", "open task").await;
    let claimed = board.thread("tasks", "claimed task").await;
    let via_reply = board.thread("tasks", "labelled on a reply").await;
    let plain = board.thread("tasks", "no labels").await;
    let elsewhere = board.thread("other", "another channel").await;
    let reply = board.reply(&via_reply.id, "the reply").await;
    board
        .labels(&open.id, json!({"set": {"status": "open", "kind": "bug"}}))
        .await;
    board
        .labels(&claimed.id, json!({"set": {"status": "in progress"}}))
        .await;
    board
        .labels(&reply.id, json!({"set": {"status": "done"}}))
        .await;
    board
        .labels(&elsewhere.id, json!({"set": {"status": "open"}}))
        .await;

    let listed = |query: &'static str| {
        let board = &board;
        async move {
            let list: ThreadList = board
                .get_json(&format!("/v1/boards/general/threads?{query}"))
                .await;
            let mut ids: Vec<String> = list.threads.into_iter().map(|t| t.root.id).collect();
            ids.sort();
            ids
        }
    };
    let ids = |messages: &[&Message]| {
        let mut ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
        ids.sort();
        ids
    };

    assert_eq!(
        listed("channel=tasks").await,
        ids(&[&open, &claimed, &via_reply, &plain])
    );
    // `key`: any value, on the root or on any reply.
    assert_eq!(
        listed("channel=tasks&label=status").await,
        ids(&[&open, &claimed, &via_reply])
    );
    assert_eq!(listed("channel=tasks&label=kind").await, ids(&[&open]));
    assert!(listed("channel=tasks&label=nothing").await.is_empty());
    // `key=value`.
    assert_eq!(
        listed("channel=tasks&label=status%3Dopen").await,
        ids(&[&open])
    );
    assert_eq!(
        listed("channel=tasks&label=status=done").await,
        ids(&[&via_reply])
    );
    assert_eq!(
        listed("channel=tasks&label=status=in%20progress").await,
        ids(&[&claimed])
    );
    assert!(listed("channel=tasks&label=status=OPEN").await.is_empty());
    assert!(listed("channel=tasks&label=kind=open").await.is_empty());
    // `key=a|b`: either value.
    assert_eq!(
        listed("channel=tasks&label=status=open|done").await,
        ids(&[&open, &via_reply])
    );
    assert_eq!(
        listed("channel=tasks&label=status=open%7Cin%20progress%7Cnope").await,
        ids(&[&open, &claimed])
    );
    // The channel still applies, and so does a subtree.
    assert_eq!(
        listed("channel=other&label=status=open").await,
        ids(&[&elsewhere])
    );

    // The filter pages like any listing.
    let first: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=tasks&label=status&limit=2")
        .await;
    assert_eq!(first.threads.len(), 2);
    let cursor = first.next_cursor.expect("another page");
    let second: ThreadList = board
        .get_json(&format!(
            "/v1/boards/general/threads?channel=tasks&label=status&limit=2&cursor={cursor}"
        ))
        .await;
    assert_eq!(second.threads.len(), 1);
    assert!(second.next_cursor.is_none());
    let mut paged: Vec<String> = first
        .threads
        .iter()
        .chain(&second.threads)
        .map(|t| t.root.id.clone())
        .collect();
    paged.sort();
    assert_eq!(paged, ids(&[&open, &claimed, &via_reply]));

    // Clearing the label takes the thread out.
    board.labels(&reply.id, json!({"clear": ["status"]})).await;
    assert_eq!(
        listed("channel=tasks&label=status").await,
        ids(&[&open, &claimed])
    );

    for query in [
        "channel=tasks&label=",
        "channel=tasks&label=Status",
        "channel=tasks&label=status=",
        "channel=tasks&label=status=a|",
        "channel=tasks&label=board.kind",
        "channel=tasks&label==open",
    ] {
        let response = board
            .get(&format!("/v1/boards/general/threads?{query}"))
            .await;
        expect_error(response, StatusCode::BAD_REQUEST, codes::INVALID_LABEL).await;
    }
    board.stop().await;
}

// ---------------------------------------------------------------------------
// Reactions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reactions_are_one_per_principal_per_name() {
    let board = Board::start().await;
    let root = board.thread("dev", "ship it?").await;
    let before = board.events("").await.cursor;
    let shown = |message: &Message| -> Vec<(String, Vec<String>)> {
        message
            .reactions
            .iter()
            .map(|reaction| (reaction.name.clone(), reaction.by.clone()))
            .collect()
    };
    let local = || vec!["local".to_owned()];

    let first = board.react(&root.id, "thumbsup").await;
    assert_eq!(shown(&first), [("thumbsup".to_owned(), local())]);
    assert_eq!(first.revision, 1);
    assert_eq!(first.edited_at, None);

    // Twice is once, and a profile is the same principal.
    assert_eq!(board.react(&root.id, "thumbsup").await, first);
    let as_fx = board
        .react_raw(Method::PUT, &root.id, "thumbsup", Some("fx"))
        .await;
    assert_eq!(message(as_fx).await, first);

    // Reactions are ordered by name. `+1` needs no escaping in a path.
    board.react(&root.id, "rocket").await;
    board.react(&root.id, "+1").await;
    let three = board.react(&root.id, "-1").await;
    assert_eq!(
        shown(&three),
        [
            ("+1".to_owned(), local()),
            ("-1".to_owned(), local()),
            ("rocket".to_owned(), local()),
            ("thumbsup".to_owned(), local())
        ]
    );
    assert_eq!(board.read(&root.id).await.root, three);

    // Removing, also from under another profile, and removing what is not
    // there.
    let removed = board
        .react_raw(Method::DELETE, &root.id, "rocket", Some("fx"))
        .await;
    assert_eq!(message(removed).await.reactions.len(), 3);
    let same = board.unreact(&root.id, "rocket").await;
    assert_eq!(same.reactions.len(), 3);
    assert_eq!(board.unreact(&root.id, "never-given").await, same);

    // Five changes, five events; the three no-ops appended none. Events
    // carry no reaction name.
    let log = board.events(&format!("after={before}")).await;
    assert_eq!(kinds_of(&log.events), [event_types::REACTIONS_CHANGED; 5]);
    let text = serde_json::to_string(&log.events).unwrap();
    for name in ["thumbsup", "rocket", "+1"] {
        assert!(!text.contains(name), "{name} is in an event");
    }
    // The event is by whoever reacted, profile and all.
    assert_eq!(log.events[4].by.handle, "local/fx");
    assert_eq!(log.events[0].by.handle, "local");

    let bad = StatusCode::BAD_REQUEST;
    for name in [
        "Thumbsup",
        "a%20b",
        "%F0%9F%91%8D",
        "a.b",
        &"a".repeat(33),
        "%3Cscript%3E",
    ] {
        for method in [Method::PUT, Method::DELETE] {
            let response = board.react_raw(method, &root.id, name, None).await;
            expect_error(response, bad, codes::INVALID_REACTION).await;
        }
    }
    assert_eq!(
        board.react(&root.id, &"a".repeat(32)).await.reactions.len(),
        4
    );
    let response = board
        .react_raw(Method::PUT, "msg_nope", "thumbsup", None)
        .await;
    expect_error(response, StatusCode::NOT_FOUND, codes::NOT_FOUND).await;
    let response = board
        .react_raw(Method::POST, &root.id, "thumbsup", None)
        .await;
    expect_error(
        response,
        StatusCode::METHOD_NOT_ALLOWED,
        codes::INVALID_REQUEST,
    )
    .await;
    board.stop().await;
}

// ---------------------------------------------------------------------------
// Mentions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mentions_are_resolved_when_text_is_written() {
    let board = Board::start().await;
    let mentions = |message: &Message| -> Vec<(String, Option<String>)> {
        message
            .mentions
            .iter()
            .map(|mention| (mention.handle.clone(), mention.profile.clone()))
            .collect()
    };
    let local = |profile: Option<&str>| ("local".to_owned(), profile.map(str::to_owned));

    // The only principal of a local board is `local`.
    let root = board
        .thread(
            "dev",
            "hi @Local, and @local/reviewer. Not @nobody, not `@local/code`, not a@local.\n\n\
             ```\n@local/fenced\n```\n\n    @local/indented\n\n@local again, @LOCAL/Reviewer too",
        )
        .await;
    assert_eq!(mentions(&root), [local(None), local(Some("reviewer"))]);
    assert!(root
        .mentions
        .iter()
        .all(|mention| mention.kind == ActorKind::Local));
    // The read returns what was stored.
    assert_eq!(board.read(&root.id).await.root, root);

    // A message that mentions nobody has no mentions and still posts.
    let quiet = board.thread("dev", "@nobody @also-nobody `@local`").await;
    assert!(quiet.mentions.is_empty());
    let raw: Value = board
        .get_json(&format!("/v1/boards/general/threads/{}", quiet.id))
        .await;
    assert!(raw["root"].get("mentions").is_none());

    // An edit resolves again from the new text.
    let edited = board.edit(&root.id, "now only @local/fx").await;
    assert_eq!(mentions(&edited), [local(Some("fx"))]);
    let cleared = board.edit(&root.id, "nobody at all").await;
    assert!(cleared.mentions.is_empty());
    let added = board.edit(&quiet.id, "@local after all").await;
    assert_eq!(mentions(&added), [local(None)]);

    // A reply resolves too.
    let reply = board.reply(&root.id, "(@local)").await;
    assert_eq!(mentions(&reply), [local(None)]);

    // An idempotent retry returns the stored mentions.
    let body = json!({"channel": "dev", "text": "ping @local", "idempotency_key": "m-1"});
    let first = board.post_as(None, body.clone()).await;
    let retry = board.post_as(None, body).await;
    assert!(retry.duplicate);
    assert_eq!(retry.message, first.message);
    assert_eq!(mentions(&retry.message), [local(None)]);

    // Mentions are capped per message; the rest stay text.
    let many: String = (0..80).map(|n| format!("@local/p{n} ")).collect();
    let capped = board.thread("dev", &many).await;
    assert_eq!(capped.mentions.len(), 50);
    board.stop().await;
}

#[tokio::test]
async fn mentions_self_filters_events_by_stored_mentions() {
    let board = Board::start().await;
    let nobody = board.thread("dev", "mentions nobody").await;
    let anyone = board.thread("dev", "for @local").await;
    let for_fx = board.thread("dev", "for @local/fx").await;
    let for_fy = board.thread("dev", "for @local/fy").await;
    let ids = |events: &[Event]| -> Vec<String> {
        events
            .iter()
            .map(|event| event.message_id.clone())
            .collect()
    };

    // With no declared profile: only mentions that name no profile.
    let plain = board.events("after=start&mentions_self=true").await;
    assert_eq!(ids(&plain.events), std::slice::from_ref(&anyone.id));
    // With a profile: those, and the ones that name it, in any case.
    for profile in ["fx", "FX"] {
        let fx = board
            .events_as(profile, "after=start&mentions_self=true")
            .await;
        assert_eq!(ids(&fx.events), [anyone.id.clone(), for_fx.id.clone()]);
    }
    let other = board
        .events_as("zz", "after=start&mentions_self=true")
        .await;
    assert_eq!(ids(&other.events), std::slice::from_ref(&anyone.id));
    // `false` is no filter.
    assert_eq!(
        board
            .events("after=start&mentions_self=false")
            .await
            .events
            .len(),
        4
    );
    // The cursor moves past everything scanned, matching or not.
    assert_eq!(plain.cursor, board.events("after=start").await.cursor);

    // Every event about a mentioning message matches, whatever its type,
    // and other filters combine with it.
    board.react(&anyone.id, "eyes").await;
    board
        .labels(&anyone.id, json!({"set": {"seen": "yes"}}))
        .await;
    board.react(&nobody.id, "eyes").await;
    let all = board.events("after=start&mentions_self=true").await;
    assert_eq!(
        kinds_of(&all.events),
        [
            event_types::MESSAGE_POSTED,
            event_types::REACTIONS_CHANGED,
            event_types::LABELS_CHANGED
        ]
    );
    let posted = board
        .events("after=start&mentions_self=true&types=message.posted,message.edited")
        .await;
    assert_eq!(kinds_of(&posted.events), [event_types::MESSAGE_POSTED]);
    let not_mine = board
        .events("after=start&mentions_self=true&not_from_self=true")
        .await;
    assert!(not_mine.events.is_empty());

    // The filter reads the message as it is stored now. Editing the mention
    // away, or deleting the message, takes its events out, including the
    // ones from before.
    board.edit(&anyone.id, "for nobody now").await;
    assert!(board
        .events("after=start&mentions_self=true")
        .await
        .events
        .is_empty());
    let fx = board
        .events_as("fx", "after=start&mentions_self=true")
        .await;
    assert_eq!(ids(&fx.events), std::slice::from_ref(&for_fx.id));
    board.delete(&for_fx.id).await;
    assert!(board
        .events_as("fx", "after=start&mentions_self=true")
        .await
        .events
        .is_empty());
    // And editing one in puts them back.
    board.edit(&nobody.id, "now for @local/fy").await;
    let fy = board
        .events_as("fy", "after=start&mentions_self=true")
        .await;
    // Every event about either message, the earlier ones included.
    assert_eq!(
        ids(&fy.events),
        [
            nobody.id.clone(),
            for_fy.id.clone(),
            nobody.id.clone(),
            nobody.id.clone()
        ]
    );
    assert_eq!(
        kinds_of(&fy.events),
        [
            event_types::MESSAGE_POSTED,
            event_types::MESSAGE_POSTED,
            event_types::REACTIONS_CHANGED,
            event_types::MESSAGE_EDITED
        ]
    );

    let response = board
        .get("/v1/boards/general/events?mentions_self=maybe")
        .await;
    expect_error(response, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;
    board.stop().await;
}

#[tokio::test]
async fn a_mention_wakes_a_mentions_self_long_poll_and_stream() {
    let board = Arc::new(Board::start().await);
    let waiter = {
        let board = Arc::clone(&board);
        tokio::spawn(async move {
            board
                .events_as("reviewer", "wait=30&mentions_self=true")
                .await
        })
    };
    let mut stream = board
        .send(
            Method::GET,
            "/v1/boards/general/events/stream?mentions_self=true",
            Some("reviewer"),
            None,
        )
        .await;
    assert_eq!(stream.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Posts that do not mention the waiter do not wake it.
    board.thread("dev", "nothing for anyone").await;
    board.thread("dev", "for @local/someone-else").await;
    board.thread("dev", "`@local/reviewer` in code").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished());

    let wanted = board.thread("dev", "please look, @local/reviewer").await;
    let woke = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the mention wakes the long-poll")
        .unwrap();
    assert_eq!(woke.events.len(), 1);
    assert_eq!(woke.events[0].message_id, wanted.id);
    let frames = read_frames(&mut stream, 1).await;
    assert_eq!(frames[0].1.message_id, wanted.id);

    // An edit that adds a mention wakes the stream as well.
    let later = board.thread("dev", "nothing yet").await;
    board.edit(&later.id, "now for @local").await;
    let frames = read_frames(&mut stream, 1).await;
    assert_eq!(frames[0].1.message_id, later.id);
    assert_eq!(frames[0].1.kind, event_types::MESSAGE_EDITED);
    drop(stream);
    Arc::into_inner(board).unwrap().stop().await;
}

// ---------------------------------------------------------------------------
// Event types, wake-ups and the chain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn events_are_filtered_by_type() {
    let board = Board::start().await;
    let root = board.thread("dev", "one").await;
    board.edit(&root.id, "two").await;
    board.labels(&root.id, json!({"set": {"a": "b"}})).await;
    board.react(&root.id, "eyes").await;
    board.delete(&root.id).await;
    let all = [
        event_types::MESSAGE_POSTED,
        event_types::MESSAGE_EDITED,
        event_types::LABELS_CHANGED,
        event_types::REACTIONS_CHANGED,
        event_types::MESSAGE_DELETED,
    ];
    assert_eq!(board.kinds().await, all);

    for kind in all {
        let only = board.events(&format!("after=start&types={kind}")).await;
        assert_eq!(kinds_of(&only.events), [kind]);
        // The cursor is past the whole log, not only past the match.
        assert!(board
            .events(&format!("after={}", only.cursor))
            .await
            .events
            .is_empty());
    }
    let two = board
        .events("after=start&types=message.deleted,message.posted")
        .await;
    assert_eq!(
        kinds_of(&two.events),
        [event_types::MESSAGE_POSTED, event_types::MESSAGE_DELETED]
    );
    let every = board
        .events(&format!("after=start&types={}", all.join(",")))
        .await;
    assert_eq!(every.events.len(), 5);
    // The stream takes the same filter.
    let mut stream = board
        .get("/v1/boards/general/events/stream?after=start&types=labels.changed,reactions.changed")
        .await;
    let frames = read_frames(&mut stream, 2).await;
    assert_eq!(frames[0].1.kind, event_types::LABELS_CHANGED);
    assert_eq!(frames[1].1.kind, event_types::REACTIONS_CHANGED);
    drop(stream);

    for query in [
        "types=",
        "types=nope",
        "types=message.posted,nope",
        "types=message.posted,",
        "types=Message.Posted",
        "types=message.posted%20",
    ] {
        for route in ["events", "events/stream"] {
            let response = board
                .get(&format!("/v1/boards/general/{route}?{query}"))
                .await;
            expect_error(response, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;
        }
    }
    board.stop().await;
}

#[tokio::test]
async fn every_kind_of_write_wakes_a_long_poll() {
    let board = Arc::new(Board::start().await);
    let root = board.thread("dev", "one").await;
    let wait = |types: &'static str| {
        let board = Arc::clone(&board);
        tokio::spawn(async move { board.events(&format!("wait=30&types={types}")).await })
    };
    let woken = |waiter: JoinHandle<EventsResponse>, kind: &'static str| async move {
        let woke = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap_or_else(|_| panic!("{kind} wakes the long-poll"))
            .unwrap();
        assert_eq!(kinds_of(&woke.events), [kind]);
    };

    let waiter = wait(event_types::MESSAGE_EDITED);
    tokio::time::sleep(Duration::from_millis(200)).await;
    board.edit(&root.id, "two").await;
    woken(waiter, event_types::MESSAGE_EDITED).await;

    let waiter = wait(event_types::LABELS_CHANGED);
    tokio::time::sleep(Duration::from_millis(200)).await;
    board.labels(&root.id, json!({"set": {"a": "b"}})).await;
    woken(waiter, event_types::LABELS_CHANGED).await;

    let waiter = wait(event_types::REACTIONS_CHANGED);
    tokio::time::sleep(Duration::from_millis(200)).await;
    board.react(&root.id, "eyes").await;
    woken(waiter, event_types::REACTIONS_CHANGED).await;

    let waiter = wait(event_types::REACTIONS_CHANGED);
    tokio::time::sleep(Duration::from_millis(200)).await;
    board.unreact(&root.id, "eyes").await;
    woken(waiter, event_types::REACTIONS_CHANGED).await;

    let waiter = wait(event_types::MESSAGE_DELETED);
    tokio::time::sleep(Duration::from_millis(200)).await;
    board.delete(&root.id).await;
    woken(waiter, event_types::MESSAGE_DELETED).await;
    Arc::into_inner(board).unwrap().stop().await;
}

#[tokio::test]
async fn a_change_whose_client_gave_up_still_wakes_waiters() {
    let board = Arc::new(Board::start().await);
    let root = board.thread("dev", "one").await;
    let reply = board.reply(&root.id, "to be deleted").await;
    let impatient = Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let message_url = |id: &str| board.url(&format!("/v1/boards/general/messages/{id}"));
    let requests = [
        (
            event_types::MESSAGE_EDITED,
            impatient
                .patch(message_url(&root.id))
                .json(&json!({"text": "two"})),
        ),
        (
            event_types::LABELS_CHANGED,
            impatient
                .post(format!("{}/labels", message_url(&root.id)))
                .json(&json!({"set": {"a": "b"}})),
        ),
        (
            event_types::REACTIONS_CHANGED,
            impatient.put(format!("{}/reactions/eyes", message_url(&root.id))),
        ),
        (
            event_types::MESSAGE_DELETED,
            impatient.delete(message_url(&reply.id)),
        ),
    ];
    for (kind, request) in requests {
        let waiter = {
            let board = Arc::clone(&board);
            tokio::spawn(async move { board.events("wait=30").await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Hold the database's write lock so the change stalls inside the
        // store, and let the client give up while it is stalled.
        let db = open_db(&board.data_dir());
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let gave_up = request.send().await.unwrap_err();
        assert!(gave_up.is_timeout(), "{kind}: {gave_up}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiter.is_finished(), "{kind}");
        db.execute_batch("ROLLBACK").unwrap();
        drop(db);

        // The change commits once the lock is free, and the waiter hears.
        let woke = tokio::time::timeout(Duration::from_secs(4), waiter)
            .await
            .unwrap_or_else(|_| panic!("{kind} wakes although its client is gone"))
            .unwrap();
        assert_eq!(kinds_of(&woke.events), [kind]);
    }
    let thread = board.read(&root.id).await;
    assert_eq!(thread.root.text, "two");
    assert_eq!(thread.root.labels.len(), 1);
    assert_eq!(thread.root.reactions.len(), 1);
    assert!(thread.replies[0].deleted_at.is_some());
    Arc::into_inner(board).unwrap().stop().await;
}

#[tokio::test]
async fn the_chain_verifies_over_every_kind_of_event() {
    let board = Board::start().await;
    let data_dir = board.data_dir();
    let root = board.thread("dev", "PLUM-ANCHOR text for @local").await;
    let reply = board
        .post_as(Some("fx"), json!({"thread_id": root.id, "text": "a reply"}))
        .await
        .message;
    board.edit(&root.id, "PLUM-ANCHOR edited").await;
    board
        .labels(&root.id, json!({"set": {"status": "TEAL-MEADOW"}}))
        .await;
    board.react(&root.id, "sparkles").await;
    board
        .react_raw(Method::DELETE, &root.id, "sparkles", Some("fx"))
        .await;
    board
        .labels(&reply.id, json!({"set": {"status": "TEAL-MEADOW"}}))
        .await;
    board
        .send(
            Method::POST,
            "/v1/boards/second/messages",
            None,
            Some(json!({"channel": "a", "text": "on another board"})),
        )
        .await;
    board.delete_raw(&reply.id, Some("fx"), "").await;
    board.delete(&root.id).await;

    let log = board.events("after=start").await;
    assert_eq!(
        kinds_of(&log.events),
        [
            event_types::MESSAGE_POSTED,
            event_types::MESSAGE_POSTED,
            event_types::MESSAGE_EDITED,
            event_types::LABELS_CHANGED,
            event_types::REACTIONS_CHANGED,
            event_types::REACTIONS_CHANGED,
            event_types::LABELS_CHANGED,
            event_types::MESSAGE_DELETED,
            event_types::MESSAGE_DELETED,
        ]
    );
    // One chain, each event linked to the one before.
    for pair in log.events.windows(2) {
        assert_eq!(pair[1].prev_hash, pair[0].hash);
    }
    // Events carry references, never text, label values or reaction names.
    let shown = serde_json::to_string(&log.events).unwrap();
    for secret in ["PLUM-ANCHOR", "TEAL-MEADOW", "sparkles"] {
        assert!(!shown.contains(secret), "{secret} is in an event");
    }
    // Every event names its message's thread and channel.
    assert!(log
        .events
        .iter()
        .all(|event| event.thread_id == root.id && event.channel == "dev"));
    assert_eq!(log.events[5].by.handle, "local/fx");
    assert_eq!(log.events[7].message_id, reply.id);

    let report = verify_log(&data_dir).unwrap();
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(report.boards[0].verified_events, 9);
    assert_eq!(report.boards[1].verified_events, 1);
    let dir = board.stop().await;
    let data_dir = dir.path().join("data");
    assert!(verify_log(&data_dir).unwrap().is_ok());

    // The new event types are covered by the chain like the first one:
    // changing one's type, or what it is about, breaks it.
    let db = open_db(&data_dir);
    let stored: String = db
        .query_row(
            "SELECT group_concat(kind || channel || thread_id || message_id || at) FROM events",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for secret in ["PLUM-ANCHOR", "TEAL-MEADOW", "sparkles"] {
        assert!(!stored.contains(secret));
    }
    db.execute_batch("DROP TRIGGER events_are_append_only_update")
        .unwrap();
    for (seq, tamper) in [
        (3, "kind = 'labels.changed'"),
        (4, "kind = 'message.posted'"),
        (6, "message_id = 'msg_other'"),
        (8, "kind = 'message.edited'"),
    ] {
        let original: (String, String) = db
            .query_row(
                "SELECT kind, message_id FROM events WHERE board_id = 1 AND seq = ?1",
                [seq],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        db.execute(
            &format!("UPDATE events SET {tamper} WHERE board_id = 1 AND seq = ?1"),
            [seq],
        )
        .unwrap();
        let report = verify_log(&data_dir).unwrap();
        assert!(!report.is_ok(), "tampering with event {seq} went unnoticed");
        let broken = report.boards[0].broken.as_ref().unwrap();
        assert_eq!(broken.position, seq as u64);
        assert_eq!(report.boards[0].verified_events, seq as u64 - 1);
        db.execute(
            "UPDATE events SET kind = ?1, message_id = ?2 WHERE board_id = 1 AND seq = ?3",
            rusqlite::params![original.0, original.1, seq],
        )
        .unwrap();
        assert!(verify_log(&data_dir).unwrap().is_ok());
    }
}

// ---------------------------------------------------------------------------
// Rendered HTML
// ---------------------------------------------------------------------------

#[tokio::test]
async fn html_is_rendered_on_request_through_the_allowlist() {
    let board = Board::start().await;
    let root = board
        .thread(
            "dev",
            "# Plan\n\nHello @local, **read** [this](https://example.com/a?b=1&c=2).\n\n\
             <script>alert('x')</script>\n\n![logo](https://example.com/logo.png) \
             [bad](javascript:alert(1)) <img src=x onerror=alert(1)>",
        )
        .await;
    let reply = board.reply(&root.id, "- `@local` in code\n- *done*").await;
    let dead = board.reply(&root.id, "soon gone").await;
    board.delete(&dead.id).await;

    // Without the option there is no HTML, on any route.
    assert_eq!(root.html, None);
    let plain = board.read(&root.id).await;
    assert_eq!(plain.root.html, None);
    for query in ["", "?html=false"] {
        let raw: Value = board
            .get_json(&format!("/v1/boards/general/threads/{}{query}", root.id))
            .await;
        assert!(raw["root"].get("html").is_none());
        assert!(raw["replies"][0].get("html").is_none());
    }

    let thread: Thread = board
        .get_json(&format!("/v1/boards/general/threads/{}?html=true", root.id))
        .await;
    let html = thread.root.html.clone().expect("html for the root");
    assert_eq!(
        html,
        "<h1>Plan</h1><p>Hello <span class=\"mention\">@local</span>, <strong>read</strong> \
         <a href=\"https://example.com/a?b=1&amp;c=2\" rel=\"noopener noreferrer nofollow\">this</a>.</p>\
         <pre>&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;\n</pre>\
         <p><a href=\"https://example.com/logo.png\" rel=\"noopener noreferrer nofollow\">logo</a> \
         bad &lt;img src=x onerror=alert(1)&gt;</p>"
    );
    for forbidden in [
        "<script",
        "<img",
        "javascript:",
        "onerror=alert(1)>\"",
        "<h1 ",
    ] {
        assert!(!html.contains(forbidden), "{forbidden} in {html}");
    }
    // The text itself is returned untouched beside it.
    assert_eq!(thread.root.text, root.text);
    assert_eq!(
        thread.replies[0].html.as_deref(),
        Some("<ul><li><code>@local</code> in code</li><li><em>done</em></li></ul>")
    );
    // A tombstone has an empty rendering, not a missing one.
    assert_eq!(thread.replies[1].html.as_deref(), Some(""));
    assert_eq!(thread.replies[1].text, "");
    // Apart from the HTML it is the same thread.
    let mut stripped = thread.clone();
    stripped.root.html = None;
    stripped.replies.iter_mut().for_each(|m| m.html = None);
    assert_eq!(stripped, plain);
    assert_eq!(stripped.replies[0], reply);

    // The listing takes the same option.
    let list: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=dev&html=true")
        .await;
    assert_eq!(list.threads[0].root.html.as_deref(), Some(html.as_str()));
    let list: ThreadList = board
        .get_json("/v1/boards/general/threads?channel=dev")
        .await;
    assert_eq!(list.threads[0].root.html, None);

    // An edit is rendered from the new text and its new mentions.
    board.edit(&root.id, "just @nobody and @local/fx").await;
    let thread: Thread = board
        .get_json(&format!("/v1/boards/general/threads/{}?html=true", root.id))
        .await;
    assert_eq!(
        thread.root.html.as_deref(),
        Some("<p>just @nobody and <span class=\"mention\">@local/fx</span></p>")
    );

    for path in [
        format!("/v1/boards/general/threads/{}?html=yes", root.id),
        "/v1/boards/general/threads?channel=dev&html=1".to_owned(),
    ] {
        let response = board.get(&path).await;
        expect_error(response, StatusCode::BAD_REQUEST, codes::INVALID_REQUEST).await;
    }
    board.stop().await;
}

#[tokio::test]
async fn hostile_markdown_is_stored_and_rendered_safely() {
    let board = Board::start_with(|config| config.max_message_chars = 60_000, None).await;
    let hostile = [
        "> ".repeat(20_000) + "deep",
        "*".repeat(50_000),
        "[".repeat(25_000) + &"]".repeat(25_000),
        "<script>".repeat(5_000),
        "@local/a ".repeat(5_000),
        "[a](javascript:alert(1)) ".repeat(2_000),
        "- ".repeat(25_000),
    ];
    for text in &hostile {
        let started = std::time::Instant::now();
        let posted = board.thread("dev", text).await;
        let thread: Thread = board
            .get_json(&format!(
                "/v1/boards/general/threads/{}?html=true",
                posted.id
            ))
            .await;
        assert!(started.elapsed() < Duration::from_secs(10));
        let html = thread.root.html.unwrap();
        assert!(html.len() < text.len() * 40 + 64, "{} bytes", html.len());
        assert!(!html.contains("<script") && !html.contains("<a href=\"javascript"));
        assert!(html.matches("<blockquote>").count() <= 32);
        assert!(posted.mentions.len() <= 50);
    }
    board.stop().await;
}

// ---------------------------------------------------------------------------
// Boards
// ---------------------------------------------------------------------------

#[tokio::test]
async fn boards_are_listed_by_name_with_their_last_activity() {
    let board = Board::start().await;
    // The default board exists before anything is posted, with no activity.
    let boards: BoardList = board.get_json("/v1/boards").await;
    assert_eq!(boards.boards.len(), 1);
    assert_eq!(boards.boards[0].name, "general");
    assert_eq!(boards.boards[0].last_activity_at, None);
    let raw: Value = board.get_json("/v1/boards").await;
    assert_eq!(raw, json!({"boards": [{"name": "general"}]}));

    for name in ["zeta", "Alpha", "middle"] {
        let response = board
            .send(
                Method::POST,
                &format!("/v1/boards/{name}/messages"),
                None,
                Some(json!({"channel": "a", "text": "first"})),
            )
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }
    let first = board.thread("dev", "one").await;
    let last = board.thread("other", "two").await;
    // Reading a board that does not exist does not add it to the list.
    board.get("/v1/boards/ghost/channels").await;

    let boards: BoardList = board.get_json("/v1/boards").await;
    let names: Vec<&str> = boards.boards.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["Alpha", "general", "middle", "zeta"]);
    assert!(boards.boards.iter().all(|b| b.last_activity_at.is_some()));
    // The newest message on the board, across its channels.
    assert_eq!(
        boards.boards[1].last_activity_at.as_deref(),
        Some(last.created_at.as_str())
    );
    assert!(last.created_at >= first.created_at);

    for method in [Method::POST, Method::DELETE] {
        let response = board.send(method, "/v1/boards", None, None).await;
        expect_error(
            response,
            StatusCode::METHOD_NOT_ALLOWED,
            codes::INVALID_REQUEST,
        )
        .await;
    }
    board.stop().await;
}

// ---------------------------------------------------------------------------
// Pages and response headers
// ---------------------------------------------------------------------------

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                   img-src 'self' data:; connect-src 'self'; base-uri 'none'; \
                   form-action 'none'; frame-ancestors 'none'";

/// Asserts the headers every response carries. `cache` is the expected
/// `Cache-Control`.
fn assert_security_headers(response: &Response, cache: &str, what: &str) {
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .unwrap_or_else(|| panic!("{what} has no {name}"))
            .to_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(header("content-security-policy"), CSP, "{what}");
    assert_eq!(header("x-content-type-options"), "nosniff", "{what}");
    assert_eq!(header("referrer-policy"), "no-referrer", "{what}");
    assert_eq!(header("cache-control"), cache, "{what}");
    assert_eq!(
        response.headers().get_all("cache-control").iter().count(),
        1,
        "{what}"
    );
    for (name, _) in response.headers() {
        assert!(
            !name.as_str().starts_with("access-control-"),
            "{what} sends {name}"
        );
    }
}

#[tokio::test]
async fn the_page_and_its_assets_are_served_from_the_binary() {
    let board = Board::start().await;

    let index = board.get("/").await;
    assert_eq!(index.status(), StatusCode::OK);
    assert_eq!(index.headers()["content-type"], "text/html; charset=utf-8");
    assert_security_headers(&index, "no-store", "the page");
    let page = index.text().await.unwrap();
    assert!(page.to_ascii_lowercase().starts_with("<!doctype html>"));
    assert!(page.contains("/assets/app.js") && page.contains("/assets/app.css"));
    // The sign-in path serves the same page; the code is in the fragment.
    let join = board.get("/join").await;
    assert_eq!(join.status(), StatusCode::OK);
    assert_security_headers(&join, "no-store", "the join page");
    assert_eq!(join.text().await.unwrap(), page);
    // A query string changes nothing.
    assert_eq!(board.get("/?x=1").await.text().await.unwrap(), page);

    for (name, content_type) in [
        ("app.js", "text/javascript; charset=utf-8"),
        ("app.css", "text/css; charset=utf-8"),
    ] {
        let asset = board.get(&format!("/assets/{name}")).await;
        assert_eq!(asset.status(), StatusCode::OK, "{name}");
        assert_eq!(asset.headers()["content-type"], content_type);
        assert_security_headers(&asset, "no-cache", name);
        assert!(!asset.bytes().await.unwrap().is_empty());
    }

    // Unknown names, and anything that is not one plain file name.
    for path in [
        "/assets/nope.js",
        "/assets/index.html",
        "/assets/app.js.map",
        "/assets/",
        "/assets",
        "/assets/a/b",
        "/assets/..%2Findex.html",
        "/assets/%2e%2e/index.html",
        "/index.html",
        "/join/",
        "/joins",
        "/favicon.ico",
    ] {
        let response = board.get(path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_security_headers(&response, "no-store", path);
        let body: ErrorBody = response.json().await.unwrap();
        assert_eq!(body.error.code, codes::NOT_FOUND, "{path}");
    }
    // The pages are read-only.
    for path in ["/", "/join", "/assets/app.js"] {
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            let response = board.send(method, path, None, None).await;
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{path}");
            assert_security_headers(&response, "no-store", path);
        }
    }
    // A local board still answers only to a loopback host, pages included.
    for path in ["/", "/assets/app.js"] {
        let response = board
            .http
            .get(board.url(path))
            .header("host", "evil.test")
            .send()
            .await
            .unwrap();
        assert_security_headers(&response, "no-store", "a refused host");
        expect_error(response, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    }
    board.stop().await;
}

#[tokio::test]
async fn every_api_response_carries_the_security_headers() {
    let board = Board::start().await;
    let root = board.thread("dev", "one").await;
    let id = &root.id;

    for (method, path, body) in [
        (Method::GET, "/v1/health".to_owned(), None),
        (Method::GET, "/v1/whoami".to_owned(), None),
        (Method::GET, "/v1/boards".to_owned(), None),
        (Method::GET, "/v1/boards/general/channels".to_owned(), None),
        (
            Method::GET,
            "/v1/boards/general/threads?channel=dev&html=true".to_owned(),
            None,
        ),
        (
            Method::GET,
            format!("/v1/boards/general/threads/{id}"),
            None,
        ),
        (
            Method::GET,
            "/v1/boards/general/events?after=start".to_owned(),
            None,
        ),
        (
            Method::POST,
            "/v1/boards/general/messages".to_owned(),
            Some(json!({"channel": "dev", "text": "two"})),
        ),
        (
            Method::PATCH,
            format!("/v1/boards/general/messages/{id}"),
            Some(json!({"text": "edited"})),
        ),
        (
            Method::POST,
            format!("/v1/boards/general/messages/{id}/labels"),
            Some(json!({"set": {"a": "b"}})),
        ),
        (
            Method::PUT,
            format!("/v1/boards/general/messages/{id}/reactions/eyes"),
            None,
        ),
        (
            Method::DELETE,
            format!("/v1/boards/general/messages/{id}/reactions/eyes"),
            None,
        ),
        (
            Method::DELETE,
            format!("/v1/boards/general/messages/{id}"),
            None,
        ),
        // Failures too: not found, bad input, wrong method, conflicts.
        (Method::GET, "/v1/nope".to_owned(), None),
        (Method::GET, "/v1/boards/ghost/channels".to_owned(), None),
        (Method::GET, "/v1/boards/general/threads".to_owned(), None),
        (
            Method::DELETE,
            "/v1/boards/general/channels".to_owned(),
            None,
        ),
        (
            Method::PATCH,
            format!("/v1/boards/general/messages/{id}"),
            Some(json!({"text": "after deletion"})),
        ),
        (
            Method::POST,
            "/v1/join".to_owned(),
            Some(json!({"code": "x"})),
        ),
        (Method::POST, "/v1/logout".to_owned(), None),
    ] {
        let what = format!("{method} {path}");
        let response = board.send(method, &path, None, body).await;
        assert_security_headers(&response, "no-store", &what);
    }
    // The stream as well.
    let stream = board
        .get("/v1/boards/general/events/stream?after=start")
        .await;
    assert_eq!(stream.status(), StatusCode::OK);
    assert_security_headers(&stream, "no-store", "the event stream");
    drop(stream);
    // A request with an `Origin` gets no permission to read the answer.
    let cross = board
        .http
        .get(board.url("/v1/boards"))
        .header("origin", "https://evil.test")
        .send()
        .await
        .unwrap();
    assert_security_headers(&cross, "no-store", "a cross-origin read");
    let preflight = board
        .http
        .request(Method::OPTIONS, board.url("/v1/boards/general/messages"))
        .header("origin", "https://evil.test")
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert!(!preflight.status().is_success());
    assert_security_headers(&preflight, "no-store", "a preflight");
    board.stop().await;
}

// ---------------------------------------------------------------------------
// The authorization seam
// ---------------------------------------------------------------------------

/// Denies one action, and everything in channels under `secret`.
struct Deny(Action);

impl Policy for Deny {
    fn decide(
        &self,
        _: &Principal,
        action: Action,
        _: Option<&str>,
        channel: Option<&str>,
    ) -> Decision {
        let secret = channel.is_some_and(|path| path == "secret" || path.starts_with("secret/"));
        if secret || action == self.0 {
            Decision::Deny
        } else {
            Decision::Allow
        }
    }
}

/// Hides the board named `hidden` from reads.
struct HideBoard;

impl Policy for HideBoard {
    fn decide(
        &self,
        _: &Principal,
        action: Action,
        board: Option<&str>,
        _: Option<&str>,
    ) -> Decision {
        if action == Action::Read && board == Some("hidden") {
            Decision::Deny
        } else {
            Decision::Allow
        }
    }
}

#[tokio::test]
async fn the_policy_is_asked_for_every_new_operation() {
    // Seed with the open policy.
    let open = Board::start().await;
    let visible = open.thread("public", "visible, for @local").await;
    let hidden = open.thread("secret/plans", "hidden, for @local").await;
    open.labels(&hidden.id, json!({"set": {"status": "open"}}))
        .await;
    open.send(
        Method::POST,
        "/v1/boards/hidden/messages",
        None,
        Some(json!({"channel": "a", "text": "x"})),
    )
    .await;
    let mut dir = open.stop().await;
    let forbidden = StatusCode::FORBIDDEN;

    // Each action, denied alone, refuses exactly its routes and changes
    // nothing; the others still work. A message in a channel the caller
    // may not read is not found, whatever is asked of it.
    for action in [Action::Edit, Action::Delete, Action::Label, Action::React] {
        let board = Board::start_in(dir, |_| {}, Some(Arc::new(Deny(action)))).await;
        let fresh = board.thread("public", "a fresh message").await;
        let id = &fresh.id;
        let attempts = [
            (
                Action::Edit,
                board.edit_raw(id, None, json!({"text": "x"})).await,
            ),
            (
                Action::Label,
                board.labels_raw(id, json!({"set": {"a": "b"}})).await,
            ),
            (
                Action::React,
                board.react_raw(Method::PUT, id, "eyes", None).await,
            ),
            (
                Action::React,
                board.react_raw(Method::DELETE, id, "eyes", None).await,
            ),
            (Action::Delete, board.delete_raw(id, None, "").await),
        ];
        for (tried, response) in attempts {
            if tried == action {
                expect_error(response, forbidden, codes::FORBIDDEN).await;
            } else {
                assert_eq!(
                    response.status(),
                    StatusCode::OK,
                    "{tried:?} under a policy without {action:?}"
                );
            }
        }
        let now = board.read(id).await.root;
        match action {
            Action::Edit => assert_eq!(now.edited_at, None),
            Action::Delete => assert_eq!(now.deleted_at, None),
            _ => assert!(now.deleted_at.is_some()),
        }
        let denied_kind = match action {
            Action::Edit => event_types::MESSAGE_EDITED,
            Action::Delete => event_types::MESSAGE_DELETED,
            Action::Label => event_types::LABELS_CHANGED,
            _ => event_types::REACTIONS_CHANGED,
        };
        let about: Vec<String> = board
            .events("after=start&limit=1000")
            .await
            .events
            .into_iter()
            .filter(|event| event.message_id == *id)
            .map(|event| event.kind)
            .collect();
        assert!(!about.iter().any(|kind| kind == denied_kind), "{about:?}");

        let missing = StatusCode::NOT_FOUND;
        let response = board.edit_raw(&hidden.id, None, json!({"text": "x"})).await;
        expect_error(response, missing, codes::NOT_FOUND).await;
        let response = board
            .labels_raw(&hidden.id, json!({"set": {"a": "b"}}))
            .await;
        expect_error(response, missing, codes::NOT_FOUND).await;
        for method in [Method::PUT, Method::DELETE] {
            let response = board.react_raw(method, &hidden.id, "eyes", None).await;
            expect_error(response, missing, codes::NOT_FOUND).await;
        }
        let response = board.delete_raw(&hidden.id, None, "").await;
        expect_error(response, missing, codes::NOT_FOUND).await;

        // The filters leak nothing from the hidden channel.
        let mentioned = board.events("after=start&mentions_self=true").await;
        let about: Vec<&str> = mentioned
            .events
            .iter()
            .map(|event| event.message_id.as_str())
            .collect();
        assert_eq!(about, [visible.id.as_str()]);
        let labelled: ThreadList = board
            .get_json("/v1/boards/general/threads?channel=secret/&label=status")
            .await;
        assert!(labelled.threads.is_empty());
        dir = board.stop().await;
    }

    // The hidden message was never touched.
    let data_dir = dir.path().join("data");
    let (text, labels): (String, i64) = open_db(&data_dir)
        .query_row(
            "SELECT text, (SELECT count(*) FROM labels WHERE message_id = m.id)
             FROM messages m WHERE id = ?1",
            [&hidden.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((text.as_str(), labels), ("hidden, for @local", 1));

    // The static files ask the policy too.
    let board = Board::start_in(dir, |_| {}, Some(Arc::new(Deny(Action::Page)))).await;
    for path in ["/", "/join", "/assets/app.js", "/assets/nope"] {
        expect_error(board.get(path).await, forbidden, codes::FORBIDDEN).await;
    }
    let dir = board.stop().await;

    // The board list shows only boards the caller may read.
    let board = Board::start_in(dir, |_| {}, Some(Arc::new(HideBoard))).await;
    let boards: BoardList = board.get_json("/v1/boards").await;
    let names: Vec<&str> = boards.boards.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["general"]);
    let dir = board.stop().await;
    assert!(verify_log(&dir.path().join("data")).unwrap().is_ok());
}
