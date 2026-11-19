//! The HTTP surface: routes, request validation and error mapping.
//!
//! Handlers never log request or message content (design P15).

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use openboard_api::{
    codes, ApiError, ChannelList, ErrorBody, Event, EventsQuery, EventsResponse, Health,
    PostMessageRequest, PostMessageResponse, Thread, ThreadList, ThreadsQuery, CURSOR_START,
    DEFAULT_EVENTS_PER_PAGE, DEFAULT_THREADS_PER_PAGE, IDEMPOTENCY_HEADER, MAX_EVENTS_PER_PAGE,
    MAX_THREADS_PER_PAGE, MAX_WAIT_SECS, PROFILE_HEADER,
};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::authz::{Action, Authorizer, Principal};
use crate::chain::hex;
use crate::names::{self, ChannelSel};
use crate::store::{
    event_cursor, parse_cursor, thread_cursor, BoardRef, EventFilter, NewPost, PostTarget, Store,
    StoreError,
};

/// Longest idempotency key, in bytes.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;
/// How often the event stream sends a comment to keep the connection open.
const STREAM_KEEP_ALIVE: Duration = Duration::from_secs(15);

/// Wakes event waiters when a board's log grows. It holds one entry per
/// board that exists, never one for a name that was merely asked about.
#[derive(Default)]
struct Hub {
    boards: Mutex<HashMap<i64, watch::Sender<i64>>>,
}

impl Hub {
    fn sender(&self, board_id: i64) -> watch::Sender<i64> {
        self.boards
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry(board_id)
            .or_insert_with(|| watch::channel(0).0)
            .clone()
    }

    /// A receiver that reports every append committed after this call.
    fn subscribe(&self, board_id: i64) -> watch::Receiver<i64> {
        self.sender(board_id).subscribe()
    }

    fn publish(&self, board_id: i64, seq: i64) {
        self.sender(board_id).send_if_modified(|head| {
            let grew = seq > *head;
            if grew {
                *head = seq;
            }
            grew
        });
    }
}

pub(crate) struct AppState {
    store: Store,
    authz: Authorizer,
    hub: Hub,
    max_message_chars: usize,
    /// Turns true when the server starts shutting down.
    shutdown: watch::Receiver<bool>,
    /// Dropped with the state, after the store above it has closed. The
    /// server waits for that before it reports that it has stopped.
    _in_use: mpsc::Sender<()>,
}

type Shared = Arc<AppState>;

pub(crate) fn router(
    store: Store,
    authz: Authorizer,
    max_message_chars: usize,
    shutdown: watch::Receiver<bool>,
    in_use: mpsc::Sender<()>,
) -> Router {
    let state = Arc::new(AppState {
        store,
        authz,
        hub: Hub::default(),
        max_message_chars,
        shutdown,
        _in_use: in_use,
    });
    // Room for the largest allowed text with every character escaped.
    let body_limit = max_message_chars
        .saturating_mul(12)
        .saturating_add(16 * 1024);
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/boards/{board}/messages", post(post_message))
        .route("/v1/boards/{board}/channels", get(list_channels))
        .route("/v1/boards/{board}/threads", get(list_threads))
        .route("/v1/boards/{board}/threads/{thread_id}", get(get_thread))
        .route("/v1/boards/{board}/events", get(get_events))
        .route("/v1/boards/{board}/events/stream", get(stream_events))
        .fallback(unknown_route)
        .method_not_allowed_fallback(wrong_method)
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(middleware::from_fn(loopback_host_only))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct AppError {
    status: StatusCode,
    code: &'static str,
    message: String,
    next: Option<String>,
}

impl AppError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        let status = match code {
            codes::NOT_FOUND => StatusCode::NOT_FOUND,
            codes::FORBIDDEN => StatusCode::FORBIDDEN,
            codes::IDEMPOTENCY_CONFLICT => StatusCode::CONFLICT,
            codes::MESSAGE_TOO_LARGE => StatusCode::PAYLOAD_TOO_LARGE,
            codes::INTERNAL => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        };
        Self {
            status,
            code,
            message: message.into(),
            next: None,
        }
    }

    fn status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    fn next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(next.into());
        self
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_REQUEST, message)
    }

    fn internal(detail: &dyn std::fmt::Display) -> Self {
        // Storage errors describe SQL and schema, never bound values, so this
        // line cannot carry message text.
        eprintln!("openboard-server: internal error: {detail}");
        Self::new(codes::INTERNAL, "the server failed to complete the request")
    }

    fn forbidden() -> Self {
        Self::new(
            codes::FORBIDDEN,
            "the caller is not allowed to perform this action",
        )
    }

    fn invalid_cursor() -> Self {
        Self::new(
            codes::INVALID_CURSOR,
            "the cursor was not issued by this board",
        )
        .next("omit the cursor to start from now, or pass after=start to read events from the beginning")
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ApiError {
                code: self.code.to_owned(),
                message: self.message,
                next: self.next,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

impl From<StoreError> for AppError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::IdempotencyConflict => AppError::new(
                codes::IDEMPOTENCY_CONFLICT,
                "the idempotency key was already used with a different request",
            )
            .next("use a new idempotency key for a new message"),
            StoreError::ThreadMissing => thread_not_found(),
            other => AppError::internal(&other),
        }
    }
}

fn board_not_found() -> AppError {
    AppError::new(codes::NOT_FOUND, "no such board")
        .next("a board is created by the first post to it; check the board name")
}

fn thread_not_found() -> AppError {
    AppError::new(codes::NOT_FOUND, "no such thread on this board")
        .next("list threads in a channel to find a thread ID")
}

type ApiResult<T> = Result<T, AppError>;

async fn unknown_route() -> AppError {
    AppError::new(codes::NOT_FOUND, "no such route").next("see the route table in openboard-api")
}

async fn wrong_method() -> AppError {
    AppError::invalid("this route does not support that method")
        .status(StatusCode::METHOD_NOT_ALLOWED)
}

/// Local mode has no auth, so a web page must not be able to reach the board
/// by pointing a hostname it controls at loopback (DNS rebinding). Requests
/// whose `Host` is not a loopback name are refused.
async fn loopback_host_only(request: Request, next: Next) -> Response {
    let allowed = match request.headers().get(header::HOST) {
        None => true,
        Some(value) => value.to_str().is_ok_and(is_loopback_host),
    };
    if allowed {
        next.run(request).await
    } else {
        AppError::new(
            codes::FORBIDDEN,
            "a local board only answers requests addressed to a loopback host",
        )
        .into_response()
    }
}

fn is_loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(name, _port)| name)
    };
    name.eq_ignore_ascii_case("localhost")
        || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

// ---------------------------------------------------------------------------
// Shared request handling
// ---------------------------------------------------------------------------

/// Who is calling. In local mode this is always the local principal; the
/// profile header only picks a label.
struct Caller {
    principal: Principal,
    profile: Option<String>,
}

impl Caller {
    fn from_headers(headers: &HeaderMap) -> ApiResult<Self> {
        let profile = match headers.get(PROFILE_HEADER) {
            None => None,
            Some(value) => {
                let name = value.to_str().unwrap_or_default();
                names::validate_profile(name).map_err(|reason| {
                    AppError::new(codes::INVALID_PROFILE, reason)
                        .next(format!("fix or drop the {PROFILE_HEADER} header"))
                })?;
                Some(name.to_owned())
            }
        };
        Ok(Self {
            principal: Principal::local(),
            profile,
        })
    }
}

impl AppState {
    /// Runs storage work off the async runtime. The work runs to the end
    /// even if the request that asked for it is dropped, and it keeps the
    /// state, and so the store, alive until then.
    async fn db<T, F>(self: &Arc<Self>, work: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&AppState) -> Result<T, StoreError> + Send + 'static,
    {
        let state = Arc::clone(self);
        match tokio::task::spawn_blocking(move || work(&state)).await {
            Ok(result) => result.map_err(AppError::from),
            Err(join_error) => Err(AppError::internal(&join_error)),
        }
    }

    /// The one place handlers ask whether the caller may do something.
    fn allows(&self, caller: &Caller, action: Action, board: &str, channel: Option<&str>) -> bool {
        self.authz
            .authorize(&caller.principal, action, Some(board), channel)
            .is_allowed()
    }

    fn require(
        &self,
        caller: &Caller,
        action: Action,
        board: &str,
        channel: Option<&str>,
    ) -> ApiResult<()> {
        if self.allows(caller, action, board, channel) {
            Ok(())
        } else {
            Err(AppError::forbidden())
        }
    }

    /// Validates a board name and looks the board up. Nothing is created:
    /// only a successful post creates a board.
    async fn find_board(self: &Arc<Self>, name: &str) -> ApiResult<Option<BoardRef>> {
        names::validate_board(name)
            .map_err(|reason| AppError::new(codes::INVALID_BOARD, reason))?;
        let name = name.to_owned();
        self.db(move |state| state.store.find_board(&name)).await
    }

    /// The board a read is addressed to. Fails with `not_found` when there
    /// is no such board.
    async fn board(
        self: &Arc<Self>,
        caller: &Caller,
        name: Result<Path<String>, PathRejection>,
    ) -> ApiResult<BoardRef> {
        let Path(name) = name.map_err(|_| bad_path())?;
        self.board_named(caller, &name).await
    }

    async fn board_named(self: &Arc<Self>, caller: &Caller, name: &str) -> ApiResult<BoardRef> {
        match self.find_board(name).await? {
            Some(board) => Ok(board),
            None => Err(self.missing_board(caller, name)),
        }
    }

    /// The error for a request addressed to a board that does not exist. A
    /// caller who may not read the board is told so, exactly as for a board
    /// that exists, and so learns nothing about which names are taken.
    fn missing_board(&self, caller: &Caller, name: &str) -> AppError {
        match self.require(caller, Action::Read, name, None) {
            Ok(()) => board_not_found(),
            Err(denied) => denied,
        }
    }
}

fn bad_path() -> AppError {
    AppError::invalid("the request path is malformed")
}

fn bad_query(rejection: QueryRejection) -> AppError {
    AppError::invalid(format!(
        "the query string is malformed: {}",
        rejection.body_text()
    ))
}

fn invalid_channel(reason: &'static str) -> AppError {
    AppError::new(codes::INVALID_CHANNEL, reason)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health(State(state): State<Shared>, headers: HeaderMap) -> ApiResult<Json<Health>> {
    let caller = Caller::from_headers(&headers)?;
    if !state
        .authz
        .authorize(&caller.principal, Action::Health, None, None)
        .is_allowed()
    {
        return Err(AppError::forbidden());
    }
    Ok(Json(Health {
        status: "ok".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        mode: "local".to_owned(),
    }))
}

async fn post_message(
    State(state): State<Shared>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<(StatusCode, Json<PostMessageResponse>)> {
    let caller = Caller::from_headers(&headers)?;
    let Path(board_name) = board.map_err(|_| bad_path())?;
    names::validate_board(&board_name)
        .map_err(|reason| AppError::new(codes::INVALID_BOARD, reason))?;
    let limit = state.max_message_chars;
    let too_large = |detail: String| {
        AppError::new(codes::MESSAGE_TOO_LARGE, detail)
            .next("shorten the message or split it across replies")
    };

    // Requiring JSON keeps a web page from posting here with a plain form.
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return Err(
            AppError::invalid("the request body must be sent as application/json")
                .status(StatusCode::UNSUPPORTED_MEDIA_TYPE),
        );
    }
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            too_large(format!(
                "the request body is too large for a message of at most {limit} characters"
            ))
        } else {
            AppError::invalid("the request body could not be read")
        }
    })?;
    let request: PostMessageRequest = serde_json::from_slice(&body).map_err(|error| {
        // Only the position is reported. A serde message can quote the input.
        AppError::invalid(format!(
            "the request body is not a valid post request (JSON {:?} error at line {}, column {})",
            error.classify(),
            error.line(),
            error.column()
        ))
        .next("send JSON with `text` and exactly one of `channel` or `thread_id`")
    })?;

    if request.text.trim().is_empty() {
        return Err(AppError::invalid("the message text is empty"));
    }
    let chars = request.text.chars().count();
    if chars > limit {
        return Err(too_large(format!(
            "the message has {chars} characters and the limit is {limit}"
        )));
    }
    let key = match request.idempotency_key.clone() {
        Some(key) => Some(key),
        None => match headers.get(IDEMPOTENCY_HEADER) {
            None => None,
            Some(value) => Some(
                value
                    .to_str()
                    .map_err(|_| AppError::invalid("the idempotency key is not valid text"))?
                    .to_owned(),
            ),
        },
    };
    if key
        .as_ref()
        .is_some_and(|key| key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_LEN)
    {
        return Err(AppError::invalid("an idempotency key is 1 to 255 bytes"));
    }

    // The request is well-formed. Only now is the board looked at, and it is
    // created, with the message, only if everything below allows the post.
    let found = state.find_board(&board_name).await?;
    let (target, channel, thread_id) = match (&request.channel, &request.thread_id) {
        (Some(channel), None) => {
            names::validate_channel(channel).map_err(invalid_channel)?;
            let target = match found {
                Some(board) => {
                    state.require(&caller, Action::Post, &board.name, Some(channel))?;
                    let (board_id, path) = (board.id, channel.clone());
                    if !state
                        .db(move |state| state.store.channel_exists(board_id, &path))
                        .await?
                    {
                        state.require(
                            &caller,
                            Action::CreateChannel,
                            &board.name,
                            Some(channel),
                        )?;
                    }
                    PostTarget::Board(board)
                }
                None => {
                    state.require(&caller, Action::CreateBoard, &board_name, None)?;
                    state.require(&caller, Action::Post, &board_name, Some(channel))?;
                    state.require(&caller, Action::CreateChannel, &board_name, Some(channel))?;
                    PostTarget::NewBoard(board_name)
                }
            };
            (target, channel.clone(), None)
        }
        (None, Some(target)) => {
            // A reply needs a thread, so it never creates a board.
            let board = found.ok_or_else(|| state.missing_board(&caller, &board_name))?;
            let (board_id, target) = (board.id, target.clone());
            let (root, channel) = state
                .db(move |state| state.store.thread_of(board_id, &target))
                .await?
                .filter(|(_, channel)| {
                    state.allows(&caller, Action::Read, &board.name, Some(channel))
                })
                .ok_or_else(thread_not_found)?;
            state.require(&caller, Action::Post, &board.name, Some(&channel))?;
            (PostTarget::Board(board), channel, Some(root))
        }
        _ => {
            return Err(AppError::invalid(
                "set exactly one of `channel` (start a thread) or `thread_id` (reply)",
            ))
        }
    };

    let actor = caller.principal.actor(caller.profile.as_deref());
    let idempotency = key.map(|key| (key, request_fingerprint(&request)));
    let post = NewPost {
        channel,
        thread_id,
        actor,
        principal_id: caller.principal.id.clone(),
        text: request.text,
        idempotency,
    };
    // Waiters are told from the blocking task, straight after the commit.
    // The task finishes even if this request is dropped because its client
    // went away, so a committed post is always announced. An idempotent
    // retry announces the original position again, which wakes nobody who
    // has already seen it.
    let posted = state
        .db(move |state| {
            let posted = state.store.post(target, post)?;
            state.hub.publish(posted.board.id, posted.seq);
            Ok(posted)
        })
        .await?;
    let status = if posted.duplicate {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((
        status,
        Json(PostMessageResponse {
            message: posted.message,
            cursor: event_cursor(&posted.board.tag, posted.seq),
            duplicate: posted.duplicate,
        }),
    ))
}

/// A digest of what a post asks for, used to tell a retry from a reuse of
/// the same idempotency key for something else.
fn request_fingerprint(request: &PostMessageRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"openboard.post.v1\n");
    for part in [
        request.channel.as_deref(),
        request.thread_id.as_deref(),
        Some(request.text.as_str()),
    ] {
        match part {
            Some(value) => {
                hasher.update(format!("{}:", value.len()).as_bytes());
                hasher.update(value.as_bytes());
            }
            None => hasher.update(b"-"),
        }
        hasher.update(b"\n");
    }
    hex(&hasher.finalize())
}

async fn list_channels(
    State(state): State<Shared>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
) -> ApiResult<Json<ChannelList>> {
    let caller = Caller::from_headers(&headers)?;
    let board = state.board(&caller, board).await?;
    state.require(&caller, Action::Read, &board.name, None)?;
    let board_id = board.id;
    let mut channels = state
        .db(move |state| state.store.channels(board_id))
        .await?;
    channels
        .retain(|channel| state.allows(&caller, Action::Read, &board.name, Some(&channel.path)));
    Ok(Json(ChannelList { channels }))
}

async fn list_threads(
    State(state): State<Shared>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    query: Result<Query<ThreadsQuery>, QueryRejection>,
) -> ApiResult<Json<ThreadList>> {
    let caller = Caller::from_headers(&headers)?;
    let board = state.board(&caller, board).await?;
    let Query(query) = query.map_err(bad_query)?;
    let channel = names::parse_channel_sel(&query.channel).map_err(invalid_channel)?;
    let exact = match &channel {
        ChannelSel::Exact(path) => Some(path.as_str()),
        ChannelSel::Subtree(_) => None,
    };
    state.require(&caller, Action::Read, &board.name, exact)?;
    let limit = page_size(query.limit, DEFAULT_THREADS_PER_PAGE, MAX_THREADS_PER_PAGE)?;
    let before = match &query.cursor {
        None => None,
        Some(cursor) => {
            Some(parse_cursor("th", &board.tag, cursor).ok_or_else(AppError::invalid_cursor)?)
        }
    };

    let listed_on = board.clone();
    let page = state
        .db(move |state| state.store.threads(&listed_on, &channel, before, limit))
        .await?;
    let next_cursor = match page.threads.last() {
        Some((last_seq, _)) if page.more => Some(thread_cursor(&board.tag, *last_seq)),
        _ => None,
    };
    let threads = page
        .threads
        .into_iter()
        .map(|(_, summary)| summary)
        .filter(|summary| {
            state.allows(
                &caller,
                Action::Read,
                &board.name,
                Some(&summary.root.channel),
            )
        })
        .collect();
    Ok(Json(ThreadList {
        threads,
        next_cursor,
    }))
}

async fn get_thread(
    State(state): State<Shared>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> ApiResult<Json<Thread>> {
    let caller = Caller::from_headers(&headers)?;
    let Path((board, thread_id)) = path.map_err(|_| bad_path())?;
    let board = state.board_named(&caller, &board).await?;
    state.require(&caller, Action::Read, &board.name, None)?;
    let board_id = board.id;
    // A reply's ID is accepted and resolves to the thread that holds it.
    let (root, _channel) = state
        .db(move |state| state.store.thread_of(board_id, &thread_id))
        .await?
        .filter(|(_, channel)| state.allows(&caller, Action::Read, &board.name, Some(channel)))
        .ok_or_else(thread_not_found)?;
    let read_on = board.clone();
    let thread = state
        .db(move |state| state.store.thread(&read_on, &root))
        .await?
        .ok_or_else(thread_not_found)?;
    Ok(Json(thread))
}

fn page_size(requested: Option<u32>, default: u32, max: u32) -> ApiResult<u32> {
    match requested {
        None => Ok(default),
        Some(0) => Err(AppError::invalid("`limit` must be at least 1")),
        Some(limit) => Ok(limit.min(max)),
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// A resolved events request, shared by the long-poll and the stream.
struct EventScan {
    state: Shared,
    caller: Caller,
    board: BoardRef,
    filter: EventFilter,
    limit: u32,
    /// Position every earlier event has been considered up to.
    after: i64,
    wake: watch::Receiver<i64>,
}

impl EventScan {
    /// `resume` is the cursor to continue from: the `after` parameter or,
    /// for the stream, a `Last-Event-ID` header when one is sent.
    async fn open(
        state: Shared,
        headers: &HeaderMap,
        board: Result<Path<String>, PathRejection>,
        query: &EventsQuery,
        resume: Option<&str>,
    ) -> ApiResult<Self> {
        let caller = Caller::from_headers(headers)?;
        let board = state.board(&caller, board).await?;
        let channel = match &query.channel {
            None => None,
            Some(raw) => Some(names::parse_channel_sel(raw).map_err(invalid_channel)?),
        };
        let exact = match &channel {
            Some(ChannelSel::Exact(path)) => Some(path.as_str()),
            _ => None,
        };
        state.require(&caller, Action::Read, &board.name, exact)?;
        let limit = page_size(query.limit, DEFAULT_EVENTS_PER_PAGE, MAX_EVENTS_PER_PAGE)?;
        // Like `GET /threads/{id}`, the filter takes the ID of any message
        // in the thread and resolves it to the thread's root.
        let thread = match &query.thread {
            None => None,
            Some(id) => {
                let (board_id, id) = (board.id, id.clone());
                let (root, _channel) = state
                    .db(move |state| state.store.thread_of(board_id, &id))
                    .await?
                    .filter(|(_, channel)| {
                        state.allows(&caller, Action::Read, &board.name, Some(channel))
                    })
                    .ok_or_else(thread_not_found)?;
                Some(root)
            }
        };

        // Subscribe before reading the head, so no append can fall between.
        let wake = state.hub.subscribe(board.id);
        let board_id = board.id;
        let head = state.db(move |state| state.store.head(board_id)).await?;
        let after = match resume {
            None => head,
            Some(CURSOR_START) => 0,
            Some(cursor) => parse_cursor("ev", &board.tag, cursor)
                .filter(|seq| *seq <= head)
                .ok_or_else(AppError::invalid_cursor)?,
        };
        Ok(Self {
            state,
            caller,
            board,
            filter: EventFilter {
                channel,
                thread,
                not_from: query.not_from.clone(),
            },
            limit,
            after,
            wake,
        })
    }

    /// Reads the next page of events the caller may see and advances
    /// `after` past everything considered. Returns the events and whether
    /// more may already be waiting.
    async fn next_page(&mut self) -> ApiResult<(Vec<Event>, bool)> {
        let (board, filter, after, limit) = (
            self.board.clone(),
            self.filter.clone(),
            self.after,
            self.limit,
        );
        let page = self
            .state
            .db(move |state| state.store.events_after(&board, after, &filter, limit))
            .await?;
        self.after = page.scanned_to;
        let events = page
            .events
            .into_iter()
            .map(|(_, event)| event)
            .filter(|event| {
                self.state.allows(
                    &self.caller,
                    Action::Read,
                    &self.board.name,
                    Some(&event.channel),
                )
            })
            .collect();
        Ok((events, page.more))
    }

    fn cursor(&self) -> String {
        event_cursor(&self.board.tag, self.after)
    }
}

/// Resolves when the server starts shutting down.
async fn stopping(shutdown: &mut watch::Receiver<bool>) {
    if shutdown.wait_for(|stopping| *stopping).await.is_err() {
        // The server handle is gone, which also means shutdown.
    }
}

async fn get_events(
    State(state): State<Shared>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> ApiResult<Json<EventsResponse>> {
    let Query(query) = query.map_err(bad_query)?;
    let wait = Duration::from_secs(query.wait.unwrap_or(0).min(MAX_WAIT_SECS));
    let deadline = Instant::now() + wait;
    let mut shutdown = state.shutdown.clone();
    let mut scan = EventScan::open(state, &headers, board, &query, query.after.as_deref()).await?;

    let mut expired = wait.is_zero();
    loop {
        let (events, more) = scan.next_page().await?;
        if !events.is_empty() || (!more && expired) {
            return Ok(Json(EventsResponse {
                events,
                cursor: scan.cursor(),
            }));
        }
        if more {
            // The page filled with events the caller may not see. Keep going.
            continue;
        }
        tokio::select! {
            changed = scan.wake.changed() => expired = changed.is_err(),
            () = tokio::time::sleep_until(deadline) => expired = true,
            () = stopping(&mut shutdown) => expired = true,
        }
    }
}

struct EventStream(mpsc::Receiver<SseEvent>);

impl futures_core::Stream for EventStream {
    type Item = Result<SseEvent, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx).map(|item| item.map(Ok))
    }
}

async fn stream_events(
    State(state): State<Shared>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> ApiResult<Response> {
    let Query(query) = query.map_err(bad_query)?;
    // A reconnecting EventSource repeats its original URL, `after` and all,
    // and adds the last ID it saw. That ID is the newer position, so it
    // wins; otherwise every reconnect would replay from the stale `after`.
    let last_event_id = match headers.get("last-event-id") {
        None => None,
        Some(value) => Some(value.to_str().map_err(|_| AppError::invalid_cursor())?),
    }
    // An empty ID is how a client says it has none.
    .filter(|id| !id.is_empty());
    let resume = last_event_id.or(query.after.as_deref());
    let mut shutdown = state.shutdown.clone();
    let mut scan = EventScan::open(state, &headers, board, &query, resume).await?;

    let (tx, rx) = mpsc::channel::<SseEvent>(16);
    tokio::spawn(async move {
        loop {
            let Ok((events, more)) = scan.next_page().await else {
                return;
            };
            for event in events {
                let Ok(data) = serde_json::to_string(&event) else {
                    return;
                };
                let frame = SseEvent::default().id(event.cursor).data(data);
                if tx.send(frame).await.is_err() {
                    return;
                }
            }
            if more {
                continue;
            }
            tokio::select! {
                changed = scan.wake.changed() => if changed.is_err() { return },
                () = tx.closed() => return,
                () = stopping(&mut shutdown) => return,
            }
        }
    });

    Ok(Sse::new(EventStream(rx))
        .keep_alive(
            KeepAlive::new()
                .interval(STREAM_KEEP_ALIVE)
                .text("keep-alive"),
        )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::is_loopback_host;

    #[test]
    fn loopback_hosts() {
        for ok in [
            "localhost",
            "LOCALHOST:7878",
            "127.0.0.1",
            "127.0.0.1:7878",
            "[::1]",
            "[::1]:7878",
        ] {
            assert!(is_loopback_host(ok), "{ok}");
        }
        for bad in [
            "example.com",
            "example.com:7878",
            "192.168.1.5:7878",
            "localhost.evil.test",
            "",
        ] {
            assert!(!is_loopback_host(bad), "{bad}");
        }
    }
}
