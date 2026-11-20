//! The HTTP surface: routes, request validation and error mapping.
//!
//! Handlers never log request or message content, tokens, session cookie
//! values, or enrolment and sign-in codes (design P15).
//!
//! Every request passes [`gate`] before anything else looks at it. On a
//! local board the gate checks the `Host` header. On a network board it
//! authenticates the bearer token or, when the request has no
//! `Authorization` header, the browser session cookie, so a caller without
//! a valid credential learns nothing about boards, routes or the shape of
//! valid requests. The only routes it lets through without one are the
//! health check, enrolment, sign-in and the web UI's static files, which
//! hold no board content. In both modes the gate then applies the two
//! anti-CSRF checks of `openboard_api::web` to every write.
//!
//! Every response, from a handler, the gate or a fallback, leaves through
//! [`security_headers`].

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::rejection::{BytesRejection, PathRejection, QueryRejection};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use openboard_api::auth::{JoinRequest, JoinResponse, WhoAmI};
use openboard_api::web::{session_cookie_name, LoginRequest, Session, CSRF_HEADER};
use openboard_api::{
    codes, event_types, ApiError, BoardList, ChannelList, DeleteMessageQuery, EditMessageRequest,
    ErrorBody, Event, EventsQuery, EventsResponse, Health, LabelsRequest, Message,
    PostMessageRequest, PostMessageResponse, ReadOptions, Thread, ThreadList, ThreadsQuery,
    API_LEVEL, API_LEVEL_HEADER, CURSOR_START, DEFAULT_EVENTS_PER_PAGE, DEFAULT_THREADS_PER_PAGE,
    IDEMPOTENCY_HEADER, MAX_EVENTS_PER_PAGE, MAX_HTML_BYTES_PER_RESPONSE,
    MAX_REACTION_NAMES_PER_MESSAGE, MAX_THREADS_PER_PAGE, MAX_WAIT_SECS, PROFILE_HEADER,
    VERSION_HEADER,
};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::authz::{Action, Authorizer, Principal};
use crate::chain::hex;
use crate::grants;
use crate::identity::{
    self, log_kinds, session_lookup_hash, token_lookup_hash, AuthOutcome, Credential, InviteKind,
    LogEntry,
};
use crate::limits::{address_key, Hit, Limits, LogRetention, RateTable, WaiterGuard, Waiters};
use crate::markdown;
use crate::names::{self, ChannelSel};
use crate::pages;
use crate::serve::ClientAddr;
use crate::store::{
    event_cursor, parse_cursor, thread_cursor, BoardRef, Changed, EventFilter, LabelFilter,
    MentionInput, MentionedSelf, MessageMeta, NewPost, PostTarget, Store, StoreError, Writer,
    MAX_LABELS_PER_MESSAGE,
};
use crate::Mode;

mod access;
mod grants_routes;
mod tasks;

/// Longest idempotency key, in bytes.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;
/// How often the event stream sends a comment to keep the connection open.
const STREAM_KEEP_ALIVE: Duration = Duration::from_secs(15);
/// Events an event stream may have ready beyond the one being written. They
/// have passed the credential check, so this is kept small: it is what a
/// reader that stalls can have queued when its credential is revoked, and a
/// revocation discards it.
const STREAM_BUFFER: usize = 4;
/// How often the upkeep task tries again to empty the write-ahead log while
/// a scrub is pending; see `Store::scrub`. Between tries it costs nothing.
const SCRUB_RETRY_EVERY: Duration = Duration::from_millis(500);
/// Largest accepted body of an enrolment or sign-in request, which holds
/// one code.
const MAX_JOIN_BODY: usize = 4096;

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
    mode: Mode,
    max_message_chars: usize,
    limits: Limits,
    /// Requests per credential.
    requests: RateTable<String>,
    retention: LogRetention,
    /// Failed enrolment and sign-in attempts per client address.
    joins: RateTable<IpAddr>,
    /// Failed authentications per client address.
    auth_failures: RateTable<IpAddr>,
    /// Failure entries written to the security log, over all clients.
    failure_log: RateTable<()>,
    /// Failure entries written to the security log, per client address.
    failure_log_by_address: RateTable<IpAddr>,
    /// Open long-polls and streams per credential.
    waiters: Arc<Waiters>,
    /// Turns true when the server starts shutting down.
    shutdown: watch::Receiver<bool>,
    /// Dropped with the state, after the store above it has closed. The
    /// server waits for that before it reports that it has stopped.
    _in_use: mpsc::Sender<()>,
}

type Shared = Arc<AppState>;

/// What [`router`] is built from.
pub(crate) struct Parts {
    pub store: Store,
    pub authz: Authorizer,
    pub mode: Mode,
    pub max_message_chars: usize,
    pub limits: Limits,
    pub retention: LogRetention,
    pub shutdown: watch::Receiver<bool>,
    pub in_use: mpsc::Sender<()>,
}

/// Upkeep a running server does beside answering requests: it trims the
/// security log to its retention, it ends task leases that have run out,
/// and it empties the write-ahead log of
/// removed content when the request that removed it could not, because
/// another process was reading the database. It shares the store with the
/// router and lets go of it when the server stops.
pub(crate) struct Maintenance(Shared);

impl Maintenance {
    /// Runs until `stop` turns true or its sender is gone.
    pub(crate) async fn run(self, mut stop: watch::Receiver<bool>) {
        let state = self.0;
        let period = state.retention.check_every.max(Duration::from_millis(10));
        let mut timer = tokio::time::interval_at(Instant::now() + period, period);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut scrub =
            tokio::time::interval_at(Instant::now() + SCRUB_RETRY_EVERY, SCRUB_RETRY_EVERY);
        scrub.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let prune = tokio::select! {
                () = stopping(&mut stop) => return,
                _ = timer.tick() => true,
                _ = scrub.tick() => false,
            };
            if prune {
                let pruned = state
                    .db(|state| state.store.prune_log(&state.retention))
                    .await;
                if pruned.is_err() {
                    eprintln!("openboard-server: could not trim the security log");
                }
            } else {
                // Leases that ran out return their tasks to the queue, and
                // whoever waits on the board hears of it.
                match state.db(|state| state.store.expire_leases()).await {
                    Ok(appended) => {
                        for (board_id, seq) in appended {
                            state.hub.publish(board_id, seq);
                        }
                    }
                    Err(_) => eprintln!("openboard-server: could not end expired task leases"),
                }
                if state.store.scrub_pending() {
                    // Each try gives up at once if the log is still in use,
                    // so it holds the store no longer than a small write does.
                    let _ = state.db(|state| Ok(state.store.scrub())).await;
                }
            }
        }
    }
}

pub(crate) fn router(parts: Parts) -> (Router, Maintenance) {
    let max_message_chars = parts.max_message_chars;
    let state = Arc::new(AppState {
        store: parts.store,
        authz: parts.authz,
        hub: Hub::default(),
        mode: parts.mode,
        max_message_chars,
        limits: parts.limits,
        retention: parts.retention,
        requests: RateTable::new(),
        joins: RateTable::new(),
        auth_failures: RateTable::new(),
        failure_log: RateTable::new(),
        failure_log_by_address: RateTable::new(),
        waiters: Arc::new(Waiters::default()),
        shutdown: parts.shutdown,
        _in_use: parts.in_use,
    });
    // Room for the largest allowed text with every character escaped.
    let body_limit = max_message_chars
        .saturating_mul(12)
        .saturating_add(16 * 1024);
    let router = Router::new()
        .route(HEALTH_PATH, get(health))
        .route(JOIN_PATH, post(join))
        .route(LOGIN_PATH, post(web_login))
        .route(SESSION_PATH, get(web_session))
        .route("/v1/whoami", get(whoami))
        .route("/v1/logout", post(logout))
        .route(pages::INDEX_PATH, get(page))
        .route(pages::JOIN_PATH, get(page))
        .route("/assets/{file}", get(asset))
        .route("/v1/boards", get(list_boards))
        .route("/v1/boards/{board}/messages", post(post_message))
        .route(
            "/v1/boards/{board}/messages/{message_id}",
            patch(edit_message).delete(delete_message),
        )
        .route(
            "/v1/boards/{board}/messages/{message_id}/labels",
            post(label_message),
        )
        .route(
            "/v1/boards/{board}/messages/{message_id}/reactions/{emoji}",
            put(add_reaction).delete(remove_reaction),
        )
        .route(
            "/v1/boards/{board}/channels",
            get(list_channels).post(grants_routes::create_channel),
        )
        .route(
            "/v1/boards/{board}/grants",
            get(grants_routes::list_grants).post(grants_routes::add_grant),
        )
        .route(
            "/v1/boards/{board}/grants/{grant_id}",
            axum::routing::delete(grants_routes::remove_grant),
        )
        .route("/v1/boards/{board}/explain", get(grants_routes::explain))
        .route("/v1/boards/{board}/threads", get(list_threads))
        .route("/v1/boards/{board}/threads/{thread_id}", get(get_thread))
        .route("/v1/boards/{board}/events", get(get_events))
        .route("/v1/boards/{board}/events/stream", get(stream_events))
        .route(
            "/v1/boards/{board}/tasks",
            post(tasks::create_task).get(tasks::list_tasks),
        )
        .route(
            "/v1/boards/{board}/tasks/batch",
            post(tasks::create_tasks).layer(DefaultBodyLimit::max(
                body_limit.saturating_mul(openboard_api::tasks::MAX_TASKS_PER_BATCH),
            )),
        )
        .route("/v1/boards/{board}/tasks/claim", post(tasks::claim_ready))
        .route("/v1/boards/{board}/tasks/{task_id}", get(tasks::get_task))
        .route(
            "/v1/boards/{board}/tasks/{task_id}/{action}",
            post(tasks::task_action),
        )
        .fallback(unknown_route)
        .method_not_allowed_fallback(wrong_method)
        .layer(DefaultBodyLimit::max(body_limit))
        // It runs before routing, extraction and every handler.
        .layer(middleware::from_fn_with_state(Arc::clone(&state), gate))
        .with_state(Arc::clone(&state));
    // The outermost layer, around the whole router, so that what the gate
    // refuses carries the headers too, and so that it sees, and can take
    // out, what routing adds to an answer; see `security_headers`.
    let router = Router::new()
        .fallback_service(router)
        .layer(middleware::from_fn(security_headers));
    (router, Maintenance(state))
}

/// The API routes a network board answers without a credential.
const HEALTH_PATH: &str = "/v1/health";
const JOIN_PATH: &str = "/v1/join";
const LOGIN_PATH: &str = "/v1/web/login";
/// Describes the caller's session. It needs a credential like any route.
const SESSION_PATH: &str = "/v1/web/session";

/// Whether a network board answers this request without a credential: the
/// health check, enrolment, a `POST` of a sign-in, and a `GET` of the web
/// UI's page or of one file directly under `/assets/`. Nothing else is
/// exempt, whatever its method.
fn needs_no_token(method: &Method, path: &str) -> bool {
    if path == HEALTH_PATH || path == JOIN_PATH {
        return true;
    }
    if path == LOGIN_PATH {
        return method == Method::POST;
    }
    if method != Method::GET {
        return false;
    }
    path == pages::INDEX_PATH
        || path == pages::JOIN_PATH
        || path
            .strip_prefix(pages::ASSETS_PREFIX)
            .is_some_and(|file| !file.is_empty() && !file.contains('/'))
}

/// The content security policy of every response; see `openboard_api::web`.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
     img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'";

/// Adds the headers every response carries, pages and API alike. Nothing
/// may be cached except an asset that was found, which says for itself how.
/// No `Access-Control-*` header is ever set, so no other origin can read a
/// response.
async fn security_headers(request: Request, next: Next) -> Response {
    let is_asset = request.uri().path().starts_with(pages::ASSETS_PREFIX);
    let mut response = next.run(request).await;
    let cacheable = is_asset && response.status() == StatusCode::OK;
    let refused = response.status() == StatusCode::UNAUTHORIZED;
    let headers = response.headers_mut();
    // Routing adds `Allow` to an answer for a known path with the wrong
    // method, even when the gate refused it first. A refusal must not say
    // which paths exist, so it never carries one.
    if refused {
        headers.remove(header::ALLOW);
    }
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if !cacheable {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    // What this server is, the same on every response, so that a client
    // learns it from whatever it asked and a refusal looks like any other.
    headers.insert(
        HeaderName::from_static(VERSION_HEADER),
        HeaderValue::from_static(openboard_api::build::VERSION),
    );
    headers.insert(
        HeaderName::from_static(API_LEVEL_HEADER),
        HeaderValue::from(API_LEVEL),
    );
    response
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
    /// Seconds for a `Retry-After` header.
    retry_after: Option<u64>,
}

impl AppError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        let status = match code {
            codes::NOT_FOUND | codes::NOT_A_TASK => StatusCode::NOT_FOUND,
            codes::FORBIDDEN
            | codes::INVITE_INVALID
            | codes::CSRF_FAILED
            | codes::NOT_TASK_CREATOR
            | codes::NO_AUTHORITY => StatusCode::FORBIDDEN,
            codes::UNAUTHENTICATED => StatusCode::UNAUTHORIZED,
            codes::RATE_LIMITED => StatusCode::TOO_MANY_REQUESTS,
            codes::IDEMPOTENCY_CONFLICT
            | codes::REVISION_CONFLICT
            | codes::LABEL_CONFLICT
            | codes::MESSAGE_DELETED
            | codes::TASK_EXISTS
            | codes::TASK_CLAIMED
            | codes::TASK_ASSIGNED
            | codes::TASK_WAITING
            | codes::TASK_BLOCKED
            | codes::TASK_FINISHED
            | codes::TASK_STATE
            | codes::CLAIM_LOST
            | codes::CLAIM_NOT_HELD
            | codes::CHANNEL_EXISTS => StatusCode::CONFLICT,
            codes::MESSAGE_TOO_LARGE => StatusCode::PAYLOAD_TOO_LARGE,
            codes::INTERNAL => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        };
        Self {
            status,
            code,
            message: message.into(),
            next: None,
            retry_after: None,
        }
    }

    /// The one answer to every request a network board cannot authenticate:
    /// no token or session, a malformed one, an unknown one, an expired one
    /// or a revoked one. It never varies, so it tells the caller nothing,
    /// and it is the same for a bearer token and a session cookie.
    fn unauthenticated() -> Self {
        Self::new(
            codes::UNAUTHENTICATED,
            "this board needs a valid bearer token",
        )
        .next("enrol with an invite from the board's operator, or ask for a new one if your token has expired or been revoked")
    }

    /// A write that failed the `Origin` check. Nothing was changed.
    fn foreign_origin() -> Self {
        Self::new(
            codes::CSRF_FAILED,
            "the request's Origin is not the address it was sent to",
        )
        .next("write to the board only from its own pages, or send no Origin header")
    }

    /// A write with a session cookie and without that session's anti-CSRF
    /// token. Nothing was changed.
    fn csrf_token_missing() -> Self {
        Self::new(
            codes::CSRF_FAILED,
            "a write with a session cookie needs the session's anti-CSRF token",
        )
        .next(format!(
            "send the csrf_token from GET {SESSION_PATH} in the {CSRF_HEADER} header"
        ))
    }

    /// The one answer to every enrolment or sign-in code that cannot be
    /// used, at either endpoint.
    fn invite_invalid() -> Self {
        Self::new(
            codes::INVITE_INVALID,
            "the invite is wrong, expired or already used",
        )
        .next("ask the board's operator for a new invite")
    }

    fn rate_limited(retry_after: u64) -> Self {
        let mut error = Self::new(codes::RATE_LIMITED, "too many requests or open connections")
            .next(format!("retry after {retry_after} seconds"));
        error.retry_after = Some(retry_after);
        error
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
        let mut response = (self.status, Json(body)).into_response();
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, seconds.into());
        }
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
        }
        response
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
            StoreError::MessageMissing => message_not_found(),
            StoreError::MessageDeleted => AppError::new(
                codes::MESSAGE_DELETED,
                "the message was deleted and cannot be changed",
            )
            .next("reply to its thread instead; replying still works"),
            StoreError::RevisionConflict => AppError::new(
                codes::REVISION_CONFLICT,
                "the message is not at the revision the request named",
            )
            .next("read the message again and retry with its current revision"),
            // The key was validated before it got to the store. A value
            // never appears here.
            StoreError::LabelConflict(key) => AppError::new(
                codes::LABEL_CONFLICT,
                format!("the condition on label `{key}` does not hold"),
            )
            .next("read the message again to see its current labels"),
            StoreError::TooManyLabels => invalid_label(format!(
                "a message holds at most {MAX_LABELS_PER_MESSAGE} labels"
            )),
            StoreError::TooManyReactions => AppError::new(
                codes::INVALID_REACTION,
                format!(
                    "a message holds at most {MAX_REACTION_NAMES_PER_MESSAGE} different \
                     reactions, and this one would be one more"
                ),
            )
            .next("use one of the reactions the message already has, or remove one first"),
            StoreError::Task(error) => tasks::task_error(error),
            StoreError::Grant(error) => grants_routes::grant_error(error),
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

fn message_not_found() -> AppError {
    AppError::new(codes::NOT_FOUND, "no such message on this board")
        .next("read the thread to find the message ID")
}

fn invalid_label(reason: impl Into<String>) -> AppError {
    AppError::new(codes::INVALID_LABEL, reason)
}

type ApiResult<T> = Result<T, AppError>;

async fn unknown_route() -> AppError {
    AppError::new(codes::NOT_FOUND, "no such route").next("see the route table in openboard-api")
}

async fn wrong_method() -> AppError {
    AppError::invalid("this route does not support that method")
        .status(StatusCode::METHOD_NOT_ALLOWED)
}

/// Who a request comes from, as [`gate`] established it. Handlers read it
/// from the request and never from anything the client sent.
#[derive(Debug, Clone)]
struct Identity {
    principal: Principal,
    /// The credential behind an authenticated request on a network board.
    credential: Option<Credential>,
    /// The anti-CSRF token of the session, when the request was
    /// authenticated by the session cookie and not by a bearer token.
    csrf: Option<String>,
    client: Option<IpAddr>,
}

/// The first thing every request meets.
///
/// Local mode has no auth, so a web page must not be able to reach the board
/// by pointing a hostname it controls at loopback (DNS rebinding). Requests
/// whose `Host` is not a loopback name are refused.
///
/// Network mode authenticates instead: clients address the board by its LAN
/// address, and only the health check, enrolment, sign-in and the web UI's
/// static files are answered without a valid token or session; see
/// [`needs_no_token`]. Any other unauthenticated request is refused here,
/// before routing, so unknown routes, bad paths and bad bodies all look the
/// same to it.
///
/// Last, in both modes, come the anti-CSRF checks on writes; see
/// [`check_csrf`]. They too run before routing, so a refused write reaches
/// no handler and changes nothing.
async fn gate(State(state): State<Shared>, mut request: Request, next: Next) -> Response {
    let client = request
        .extensions()
        .get::<ConnectInfo<ClientAddr>>()
        .map(|ConnectInfo(ClientAddr(addr))| addr.ip());
    let identity = match state.mode {
        Mode::Local => {
            let allowed = match request.headers().get(header::HOST) {
                None => true,
                Some(value) => value.to_str().is_ok_and(is_loopback_host),
            };
            if !allowed {
                return AppError::new(
                    codes::FORBIDDEN,
                    "a local board only answers requests addressed to a loopback host",
                )
                .into_response();
            }
            Identity {
                principal: Principal::local(),
                credential: None,
                csrf: None,
                client,
            }
        }
        Mode::Network => {
            if needs_no_token(request.method(), request.uri().path()) {
                Identity {
                    principal: Principal::anonymous(),
                    credential: None,
                    csrf: None,
                    client,
                }
            } else {
                // The page asks this route who is signed in before it
                // knows whether anyone is. See `authenticate`.
                let probe = request.method() == Method::GET && request.uri().path() == SESSION_PATH;
                match state.authenticate(request.headers(), client, probe).await {
                    Ok((credential, csrf)) => Identity {
                        principal: credential.principal(),
                        credential: Some(credential),
                        csrf,
                        client,
                    },
                    Err(refused) => return refused.into_response(),
                }
            }
        }
    };
    if let Err(refused) = check_csrf(
        state.mode,
        request.method(),
        request.headers(),
        identity.csrf.as_deref(),
    ) {
        return refused.into_response();
    }
    request.extensions_mut().insert(identity);
    next.run(request).await
}

/// The two anti-CSRF checks of `openboard_api::web`, on a write: any
/// request whose method is not `GET` or `HEAD`.
///
/// First, a write that names its `Origin` must name the scheme and
/// authority it was sent to, which is the board's scheme and the request's
/// `Host`. A browser sends `Origin` with every cross-origin write and
/// cannot be made to forge it, so a page of another site, or of another
/// port on the same host, cannot write here even where no credential is
/// needed: on a local board and at sign-in. A client that is not a browser
/// sends no `Origin` and is not affected.
///
/// Second, a write authenticated by the session cookie must carry the
/// session's anti-CSRF token, `csrf`, in the [`CSRF_HEADER`] header. A
/// bearer token is never sent by a browser on its own, so a request
/// authenticated by one skips this check.
fn check_csrf(
    mode: Mode,
    method: &Method,
    headers: &HeaderMap,
    csrf: Option<&str>,
) -> ApiResult<()> {
    if method == Method::GET || method == Method::HEAD {
        return Ok(());
    }
    let scheme = match mode {
        Mode::Local => "http",
        Mode::Network => "https",
    };
    if !origin_is_own(headers, scheme) {
        return Err(AppError::foreign_origin());
    }
    if let Some(expected) = csrf {
        let presented = headers
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok());
        // Compared in constant time: the token is a secret of the session.
        if !presented.is_some_and(|presented| identity::constant_time_eq(presented, expected)) {
            return Err(AppError::csrf_token_missing());
        }
    }
    Ok(())
}

/// Whether the request has no `Origin` header, or exactly one that is
/// `scheme://` followed by the request's `Host`. The comparison ignores
/// ASCII case and nothing else: a browser writes the port the same way in
/// both headers. `Origin: null`, a second `Origin`, and an `Origin` on a
/// request without exactly one `Host` all fail.
fn origin_is_own(headers: &HeaderMap, scheme: &str) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    let mut hosts = headers.get_all(header::HOST).iter();
    let (Some(host), None) = (hosts.next(), hosts.next()) else {
        return false;
    };
    let (Ok(origin), Ok(host)) = (origin.to_str(), host.to_str()) else {
        return false;
    };
    let Some((origin_scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    !host.is_empty()
        && origin_scheme.eq_ignore_ascii_case(scheme)
        && authority.eq_ignore_ascii_case(host)
}

/// The token of an `Authorization: Bearer <token>` header.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

/// The port the client sent the request to, as its `Host` header has it:
/// the digits after the last `:`, or 443 when the header names no port,
/// which is the port a browser leaves out on a network board. `None` when
/// there is not exactly one `Host` header, or when what follows the host
/// name is not one to five digits that make a port number.
///
/// This decides the name of the session cookie, so nothing but digits ever
/// gets from the header into that name.
fn host_port(headers: &HeaderMap) -> Option<u16> {
    let mut hosts = headers.get_all(header::HOST).iter();
    let (Some(host), None) = (hosts.next(), hosts.next()) else {
        return None;
    };
    let host = host.to_str().ok()?;
    let port = match host.strip_prefix('[') {
        Some(rest) => {
            let (_, after) = rest.split_once(']')?;
            if after.is_empty() {
                return Some(443);
            }
            after.strip_prefix(':')?
        }
        None => match host.rsplit_once(':') {
            Some((name, _)) if name.is_empty() || name.contains(':') => return None,
            Some((_, port)) => port,
            None if host.is_empty() => return None,
            None => return Some(443),
        },
    };
    let digits = !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit());
    if digits {
        port.parse().ok()
    } else {
        None
    }
}

/// The name of this board's session cookie on this request; see
/// `openboard_api::web`. `None` when the request names no usable port, and
/// then it has no session cookie at all.
fn own_cookie_name(headers: &HeaderMap) -> Option<String> {
    host_port(headers).map(session_cookie_name)
}

/// The value of the session cookie called `own`, from whichever `Cookie`
/// header holds it. The first cookie with that name counts, and a browser
/// sends at most one, because the `__Host-` prefix ties the cookie to this
/// exact host and to `Path=/`.
///
/// The header is not ours alone: a browser sends every cookie it holds for
/// the host, whatever port set them, so it may be long, hold bytes that
/// are not ASCII and hold pairs that are not well formed. It is read as
/// bytes, in one pass, without copying; whatever is not this board's
/// session cookie is skipped, the session cookies of boards on other ports
/// included, and a value that is not text is no session.
fn session_cookie<'h>(headers: &'h HeaderMap, own: &str) -> Option<&'h str> {
    let trim = |bytes: &[u8]| -> std::ops::Range<usize> {
        let blank = |b: &u8| *b == b' ' || *b == b'\t';
        let start = bytes.iter().position(|b| !blank(b)).unwrap_or(bytes.len());
        let end = bytes
            .iter()
            .rposition(|b| !blank(b))
            .map_or(start, |i| i + 1);
        start..end
    };
    for header in headers.get_all(header::COOKIE) {
        for pair in header.as_bytes().split(|b| *b == b';') {
            let Some(equals) = pair.iter().position(|b| *b == b'=') else {
                continue;
            };
            let (name, value) = (&pair[..equals], &pair[equals + 1..]);
            if &name[trim(name)] == own.as_bytes() {
                return std::str::from_utf8(&value[trim(value)]).ok();
            }
        }
    }
    None
}

impl AppState {
    /// Resolves the bearer token, or without an `Authorization` header the
    /// session cookie, to its credential, and counts the request against
    /// that credential's rate limit. Returns the credential and, for a
    /// session, its anti-CSRF token. Every failure is the same `401`,
    /// except that a client address with too many failures is told to slow
    /// down.
    ///
    /// A request with an `Authorization` header is authenticated by that
    /// header alone, whatever it holds: the cookie is not looked at, not
    /// even when the header is unusable.
    ///
    /// `probe` is true for `GET /v1/web/session` and for nothing else. The
    /// web UI asks that route who is signed in every time the page loads,
    /// and when nobody is, the request carries no credential at all: no
    /// `Authorization` header and no session cookie of this board. Such a
    /// request gets the same `401` as any other, but it is not counted as a
    /// failed authentication and not logged; otherwise people behind one
    /// address would use up each other's budget by opening the sign-in
    /// page. It looks nothing up and its answer never varies, not even with
    /// the state of that budget, so it cannot be used to learn anything. A
    /// credential that is presented and is wrong is counted on this route
    /// as on every other.
    async fn authenticate(
        self: &Arc<Self>,
        headers: &HeaderMap,
        client: Option<IpAddr>,
        probe: bool,
    ) -> ApiResult<(Credential, Option<String>)> {
        // Text that cannot be a token or a session is refused without a
        // lookup. A credential is found by its digest, never by comparing
        // secrets. The two have different shapes, so a token in the cookie
        // or a session in the header is malformed.
        let by_header = headers.contains_key(header::AUTHORIZATION);
        let own_cookie = own_cookie_name(headers);
        let (presented, hash) = if by_header {
            let token = bearer_token(headers);
            (token, token.and_then(token_lookup_hash))
        } else {
            let session = own_cookie
                .as_deref()
                .and_then(|own| session_cookie(headers, own));
            (session, session.and_then(session_lookup_hash))
        };
        if probe && !by_header && presented.is_none() {
            return Err(AppError::unauthenticated());
        }
        let outcome = match hash {
            None => None,
            Some(hash) => Some(
                self.db(move |state| {
                    state
                        .store
                        .authenticate(&hash, state.limits.last_used_interval)
                })
                .await?,
            ),
        };
        let (reason, known) = match outcome {
            Some(AuthOutcome::Valid(credential)) => {
                return match self
                    .requests
                    .hit(credential.id.clone(), self.limits.requests_per_minute)
                {
                    Hit::Allowed => {
                        let csrf = match presented {
                            Some(session) if !by_header => Some(identity::csrf_token(session)),
                            _ => None,
                        };
                        Ok((credential, csrf))
                    }
                    Hit::Limited { retry_after, first } => {
                        if first {
                            self.log(
                                log_kinds::RATE_LIMITED,
                                Some(&credential),
                                client,
                                Some("requests"),
                            )
                            .await;
                        }
                        Err(AppError::rate_limited(retry_after))
                    }
                };
            }
            Some(AuthOutcome::Expired(credential)) if by_header => {
                ("expired token", Some(credential))
            }
            Some(AuthOutcome::Revoked(credential)) if by_header => {
                ("revoked token", Some(credential))
            }
            Some(AuthOutcome::Expired(credential)) => ("expired session", Some(credential)),
            Some(AuthOutcome::Revoked(credential)) => ("revoked session", Some(credential)),
            Some(AuthOutcome::Unknown) if by_header => ("unknown token", None),
            Some(AuthOutcome::Unknown) => ("unknown session", None),
            None if presented.is_some() && by_header => ("malformed token", None),
            None if presented.is_some() => ("malformed session", None),
            None => ("no token", None),
        };
        match self
            .auth_failures
            .hit(client_key(client), self.limits.auth_failures_per_minute)
        {
            Hit::Allowed => {
                self.log_failure(log_kinds::AUTH_FAILED, known.as_ref(), client, reason)
                    .await;
                Err(AppError::unauthenticated())
            }
            Hit::Limited { retry_after, first } => {
                if first {
                    self.log_capped(
                        log_kinds::RATE_LIMITED,
                        None,
                        client,
                        "failed authentication",
                    )
                    .await;
                }
                Err(AppError::rate_limited(retry_after))
            }
        }
    }

    /// Appends a security log entry. A log that cannot be written is
    /// reported on stderr and does not change the response.
    async fn log(
        self: &Arc<Self>,
        kind: &'static str,
        credential: Option<&Credential>,
        client: Option<IpAddr>,
        detail: Option<&'static str>,
    ) {
        let subject = credential.map(|c| (c.handle.clone(), c.id.clone()));
        let written = self
            .db(move |state| {
                state.store.log(&LogEntry {
                    kind,
                    principal: subject.as_ref().map(|(handle, _)| handle.as_str()),
                    credential_id: subject.as_ref().map(|(_, id)| id.as_str()),
                    client,
                    detail,
                })
            })
            .await;
        if written.is_err() {
            eprintln!("openboard-server: could not write a security log entry of type {kind}");
        }
    }

    /// Logs a failed authentication or enrolment, within two budgets, so
    /// that failures cannot fill the disk and no client can use up the log
    /// for the others. Each client address has its own share for the
    /// minute; when it is spent, one entry says so and the address's
    /// further failures are dropped. Everything that passes is then counted
    /// against the budget of all clients together.
    async fn log_failure(
        self: &Arc<Self>,
        kind: &'static str,
        credential: Option<&Credential>,
        client: Option<IpAddr>,
        reason: &'static str,
    ) {
        let share = self.limits.failure_log_per_address_per_minute;
        match self.failure_log_by_address.hit(client_key(client), share) {
            Hit::Allowed => self.log_capped(kind, credential, client, reason).await,
            Hit::Limited { first: true, .. } => {
                self.log_capped(
                    log_kinds::LOG_SUPPRESSED,
                    None,
                    client,
                    "further failures from this address this minute are not logged",
                )
                .await;
            }
            Hit::Limited { .. } => {}
        }
    }

    /// Logs an entry that a client without a credential can cause, within
    /// the budget of all clients together. When that is spent for the
    /// minute, one entry says so and the rest are dropped.
    async fn log_capped(
        self: &Arc<Self>,
        kind: &'static str,
        credential: Option<&Credential>,
        client: Option<IpAddr>,
        detail: &'static str,
    ) {
        match self.failure_log.hit((), self.limits.failure_log_per_minute) {
            Hit::Allowed => self.log(kind, credential, client, Some(detail)).await,
            Hit::Limited { first: true, .. } => {
                self.log(
                    log_kinds::LOG_SUPPRESSED,
                    None,
                    None,
                    Some("further failures this minute are not logged"),
                )
                .await;
            }
            Hit::Limited { .. } => {}
        }
    }
}

/// The key a client is rate limited under. A request always has a client
/// address; if one ever lacks it, all such requests share a key.
fn client_key(client: Option<IpAddr>) -> IpAddr {
    address_key(client.unwrap_or(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)))
}

/// Whether a `Host` header names this machine's loopback interface, the
/// only kind of name a local board answers to.
///
/// The whole value has to be one of three spellings, with nothing before,
/// between or after: `localhost` in any letter case, an IPv4 address in
/// `127.0.0.0/8` written as four decimal numbers, or `[::1]` in any of its
/// IPv6 spellings; each optionally followed by `:` and a port of one to
/// five digits that fits a port number. Anything else is refused, among it
/// a name that merely starts with one of these (`[::1].evil.example`,
/// `localhost.evil.example`), a trailing dot, an empty port, user
/// information, white space, a bracketed IPv4 address and an IPv4-mapped
/// IPv6 address.
fn is_loopback_host(host: &str) -> bool {
    let is_port = |port: &str| {
        !port.is_empty()
            && port.len() <= 5
            && port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u16>().is_ok()
    };
    if let Some(rest) = host.strip_prefix('[') {
        // Brackets hold an IPv6 address and nothing else. After them comes
        // the end, or a port.
        let Some((address, after)) = rest.split_once(']') else {
            return false;
        };
        let ends_well = match after.strip_prefix(':') {
            Some(port) => is_port(port),
            None => after.is_empty(),
        };
        return ends_well
            && address
                .parse::<std::net::Ipv6Addr>()
                .is_ok_and(|ip| ip.is_loopback());
    }
    let name = match host.rsplit_once(':') {
        Some((name, port)) if is_port(port) => name,
        Some(_) => return false,
        None => host,
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

// ---------------------------------------------------------------------------
// Shared request handling
// ---------------------------------------------------------------------------

/// Who is calling: the principal [`gate`] established, plus the profile the
/// caller declared. The profile header only picks a label.
struct Caller {
    principal: Principal,
    profile: Option<String>,
    /// The credential the caller authenticated with, on a network board.
    credential_id: Option<String>,
    /// What the caller may do, on a network board: the grants and ceilings
    /// read when the request was authenticated. `None` on a local board,
    /// where the policy alone decides, and before [`AppState::caller`] has
    /// loaded it, when a network board allows nothing.
    authority: Option<Arc<access::Authority>>,
}

impl Caller {
    fn new(identity: &Identity, headers: &HeaderMap) -> ApiResult<Self> {
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
            principal: identity.principal.clone(),
            profile,
            credential_id: identity.credential.as_ref().map(|c| c.id.clone()),
            authority: None,
        })
    }

    /// The actor the caller writes as.
    fn actor(&self) -> openboard_api::Actor {
        self.principal.actor(self.profile.as_deref())
    }

    /// The caller as the store records the author of a change. The actor
    /// and the credential come from the gate, never from the request.
    fn writer(&self) -> Writer {
        Writer {
            actor: self.actor(),
            principal_id: self.principal.id.clone(),
            credential_id: self.credential_id.clone(),
        }
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

    /// The caller behind a request, with its authority on a network board.
    async fn caller(
        self: &Arc<Self>,
        identity: &Identity,
        headers: &HeaderMap,
    ) -> ApiResult<Caller> {
        let mut caller = Caller::new(identity, headers)?;
        if self.mode == Mode::Network && !caller.principal.is_anonymous() {
            let snapshot = self.db(|state| state.store.grants_snapshot()).await?;
            let subject = grants::Subject {
                id: caller.principal.id.clone(),
                kind: caller.principal.kind,
            };
            caller.authority = Some(Arc::new(access::Authority::new(snapshot, subject)));
        }
        Ok(caller)
    }

    /// The one place handlers ask whether the caller may do something. The
    /// policy decides first; on a network board the caller's grants then
    /// decide what it may do in the board and channel.
    fn allows(&self, caller: &Caller, action: Action, board: &str, channel: Option<&str>) -> bool {
        if !self
            .authz
            .authorize(&caller.principal, action, Some(board), channel)
            .is_allowed()
        {
            return false;
        }
        match (self.mode, &caller.authority) {
            (Mode::Local, _) => true,
            (Mode::Network, Some(authority)) => authority.allows(action, board, channel),
            (Mode::Network, None) => false,
        }
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

impl AppState {
    /// The board and message a change is addressed to, once the caller is
    /// known to be allowed `action` in the message's channel. A message in
    /// a channel the caller may not read is reported as missing.
    async fn message_target(
        self: &Arc<Self>,
        caller: &Caller,
        board: &str,
        message_id: &str,
        action: Action,
    ) -> ApiResult<(BoardRef, MessageMeta)> {
        let board = self.board_named(caller, board).await?;
        let (board_id, id) = (board.id, message_id.to_owned());
        let meta = self
            .db(move |state| state.store.message_meta(board_id, &id))
            .await?
            .filter(|meta| self.allows(caller, Action::Read, &board.name, Some(&meta.channel)))
            .ok_or_else(message_not_found)?;
        self.require(caller, action, &board.name, Some(&meta.channel))?;
        Ok((board, meta))
    }

    /// Checks a message text against the rules a post and an edit share.
    fn check_text(&self, text: &str) -> ApiResult<()> {
        if text.trim().is_empty() {
            return Err(AppError::invalid("the message text is empty"));
        }
        let (chars, limit) = (text.chars().count(), self.max_message_chars);
        if chars > limit {
            return Err(message_too_large(format!(
                "the message has {chars} characters and the limit is {limit}"
            )));
        }
        Ok(())
    }

    /// What `text` addresses, for the store to resolve. This parses the
    /// text, so it is called from storage work, off the async runtime.
    fn mentions_in(&self, text: &str) -> MentionInput {
        MentionInput {
            candidates: markdown::mention_candidates(text),
            local: self.mode == Mode::Local,
        }
    }

    /// Wakes the board's waiters when a change appended an event. Called
    /// from the blocking task that made the change, straight after its
    /// commit, so it happens even if the request was dropped meanwhile.
    fn announce(&self, board: &BoardRef, changed: &Changed) {
        if let Some(seq) = changed.seq {
            self.hub.publish(board.id, seq);
        }
    }
}

/// The one place the author-only rule of edit and delete is enforced: the
/// caller must be the actor that wrote the message, which is the same
/// principal under the same declared profile. It applies on top of what
/// the policy allows.
fn require_author(caller: &Caller, message: &MessageMeta) -> ApiResult<()> {
    if caller.actor().id == message.actor_id {
        Ok(())
    } else {
        Err(AppError::new(
            codes::FORBIDDEN,
            "only the actor that wrote a message may edit or delete it",
        )
        .next(format!(
            "use the principal and the {PROFILE_HEADER} profile the message was written with"
        )))
    }
}

fn message_too_large(detail: String) -> AppError {
    AppError::new(codes::MESSAGE_TOO_LARGE, detail)
        .next("shorten the message or split it across replies")
}

/// Reads a JSON request body. Requiring JSON keeps a web page from writing
/// here with a plain form. The error reports where the JSON is wrong and
/// never quotes it.
fn read_json<T: serde::de::DeserializeOwned>(
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
    what: &str,
    too_large: impl FnOnce() -> AppError,
) -> ApiResult<T> {
    if !is_json(headers) {
        return Err(
            AppError::invalid("the request body must be sent as application/json")
                .status(StatusCode::UNSUPPORTED_MEDIA_TYPE),
        );
    }
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            too_large()
        } else {
            AppError::invalid("the request body could not be read")
        }
    })?;
    serde_json::from_slice(&body).map_err(|error| {
        // Only the position is reported. A serde message can quote the input.
        AppError::invalid(format!(
            "the request body is not a valid {what} (JSON {:?} error at line {}, column {})",
            error.classify(),
            error.line(),
            error.column()
        ))
    })
}

/// Fills [`Message::html`] for the messages of one response, in the order
/// they appear in it, and keeps the response's HTML bounded.
///
/// The renderer bounds one message; see `markdown::html_limit`. This bounds
/// their sum: once [`MAX_HTML_BYTES_PER_RESPONSE`] bytes have been rendered,
/// the messages that follow get `markdown::plain_html`, which parses
/// nothing and is at most six times the text the response carries anyway.
/// So a response holds at most the budget, one more message's limit, and a
/// small multiple of its own text, however many messages it has and
/// whatever they contain.
#[derive(Default)]
struct HtmlBudget {
    /// Bytes of HTML rendered from Markdown so far.
    rendered: usize,
}

impl HtmlBudget {
    /// Renders the message's text into [`Message::html`]. A tombstone has
    /// no text, so its HTML is empty.
    fn add_html(&mut self, message: &mut Message) {
        message.html = Some(if message.deleted_at.is_some() {
            String::new()
        } else if self.rendered >= MAX_HTML_BYTES_PER_RESPONSE {
            markdown::plain_html(&message.text)
        } else {
            let html = markdown::render_html(&message.text, &message.mentions);
            self.rendered = self.rendered.saturating_add(html.len());
            html
        });
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

impl AppState {
    /// Authorizes an action that concerns no board.
    fn require_global(&self, principal: &Principal, action: Action) -> ApiResult<()> {
        if self
            .authz
            .authorize(principal, action, None, None)
            .is_allowed()
        {
            Ok(())
        } else {
            Err(AppError::forbidden())
        }
    }

    /// Reserves one of the credential's places for a long-poll or a stream.
    /// A local board has no credentials and no cap.
    fn waiter(&self, caller: &Caller) -> ApiResult<Option<WaiterGuard>> {
        match &caller.credential_id {
            None => Ok(None),
            Some(id) => self
                .waiters
                .acquire(id, self.limits.max_waiters)
                .map(Some)
                // Places free up as waits end, so there is no exact time.
                .ok_or_else(|| AppError::rate_limited(1)),
        }
    }
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
}

async fn health(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
) -> ApiResult<Json<Health>> {
    let caller = state.caller(&identity, &headers).await?;
    state.require_global(&caller.principal, Action::Health)?;
    Ok(Json(Health {
        status: "ok".to_owned(),
        version: openboard_api::build::VERSION.to_owned(),
        mode: state.mode.as_str().to_owned(),
        commit: openboard_api::build::commit().map(str::to_owned),
        api_level: Some(API_LEVEL),
    }))
}

/// `POST /v1/join`: swaps a single-use enrolment code for a token.
///
/// Only failures count against the client address's budget, and a valid
/// code is accepted whatever that budget says: the code is a long random
/// single-use secret, so letting it through gives a guesser nothing, while
/// refusing it would let junk from one address lock out an agent that
/// shares the address. Once the address is over its budget every failure is
/// answered with `rate_limited`. What a refused attempt costs is bounded:
/// a small body read within a time limit and, for text shaped like a code,
/// one indexed lookup.
async fn join(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    if state.mode == Mode::Local {
        return Err(AppError::invalid(
            "this is a local board: it has no enrolment and needs no token",
        ));
    }
    state.require_global(&identity.principal, Action::Enrol)?;
    let client = identity.client;
    let request: JoinRequest = match read_code_request(&state, &headers, body, "join request").await
    {
        Ok(request) => request,
        Err(malformed) => return Err(state.join_failed(ENROLMENT, client, None, malformed).await),
    };
    // A person's sign-in code does not have this shape, so it is refused
    // here without being looked up, and stays unspent.
    let joined = if identity::code_is_well_formed(&request.code, InviteKind::Agent) {
        state
            .db(move |state| state.store.join(&request.code, client))
            .await?
    } else {
        None
    };
    let Some((token, credential)) = joined else {
        return Err(state
            .join_failed(
                ENROLMENT,
                client,
                Some("unusable code"),
                AppError::invite_invalid(),
            )
            .await);
    };
    let response = JoinResponse {
        token,
        credential: credential.info(),
    };
    // The token is in this response and nowhere else. Nothing may keep it.
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(response)).into_response())
}

/// Reads the body of an enrolment or sign-in request. The caller is not
/// authenticated, so the body is small and has to arrive within the time a
/// request head has.
async fn read_code_request<T: serde::de::DeserializeOwned>(
    state: &AppState,
    headers: &HeaderMap,
    body: Body,
    what: &str,
) -> ApiResult<T> {
    if !is_json(headers) {
        return Err(
            AppError::invalid("the request body must be sent as application/json")
                .status(StatusCode::UNSUPPORTED_MEDIA_TYPE),
        );
    }
    let unreadable = || AppError::invalid("the request body could not be read");
    let reading = axum::body::to_bytes(body, MAX_JOIN_BODY);
    let body = tokio::time::timeout(state.limits.header_read_timeout, reading)
        .await
        .map_err(|_| unreadable().status(StatusCode::REQUEST_TIMEOUT))?
        .map_err(|_| unreadable())?;
    // The error never quotes the body, which may hold a code.
    serde_json::from_slice(&body)
        .map_err(|_| AppError::invalid(format!("the request body is not a valid {what}")))
}

/// How the two ways of swapping a code for a credential appear in the
/// security log.
struct CodeSwap {
    /// The entry type of a failed attempt.
    failed: &'static str,
    /// The detail of the entry written when an address runs into the limit.
    limited: &'static str,
}

const ENROLMENT: CodeSwap = CodeSwap {
    failed: log_kinds::ENROL_FAILED,
    limited: "enrolment",
};
const SIGN_IN: CodeSwap = CodeSwap {
    failed: log_kinds::WEB_LOGIN_FAILED,
    limited: "web login",
};

impl AppState {
    /// Counts a failed enrolment or sign-in attempt against its client
    /// address and returns the answer: `refusal` while the address is
    /// within its budget, `rate_limited` once it is over. The two share one
    /// budget. `reason` is logged for an attempt that presented a code.
    async fn join_failed(
        self: &Arc<Self>,
        swap: CodeSwap,
        client: Option<IpAddr>,
        reason: Option<&'static str>,
        refusal: AppError,
    ) -> AppError {
        match self
            .joins
            .hit(client_key(client), self.limits.joins_per_minute)
        {
            Hit::Allowed => {
                if let Some(reason) = reason {
                    self.log_failure(swap.failed, None, client, reason).await;
                }
                refusal
            }
            Hit::Limited { retry_after, first } => {
                if first {
                    self.log_capped(log_kinds::RATE_LIMITED, None, client, swap.limited)
                        .await;
                }
                AppError::rate_limited(retry_after)
            }
        }
    }
}

/// `GET /v1/whoami`: the caller as the board records it.
async fn whoami(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
) -> ApiResult<Json<WhoAmI>> {
    let caller = state.caller(&identity, &headers).await?;
    state.require_global(&caller.principal, Action::Session)?;
    Ok(Json(WhoAmI {
        actor: caller.actor(),
        mode: state.mode.as_str().to_owned(),
        credential: identity.credential.as_ref().map(Credential::info),
    }))
}

/// `POST /v1/logout`: revokes the credential the caller authenticated with.
/// When that is a browser session, the response also clears the cookie.
async fn logout(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    state.require_global(&identity.principal, Action::Session)?;
    let Some(credential) = identity.credential else {
        return Err(AppError::invalid(
            "this is a local board: there is no credential to revoke",
        ));
    };
    let client = identity.client;
    let session = identity.csrf.is_some();
    let detail = session.then_some("session");
    state
        .db(move |state| state.store.logout(&credential, client, detail))
        .await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    // The cookie that authenticated the request is the one to clear: the
    // one named after the port the request was sent to.
    if let (true, Some(name)) = (session, own_cookie_name(&headers)) {
        response
            .headers_mut()
            .insert(header::SET_COOKIE, session_cookie_header(&name, "", 0)?);
    }
    Ok(response)
}

/// The `Set-Cookie` header that sets the session cookie `name` to `value`
/// for `max_age` seconds, with the attributes `openboard_api::web` fixes.
/// An empty value and a zero age clear the cookie. The header is marked
/// sensitive, so nothing that prints headers shows the value.
fn session_cookie_header(name: &str, value: &str, max_age: i64) -> ApiResult<HeaderValue> {
    let cookie =
        format!("{name}={value}; Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}");
    let mut header = HeaderValue::from_str(&cookie)
        .map_err(|_| AppError::internal(&"the session cookie is not a valid header"))?;
    header.set_sensitive(true);
    Ok(header)
}

/// `POST /v1/web/login`: swaps a person's single-use sign-in code for a
/// browser session, and sets the session cookie.
///
/// The cookie's value is the session's secret. It is in the `Set-Cookie`
/// header and nowhere else: the body describes the session without it, so
/// script on the page cannot read it. Failures are counted and answered
/// exactly as those of [`join`], with which this shares its budget, and an
/// agent's enrolment code is refused here without being spent.
///
/// The cookie is named after the port the request was sent to, so boards
/// on several ports of one host keep their sessions apart. When the request
/// already carries this board's cookie with a usable session of the same
/// person, the new session replaces it; see `Store::web_login`.
async fn web_login(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    if state.mode == Mode::Local {
        return Err(AppError::invalid(
            "this is a local board: it has no sign-in and needs none",
        ));
    }
    state.require_global(&identity.principal, Action::SignIn)?;
    // A declared profile is checked before the code is spent, and so is
    // that the cookie can be named.
    let profile = Caller::new(&identity, &headers)?.profile;
    let cookie_name = own_cookie_name(&headers).ok_or_else(|| {
        AppError::invalid("the Host header must name the board, with its port in digits if any")
    })?;
    let replaces = session_cookie(&headers, &cookie_name).and_then(session_lookup_hash);
    let client = identity.client;
    let request: LoginRequest = match read_code_request(&state, &headers, body, "login request")
        .await
    {
        Ok(request) => request,
        Err(malformed) => return Err(state.join_failed(SIGN_IN, client, None, malformed).await),
    };
    let signed_in = if identity::code_is_well_formed(&request.code, InviteKind::Person) {
        state
            .db(move |state| {
                state
                    .store
                    .web_login(&request.code, client, replaces.as_deref())
            })
            .await?
    } else {
        None
    };
    let Some((secret, credential)) = signed_in else {
        return Err(state
            .join_failed(
                SIGN_IN,
                client,
                Some("unusable code"),
                AppError::invite_invalid(),
            )
            .await);
    };
    let max_age = credential.expires_ms.saturating_sub(credential.created_ms) / 1000;
    let session = Session {
        actor: credential.principal().actor(profile.as_deref()),
        mode: state.mode.as_str().to_owned(),
        csrf_token: Some(identity::csrf_token(&secret)),
        credential: Some(credential.info()),
    };
    let mut response = Json(session).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie_header(&cookie_name, &secret, max_age.max(0))?,
    );
    Ok(response)
}

/// `GET /v1/web/session`: who the request is authenticated as, for the web
/// UI. On a local board that is the local principal. On a network board a
/// request without a valid session or token never gets here: the gate's
/// `401` is how the page learns that it must show the sign-in screen.
async fn web_session(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
) -> ApiResult<Json<Session>> {
    let caller = state.caller(&identity, &headers).await?;
    state.require_global(&caller.principal, Action::Session)?;
    Ok(Json(Session {
        actor: caller.actor(),
        mode: state.mode.as_str().to_owned(),
        csrf_token: identity.csrf.clone(),
        credential: identity.credential.as_ref().map(Credential::info),
    }))
}

async fn post_message(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<(StatusCode, Json<PostMessageResponse>)> {
    let caller = state.caller(&identity, &headers).await?;
    let Path(board_name) = board.map_err(|_| bad_path())?;
    names::validate_board(&board_name)
        .map_err(|reason| AppError::new(codes::INVALID_BOARD, reason))?;
    let limit = state.max_message_chars;
    let request: PostMessageRequest = read_json(&headers, body, "post request", || {
        message_too_large(format!(
            "the request body is too large for a message of at most {limit} characters"
        ))
    })
    .map_err(|error| match error.status {
        StatusCode::BAD_REQUEST => {
            error.next("send JSON with `text` and exactly one of `channel` or `thread_id`")
        }
        _ => error,
    })?;
    state.check_text(&request.text)?;
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

    // The author and the credential come from the gate, never the body.
    let actor = caller.actor();
    let idempotency = key.map(|key| (key, request_fingerprint(&request)));
    let mut post = NewPost {
        channel,
        thread_id,
        actor,
        principal_id: caller.principal.id.clone(),
        credential_id: caller.credential_id.clone(),
        text: request.text,
        idempotency,
        mentions: MentionInput::default(),
    };
    // Waiters are told from the blocking task, straight after the commit.
    // The task finishes even if this request is dropped because its client
    // went away, so a committed post is always announced. An idempotent
    // retry announces the original position again, which wakes nobody who
    // has already seen it.
    let posted = state
        .db(move |state| {
            post.mentions = state.mentions_in(&post.text);
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

/// `PATCH /v1/boards/{board}/messages/{message_id}`: replaces the text.
async fn edit_message(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<Json<Message>> {
    let caller = state.caller(&identity, &headers).await?;
    let Path((board_name, message_id)) = path.map_err(|_| bad_path())?;
    let limit = state.max_message_chars;
    let request: EditMessageRequest = read_json(&headers, body, "edit request", || {
        message_too_large(format!(
            "the request body is too large for a message of at most {limit} characters"
        ))
    })?;
    state.check_text(&request.text)?;

    let (board, meta) = state
        .message_target(&caller, &board_name, &message_id, Action::Edit)
        .await?;
    require_author(&caller, &meta)?;
    let by = caller.writer();
    let changed = state
        .db(move |state| {
            let mentions = state.mentions_in(&request.text);
            let changed = state.store.edit(
                &board,
                &message_id,
                &by,
                &request.text,
                request.revision,
                &mentions,
            )?;
            state.announce(&board, &changed);
            Ok(changed)
        })
        .await?;
    Ok(Json(changed.message))
}

/// `DELETE /v1/boards/{board}/messages/{message_id}`: leaves a tombstone.
async fn delete_message(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
    query: Result<Query<DeleteMessageQuery>, QueryRejection>,
) -> ApiResult<Json<Message>> {
    let caller = state.caller(&identity, &headers).await?;
    let Path((board_name, message_id)) = path.map_err(|_| bad_path())?;
    let Query(query) = query.map_err(bad_query)?;
    let (board, meta) = state
        .message_target(&caller, &board_name, &message_id, Action::Delete)
        .await?;
    require_author(&caller, &meta)?;
    let by = caller.writer();
    let changed = state
        .db(move |state| {
            let changed = state
                .store
                .delete(&board, &message_id, &by, query.revision)?;
            state.announce(&board, &changed);
            Ok(changed)
        })
        .await?;
    Ok(Json(changed.message))
}

/// `POST /v1/boards/{board}/messages/{message_id}/labels`.
async fn label_message(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<Json<Message>> {
    let caller = state.caller(&identity, &headers).await?;
    let Path((board_name, message_id)) = path.map_err(|_| bad_path())?;
    let request: LabelsRequest = read_json(&headers, body, "labels request", || {
        AppError::invalid("the request body is too large for a labels request")
            .status(StatusCode::PAYLOAD_TOO_LARGE)
    })?;
    check_labels_request(&request)?;

    let (board, _meta) = state
        .message_target(&caller, &board_name, &message_id, Action::Label)
        .await?;
    let by = caller.writer();
    let changed = state
        .db(move |state| {
            let changed = state.store.label(&board, &message_id, &by, &request)?;
            state.announce(&board, &changed);
            Ok(changed)
        })
        .await?;
    Ok(Json(changed.message))
}

/// Validates every key and value of a labels request. No error names a
/// value, and a key that breaks the rules is not echoed either.
fn check_labels_request(request: &LabelsRequest) -> ApiResult<()> {
    let keys = request
        .set
        .keys()
        .chain(&request.clear)
        .chain(request.if_equals.keys())
        .chain(&request.if_unset);
    for key in keys {
        names::validate_label_key(key).map_err(invalid_label)?;
    }
    // A condition can only hold for a value a label could have.
    for value in request.set.values().chain(request.if_equals.values()) {
        names::validate_label_value(value).map_err(invalid_label)?;
    }
    if let Some(key) = request
        .clear
        .iter()
        .find(|key| request.set.contains_key(*key))
    {
        return Err(AppError::invalid(format!(
            "label `{key}` is in both `set` and `clear`"
        )));
    }
    Ok(())
}

/// Parses [`ThreadsQuery::label`]: `key`, `key=value` or `key=a|b`. A
/// filter may name a reserved key such as `board.task`, which no request
/// can set.
fn parse_label_filter(raw: &str) -> ApiResult<LabelFilter> {
    let (key, values) = match raw.split_once('=') {
        None => (raw, Vec::new()),
        Some((key, values)) => (key, values.split('|').map(str::to_owned).collect()),
    };
    names::validate_label_filter_key(key).map_err(invalid_label)?;
    for value in &values {
        names::validate_label_value(value).map_err(invalid_label)?;
    }
    Ok(LabelFilter {
        key: key.to_owned(),
        values,
    })
}

/// `PUT .../reactions/{emoji}`.
async fn add_reaction(
    state: State<Shared>,
    identity: Extension<Identity>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
) -> ApiResult<Json<Message>> {
    react(state, identity, headers, path, true).await
}

/// `DELETE .../reactions/{emoji}`.
async fn remove_reaction(
    state: State<Shared>,
    identity: Extension<Identity>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
) -> ApiResult<Json<Message>> {
    react(state, identity, headers, path, false).await
}

async fn react(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
    add: bool,
) -> ApiResult<Json<Message>> {
    let caller = state.caller(&identity, &headers).await?;
    let Path((board_name, message_id, name)) = path.map_err(|_| bad_path())?;
    names::validate_reaction(&name)
        .map_err(|reason| AppError::new(codes::INVALID_REACTION, reason))?;
    let (board, _meta) = state
        .message_target(&caller, &board_name, &message_id, Action::React)
        .await?;
    let by = caller.writer();
    // A reaction belongs to the principal, whatever profile it declared.
    let handle = caller.principal.handle.clone();
    let changed = state
        .db(move |state| {
            let changed = state
                .store
                .react(&board, &message_id, &by, &handle, &name, add)?;
            state.announce(&board, &changed);
            Ok(changed)
        })
        .await?;
    Ok(Json(changed.message))
}

/// `GET /v1/boards`: the boards the caller may read, ordered by name.
async fn list_boards(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
) -> ApiResult<Json<BoardList>> {
    let caller = state.caller(&identity, &headers).await?;
    let mut boards = state.db(|state| state.store.boards()).await?;
    boards.retain(|board| state.allows(&caller, Action::Read, &board.name, None));
    Ok(Json(BoardList { boards }))
}

/// `GET /` and `GET /join`: the web UI's page. It holds no board content;
/// the script on it learns who is signed in from the API.
async fn page(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
) -> ApiResult<Response> {
    state.require_global(&identity.principal, Action::Page)?;
    Ok((
        [(header::CONTENT_TYPE, pages::INDEX_CONTENT_TYPE)],
        pages::INDEX_HTML,
    )
        .into_response())
}

/// `GET /assets/{file}`: one of the files in [`pages::ASSETS`].
async fn asset(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    file: Result<Path<String>, PathRejection>,
) -> ApiResult<Response> {
    state.require_global(&identity.principal, Action::Page)?;
    let Path(file) = file.map_err(|_| bad_path())?;
    let asset =
        pages::find_asset(&file).ok_or_else(|| AppError::new(codes::NOT_FOUND, "no such asset"))?;
    Ok((
        [
            (header::CONTENT_TYPE, asset.content_type),
            // The files change with the binary and carry no version in
            // their names, so a browser asks again every time.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        asset.bytes,
    )
        .into_response())
}

async fn list_channels(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
) -> ApiResult<Json<ChannelList>> {
    let caller = state.caller(&identity, &headers).await?;
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
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    query: Result<Query<ThreadsQuery>, QueryRejection>,
) -> ApiResult<Json<ThreadList>> {
    let caller = state.caller(&identity, &headers).await?;
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
    let label = match &query.label {
        None => None,
        Some(raw) => Some(parse_label_filter(raw)?),
    };
    let html = query.html == Some(true);

    let listed_on = board.clone();
    let page = state
        .db(move |state| {
            let mut page =
                state
                    .store
                    .threads(&listed_on, &channel, label.as_ref(), before, limit)?;
            if html {
                let mut budget = HtmlBudget::default();
                for (_, summary) in &mut page.threads {
                    budget.add_html(&mut summary.root);
                }
            }
            Ok(page)
        })
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
        cursor: Some(event_cursor(&board.tag, page.head)),
    }))
}

async fn get_thread(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
    options: Result<Query<ReadOptions>, QueryRejection>,
) -> ApiResult<Json<Thread>> {
    let caller = state.caller(&identity, &headers).await?;
    let Path((board, thread_id)) = path.map_err(|_| bad_path())?;
    let Query(options) = options.map_err(bad_query)?;
    let html = options.html == Some(true);
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
        .db(move |state| {
            let mut thread = state.store.thread(&read_on, &root)?;
            if let (true, Some(thread)) = (html, &mut thread) {
                let mut budget = HtmlBudget::default();
                budget.add_html(&mut thread.root);
                for reply in &mut thread.replies {
                    budget.add_html(reply);
                }
            }
            Ok(thread)
        })
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
    /// When the caller's credential was last read as usable. It starts at
    /// the request's own authentication.
    checked_at: Instant,
    /// True after the scan has waited for something to happen, which is
    /// when a revocation is most likely to have gone unnoticed.
    waited: bool,
}

impl EventScan {
    /// `resume` is the cursor to continue from: the `after` parameter or,
    /// for the stream, a `Last-Event-ID` header when one is sent.
    async fn open(
        state: Shared,
        identity: &Identity,
        headers: &HeaderMap,
        board: Result<Path<String>, PathRejection>,
        query: &EventsQuery,
        resume: Option<&str>,
    ) -> ApiResult<Self> {
        // The gate authenticated the request before it got here.
        let checked_at = Instant::now();
        let caller = state.caller(identity, headers).await?;
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
        let kinds = match &query.types {
            None => None,
            Some(raw) => Some(parse_event_types(raw)?),
        };
        let label = match &query.label {
            None => None,
            Some(raw) => Some(parse_label_filter(raw)?),
        };
        // The caller's principal, and its declared profile if it has one.
        let mentions = (query.mentions_self == Some(true)).then(|| MentionedSelf {
            principal_id: caller.principal.id.clone(),
            profile: caller.profile.clone(),
        });
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
        // The caller's own actor: its principal under its declared profile.
        let not_from_actor = (query.not_from_self == Some(true)).then(|| caller.actor().id);
        Ok(Self {
            state,
            caller,
            board,
            filter: EventFilter {
                channel,
                thread,
                not_from: query.not_from.clone(),
                not_from_actor,
                kinds,
                mentions,
                label,
            },
            limit,
            after,
            wake,
            checked_at,
            waited: false,
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

    /// How long a credential check stays good; see
    /// [`Limits::credential_recheck`].
    fn recheck_period(&self) -> Duration {
        self.state
            .limits
            .credential_recheck
            .max(Duration::from_millis(10))
    }

    /// Reads from the database whether the caller's credential may still be
    /// used, because a revocation is written by another process. Always
    /// true on a local board. An error counts as unusable.
    async fn recheck(&mut self) -> bool {
        let Some(id) = self.caller.credential_id.clone() else {
            return true;
        };
        // Taken before the read, so the check is never credited with being
        // newer than it is.
        let asked_at = Instant::now();
        let usable = self
            .state
            .db(move |state| state.store.credential_usable(&id))
            .await
            .unwrap_or(false);
        if usable {
            self.checked_at = asked_at;
            self.waited = false;
        }
        usable
    }

    /// Whether events may be sent to the caller now. Nothing is sent unless
    /// this said yes just before. It reads the credential again when the
    /// scan has waited since the last check, or when that check is older
    /// than [`Limits::credential_recheck`]; otherwise the last check stands,
    /// so paging through a backlog does not read it for every page.
    async fn may_deliver(&mut self) -> bool {
        if self.caller.credential_id.is_none() {
            return true;
        }
        if !self.waited && self.checked_at.elapsed() < self.recheck_period() {
            return true;
        }
        self.recheck().await
    }

    /// Ticks every [`Limits::credential_recheck`], starting one period from
    /// now, however often the scan is woken in between.
    fn recheck_timer(&self) -> tokio::time::Interval {
        let period = self.recheck_period();
        let mut timer = tokio::time::interval_at(Instant::now() + period, period);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        timer
    }
}

/// Every event type this server writes.
const EVENT_TYPES: [&str; 18] = event_types::ALL;

/// Parses [`EventsQuery::types`]: event type names joined by commas.
fn parse_event_types(raw: &str) -> ApiResult<Vec<&'static str>> {
    raw.split(',')
        .map(|name| {
            EVENT_TYPES
                .iter()
                .copied()
                .find(|known| *known == name)
                .ok_or_else(|| {
                    AppError::invalid("`types` names an event type this board does not have")
                        .next(format!("use names from: {}", EVENT_TYPES.join(", ")))
                })
        })
        .collect()
}

/// Resolves when the server starts shutting down.
pub(crate) async fn stopping(shutdown: &mut watch::Receiver<bool>) {
    if shutdown.wait_for(|stopping| *stopping).await.is_err() {
        // The server handle is gone, which also means shutdown.
    }
}

async fn get_events(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> ApiResult<Json<EventsResponse>> {
    let Query(query) = query.map_err(bad_query)?;
    let wait = Duration::from_secs(query.wait.unwrap_or(0).min(MAX_WAIT_SECS));
    let deadline = Instant::now() + wait;
    let mut shutdown = state.shutdown.clone();
    let mut scan = EventScan::open(
        state,
        &identity,
        &headers,
        board,
        &query,
        query.after.as_deref(),
    )
    .await?;
    // Only a request that may be held open counts as a long-poll.
    let _place = if wait.is_zero() {
        None
    } else {
        scan.state.waiter(&scan.caller)?
    };
    let has_credential = scan.caller.credential_id.is_some();
    let mut recheck = scan.recheck_timer();

    let mut expired = wait.is_zero();
    loop {
        let (events, more) = scan.next_page().await?;
        if !events.is_empty() || (!more && expired) {
            // The credential the request arrived with may have been revoked
            // or have expired while it was held open or scanning.
            if !scan.may_deliver().await {
                return Err(AppError::unauthenticated());
            }
            return Ok(Json(EventsResponse {
                events,
                cursor: scan.cursor(),
            }));
        }
        if more {
            // The page filled with events the caller may not see. Keep going.
            continue;
        }
        scan.waited = true;
        tokio::select! {
            changed = scan.wake.changed() => expired = changed.is_err(),
            () = tokio::time::sleep_until(deadline) => expired = true,
            () = stopping(&mut shutdown) => expired = true,
            _ = recheck.tick(), if has_credential => {
                if !scan.recheck().await {
                    return Err(AppError::unauthenticated());
                }
                // The wait goes on, so the next delivery checks again.
                scan.waited = true;
            }
        }
    }
}

/// The body of an event stream. `cut` is set when the stream's credential
/// stops being usable; frames that were ready but not yet written are then
/// dropped instead of sent.
struct EventStream {
    frames: mpsc::Receiver<SseEvent>,
    cut: Arc<AtomicBool>,
}

impl futures_core::Stream for EventStream {
    type Item = Result<SseEvent, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.cut.load(Ordering::Acquire) {
            return Poll::Ready(None);
        }
        self.frames.poll_recv(cx).map(|item| item.map(Ok))
    }
}

async fn stream_events(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
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
    let mut scan = EventScan::open(state, &identity, &headers, board, &query, resume).await?;
    let place = scan.state.waiter(&scan.caller)?;
    let has_credential = scan.caller.credential_id.is_some();

    let (tx, frames) = mpsc::channel::<SseEvent>(STREAM_BUFFER);
    let cut = Arc::new(AtomicBool::new(false));
    let revoked = Arc::clone(&cut);
    tokio::spawn(async move {
        // Held until the stream ends.
        let _place = place;
        let mut recheck = scan.recheck_timer();
        // Ends the stream of a credential that is no longer usable, and
        // takes back what was not written yet.
        let end = || revoked.store(true, Ordering::Release);
        loop {
            let Ok((events, more)) = scan.next_page().await else {
                return;
            };
            for event in events {
                let Ok(data) = serde_json::to_string(&event) else {
                    return;
                };
                let frame = SseEvent::default().id(event.cursor).data(data);
                // Room for the frame can take as long as the reader likes.
                // The credential is read again on every tick meanwhile, so
                // a reader that stalls is not owed events past a revocation.
                let room = loop {
                    tokio::select! {
                        room = tx.reserve() => break room,
                        () = stopping(&mut shutdown) => return,
                        _ = recheck.tick(), if has_credential => {
                            if !scan.recheck().await {
                                return end();
                            }
                        }
                    }
                };
                let Ok(room) = room else {
                    return;
                };
                // Nothing is sent without a check that is still good.
                if !scan.may_deliver().await {
                    return end();
                }
                room.send(frame);
            }
            if more {
                continue;
            }
            scan.waited = true;
            tokio::select! {
                changed = scan.wake.changed() => if changed.is_err() { return },
                () = tx.closed() => return,
                () = stopping(&mut shutdown) => return,
                _ = recheck.tick(), if has_credential => {
                    if !scan.recheck().await {
                        return end();
                    }
                    // The wait goes on, so the next delivery checks again.
                    scan.waited = true;
                }
            }
        }
    });

    Ok(Sse::new(EventStream { frames, cut })
        .keep_alive(
            KeepAlive::new()
                .interval(STREAM_KEEP_ALIVE)
                .text("keep-alive"),
        )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_static_get_routes_join_the_token_exemption() {
        for (method, path) in [
            (Method::GET, "/v1/health"),
            (Method::POST, "/v1/join"),
            (Method::POST, "/v1/web/login"),
            (Method::GET, "/"),
            (Method::GET, "/join"),
            (Method::GET, "/assets/app.js"),
            (Method::GET, "/assets/anything-else"),
        ] {
            assert!(needs_no_token(&method, path), "{method} {path}");
        }
        for (method, path) in [
            (Method::POST, "/"),
            (Method::HEAD, "/"),
            (Method::PUT, "/join"),
            (Method::POST, "/assets/app.js"),
            (Method::DELETE, "/assets/app.js"),
            (Method::GET, "/assets"),
            (Method::GET, "/assets/"),
            (Method::GET, "/assets/a/b"),
            (Method::GET, "/assets/../v1/boards"),
            (Method::GET, "/join/"),
            (Method::GET, "//"),
            (Method::GET, "/index.html"),
            (Method::GET, "/v1/boards"),
            (Method::GET, "/v1/whoami"),
            (Method::GET, "/v1/web/session"),
            (Method::POST, "/v1/web/session"),
            (Method::GET, "/v1/web/login"),
            (Method::PUT, "/v1/web/login"),
            (Method::POST, "/v1/web/login/"),
            (Method::GET, "/v1/join/"),
        ] {
            assert!(!needs_no_token(&method, path), "{method} {path}");
        }
    }

    fn headers(pairs: &[(&'static str, &[u8])]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_bytes(value).unwrap());
        }
        headers
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let cookie = |values: &[&[u8]]| {
            let pairs: Vec<(&'static str, &[u8])> =
                values.iter().map(|value| ("cookie", *value)).collect();
            // The name is whatever the board's port makes it; the search is
            // the same for every name.
            session_cookie(&headers(&pairs), "__Host-openboard_session").map(str::to_owned)
        };
        let found = Some("obs_abc".to_owned());
        assert_eq!(cookie(&[b"__Host-openboard_session=obs_abc"]), found);
        assert_eq!(
            cookie(&[b"a=1; __Host-openboard_session=obs_abc; b=2"]),
            found
        );
        assert_eq!(
            cookie(&[b"a=1;__Host-openboard_session=obs_abc;b=2"]),
            found
        );
        assert_eq!(
            cookie(&[b" \t __Host-openboard_session \t= obs_abc \t ;"]),
            found
        );
        // Another header, after ones that hold other applications' cookies:
        // not ASCII, not pairs, empty.
        assert_eq!(
            cookie(&[
                b"theme=d\xc3\xa9j\xc3\xa0; \xff\xfe=1",
                b";;; no-equals ; =; ===",
                b"",
                b"x=y; __Host-openboard_session=obs_abc"
            ]),
            found
        );
        // The first one counts, and a value that holds `=` is kept whole.
        assert_eq!(
            cookie(&[b"__Host-openboard_session=obs_abc; __Host-openboard_session=other"]),
            found
        );
        assert_eq!(
            cookie(&[b"__Host-openboard_session=a=b"]),
            Some("a=b".to_owned())
        );
        assert_eq!(cookie(&[b"__Host-openboard_session="]), Some(String::new()));
        // Names are exact.
        for absent in [
            &b"openboard_session=obs_abc"[..],
            b"__host-openboard_session=obs_abc",
            b"__Host-openboard_session2=obs_abc",
            b"x__Host-openboard_session=obs_abc",
            b"__Host-openboard_session",
            b"other=__Host-openboard_session=obs_abc",
            b"",
        ] {
            assert_eq!(cookie(&[absent]), None, "{absent:?}");
        }
        assert_eq!(cookie(&[]), None);
        // A value that is not text is no session.
        assert_eq!(cookie(&[b"__Host-openboard_session=\xff\xfe"]), None);

        // A hostile header is read once, quickly, and yields nothing.
        let mut huge = b"a=b; ".repeat(200_000);
        huge.extend_from_slice(&b"=".repeat(100_000));
        huge.extend_from_slice(&b";".repeat(100_000));
        let started = std::time::Instant::now();
        assert_eq!(cookie(&[&huge]), None);
        huge.extend_from_slice(b"__Host-openboard_session=obs_abc");
        assert_eq!(cookie(&[&huge]), found);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn the_session_cookie_is_named_after_the_port_in_the_host_header() {
        let port = |host: &[u8]| host_port(&headers(&[("host", host)]));
        for (host, expected) in [
            (&b"board.example:7878"[..], 7878),
            (b"192.168.1.20:7878", 7878),
            (b"localhost:1", 1),
            (b"board.example:65535", 65535),
            (b"board.example:00443", 443),
            (b"[::1]:7878", 7878),
            (b"[2001:db8::1]:8443", 8443),
            // No port: the one a browser leaves out for `https`.
            (b"board.example", 443),
            (b"192.168.1.20", 443),
            (b"[::1]", 443),
        ] {
            assert_eq!(port(host), Some(expected), "{:?}", host.escape_ascii());
        }
        for host in [
            &b""[..],
            b":",
            b":7878",
            b"board.example:",
            b"board.example:port",
            b"board.example:78a",
            b"board.example:+78",
            b"board.example:-1",
            b"board.example: 78",
            b"board.example:78 ",
            b"board.example:65536",
            b"board.example:123456",
            b"board.example:7878:7879",
            b"board.example:7878; x=y",
            b"board.example:7878=obs_x",
            b"board.example:\xef\xbc\x97",
            b"\xff:7878",
            b"::1:7878",
            b"[::1]:",
            b"[::1]7878",
            b"[::1]:78x",
            b"[::1",
        ] {
            assert_eq!(port(host), None, "{:?}", host.escape_ascii());
        }
        // No Host, or more than one, names no port.
        assert_eq!(host_port(&headers(&[])), None);
        assert_eq!(
            host_port(&headers(&[("host", b"a:1"), ("host", b"a:1")])),
            None
        );

        // Only this board's cookie is seen. Another board's is skipped as
        // if it were any other application's.
        let request = headers(&[
            ("host", b"board.example:7878"),
            (
                "cookie",
                b"__Host-openboard_session_7879=obs_other; __Host-openboard_session=obs_old; \
                  __Host-openboard_session_78780=obs_longer; __Host-openboard_session_787=obs_shorter; \
                  __Host-openboard_session_7878=obs_own; __Host-openboard_session_443=obs_default",
            ),
        ]);
        let own = own_cookie_name(&request).unwrap();
        assert_eq!(own, "__Host-openboard_session_7878");
        assert_eq!(session_cookie(&request, &own), Some("obs_own"));
        let elsewhere = headers(&[
            ("host", b"board.example:7878"),
            ("cookie", b"__Host-openboard_session_7879=obs_other"),
        ]);
        assert_eq!(session_cookie(&elsewhere, &own), None);
        // Whatever the Host holds, the name is the prefix and digits.
        for host in [&b"a:7878"[..], b"a", b"[::1]:9"] {
            let name = own_cookie_name(&headers(&[("host", host)])).unwrap();
            let suffix = name
                .strip_prefix(openboard_api::web::SESSION_COOKIE_PREFIX)
                .unwrap();
            assert!(!suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn a_write_is_refused_from_another_origin_and_without_the_csrf_token() {
        let check = |mode, method: &Method, pairs: &[(&'static str, &[u8])], csrf| {
            check_csrf(mode, method, &headers(pairs), csrf)
                .map_err(|error| (error.status, error.code))
        };
        let refused = Err((StatusCode::FORBIDDEN, codes::CSRF_FAILED));
        let host: (&'static str, &[u8]) = ("host", b"192.168.1.20:7878");
        let writes = [Method::POST, Method::PUT, Method::PATCH, Method::DELETE];

        for method in &writes {
            // No Origin, or the board's own.
            assert_eq!(check(Mode::Network, method, &[host], None), Ok(()));
            assert_eq!(check(Mode::Network, method, &[], None), Ok(()));
            for own in [
                &b"https://192.168.1.20:7878"[..],
                b"HTTPS://192.168.1.20:7878",
            ] {
                let pairs = [host, ("origin", own)];
                assert_eq!(
                    check(Mode::Network, method, &pairs, None),
                    Ok(()),
                    "{own:?}"
                );
            }
            for foreign in [
                &b"https://evil.example"[..],
                b"http://192.168.1.20:7878",
                b"https://192.168.1.20",
                b"https://192.168.1.20:7879",
                b"https://192.168.1.20:7878/",
                b"https://192.168.1.20:7878.evil.example",
                b"https://evil.example/https://192.168.1.20:7878",
                b"192.168.1.20:7878",
                b"null",
                b"",
                b"https://",
                b"https://\xff",
            ] {
                let pairs = [host, ("origin", foreign)];
                assert_eq!(
                    check(Mode::Network, method, &pairs, None),
                    refused,
                    "{foreign:?}"
                );
            }
            // An Origin cannot be checked without exactly one Host.
            let own: (&'static str, &[u8]) = ("origin", b"https://192.168.1.20:7878");
            assert_eq!(check(Mode::Network, method, &[own], None), refused);
            assert_eq!(
                check(Mode::Network, method, &[host, host, own], None),
                refused
            );
            assert_eq!(
                check(Mode::Network, method, &[host, own, own], None),
                refused
            );
            let empty = [("host", &b""[..]), ("origin", &b"https://"[..])];
            assert_eq!(check(Mode::Network, method, &empty, None), refused);

            // A local board is plain HTTP.
            let local: (&'static str, &[u8]) = ("host", b"localhost:7878");
            let pairs = [local, ("origin", &b"http://localhost:7878"[..])];
            assert_eq!(check(Mode::Local, method, &pairs, None), Ok(()));
            for foreign in [
                &b"https://localhost:7878"[..],
                b"http://localhost:3000",
                b"http://127.0.0.1:7878",
                b"http://evil.example",
            ] {
                let pairs = [local, ("origin", foreign)];
                assert_eq!(check(Mode::Local, method, &pairs, None), refused);
            }

            // A cookie session needs its token as well.
            let token = Some("a1b2");
            assert_eq!(check(Mode::Network, method, &[host], token), refused);
            for wrong in [&b"a1b3"[..], b"a1b", b"a1b2 ", b"", b"A1B2", b"\xff"] {
                let pairs = [host, (CSRF_HEADER, wrong)];
                assert_eq!(check(Mode::Network, method, &pairs, token), refused);
            }
            let pairs = [host, (CSRF_HEADER, &b"a1b2"[..])];
            assert_eq!(check(Mode::Network, method, &pairs, token), Ok(()));
            // The right token does not excuse a foreign Origin.
            let pairs = [
                host,
                (CSRF_HEADER, &b"a1b2"[..]),
                ("origin", &b"https://evil.example"[..]),
            ];
            assert_eq!(check(Mode::Network, method, &pairs, token), refused);
        }

        // Reads are never checked.
        for method in [Method::GET, Method::HEAD] {
            let pairs = [host, ("origin", &b"https://evil.example"[..])];
            assert_eq!(check(Mode::Network, &method, &pairs, Some("a1b2")), Ok(()));
        }
    }

    #[test]
    fn event_type_lists() {
        assert_eq!(
            parse_event_types("message.posted,labels.changed").unwrap(),
            ["message.posted", "labels.changed"]
        );
        for bad in [
            "",
            "message.posted,",
            "posted",
            "message.posted, message.edited",
            "*",
        ] {
            assert!(parse_event_types(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn label_filters() {
        let filter = |raw: &str| parse_label_filter(raw).ok();
        assert_eq!(
            filter("status"),
            Some(LabelFilter {
                key: "status".into(),
                values: vec![]
            })
        );
        assert_eq!(
            filter("status=open|in progress"),
            Some(LabelFilter {
                key: "status".into(),
                values: vec!["open".into(), "in progress".into()]
            })
        );
        // A filter may name the reserved keys the board writes.
        assert_eq!(
            filter("board.task=ready|claimed").unwrap().key,
            "board.task"
        );
        // Only the first `=` separates the key from the values.
        assert_eq!(filter("k=a=b").unwrap().values, ["a=b"]);
        for bad in [
            "",
            "=x",
            "Status",
            "status=",
            "status=a|",
            "board.",
            "board.board.x",
            "a b=c",
        ] {
            assert_eq!(filter(bad), None, "{bad:?}");
        }
    }

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

    #[test]
    fn the_host_guard_takes_only_whole_loopback_names() {
        let accepted = [
            "localhost",
            "LocalHost",
            "LOCALHOST",
            "localhost:1",
            "localhost:80",
            "localhost:65535",
            "localhost:00080",
            "127.0.0.1",
            "127.0.0.1:7878",
            "127.1.2.3",
            "127.255.255.254:9",
            "[::1]",
            "[::1]:7878",
            "[::1]:0",
            "[0:0:0:0:0:0:0:1]",
            "[0000:0000:0000:0000:0000:0000:0000:0001]:7878",
            "[::0:1]",
        ];
        for host in accepted {
            assert!(is_loopback_host(host), "{host:?} should be accepted");
        }
        let refused = [
            // Something after the bracket that is not a port.
            "[::1].evil.com",
            "[::1].evil.com:7878",
            "[::1]evil.com",
            "[::1]:7878.evil.com",
            "[::1]:7878:evil.com",
            "[::1]@evil.com",
            "[::1]/",
            "[::1] ",
            "[::1]]",
            "[::1][::1]",
            "[::1]:",
            "[::1]:+80",
            "[::1]:-1",
            "[::1]:80a",
            "[::1]: 80",
            "[::1]:65536",
            "[::1]:123456",
            "[::1]::80",
            // Brackets around something that is not the IPv6 loopback.
            "[]",
            "[",
            "]",
            "[::1",
            "::1]",
            "[::]",
            "[::2]",
            "[::1%lo0]",
            "[::1%25lo0]",
            "[ ::1]",
            "[::1 ]",
            "[[::1]]",
            "[localhost]",
            "[127.0.0.1]",
            "[127.0.0.1]:7878",
            "[::ffff:127.0.0.1]",
            "[::ffff:127.0.0.1]:7878",
            "[::ffff:7f00:1]",
            "[::127.0.0.1]",
            "[0:0:0:0:0:ffff:127.0.0.1]",
            "[fe80::1]",
            "[2001:db8::1]",
            // IPv6 without brackets is not a Host.
            "::1",
            "::1:7878",
            "0:0:0:0:0:0:0:1",
            "::ffff:127.0.0.1",
            // Names that only start or end like a loopback one.
            "localhost.",
            "localhost.:7878",
            "localhost.evil.com",
            "localhost.localdomain",
            "evil.com.localhost",
            "evil-localhost",
            "localhost-evil",
            "notlocalhost",
            "127.0.0.1.evil.com",
            "127.0.0.1.",
            "127.0.0.1.:80",
            "evil.com:127.0.0.1",
            // IPv4 spellings a resolver may take but a Host must not use.
            "127.1",
            "127.0.1",
            "2130706433",
            "0x7f.0.0.1",
            "0x7f000001",
            "0177.0.0.1",
            "127.0.0.01",
            "127.0.0.1.1",
            "127.0.0.256",
            "128.0.0.1",
            "126.255.255.255",
            "0.0.0.0",
            "0",
            // Ports.
            "localhost:",
            "127.0.0.1:",
            "localhost:port",
            "localhost:80a",
            "localhost:8 0",
            "localhost:+80",
            "localhost:-80",
            "localhost:65536",
            "localhost:999999",
            "localhost:0x50",
            "localhost:80:80",
            "localhost::80",
            "127.0.0.1:80:80",
            ":80",
            ":",
            // User information, paths and other parts of a URL.
            "user@localhost",
            "user:pass@localhost",
            "localhost@evil.com",
            "localhost:80@evil.com",
            "evil.com@127.0.0.1",
            "evil.com#@localhost",
            "localhost/",
            "localhost/evil.com",
            "localhost#evil.com",
            "localhost?evil.com",
            "localhost\\evil.com",
            "http://localhost",
            "//localhost",
            // White space, control characters and separators.
            " localhost",
            "localhost ",
            "local host",
            "localhost\t",
            "\tlocalhost",
            "localhost\r\n",
            "localhost\n",
            "localhost\0",
            "localhost\0.evil.com",
            " 127.0.0.1",
            "127.0.0.1 ",
            "127.0.0.1 evil.com",
            "localhost,evil.com",
            "localhost, evil.com",
            "evil.com,localhost",
            "localhost;evil.com",
            // Letters that only look or fold like ASCII ones.
            "ⓛocalhost",
            "localhoſt",
            "LOCALHOST\u{212a}",
            "１２７.０.０.１",
            "127。0。0。1",
            "",
        ];
        for host in refused {
            assert!(!is_loopback_host(host), "{host:?} should be refused");
        }
    }

    fn rendered_message(text: &str) -> Message {
        Message {
            id: "msg_1".to_owned(),
            board: "general".to_owned(),
            channel: "dev".to_owned(),
            thread_id: "msg_1".to_owned(),
            by: Principal::local().actor(None),
            text: text.to_owned(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            revision: 1,
            credential_id: None,
            edited_at: None,
            deleted_at: None,
            labels: Default::default(),
            reactions: Vec::new(),
            mentions: Vec::new(),
            html: None,
            task: None,
        }
    }

    #[test]
    fn the_html_of_one_response_is_bounded() {
        // Each message renders to a little under its own limit.
        let text = "> a\n\n".repeat(2000);
        let each = markdown::render_html(&text, &[]).len();
        assert!(each > 6 * text.len() && each <= markdown::html_limit(&text));
        let fit = MAX_HTML_BYTES_PER_RESPONSE.div_ceil(each);

        let mut budget = HtmlBudget::default();
        let mut rendered = 0;
        let mut total = 0;
        for index in 0..fit + 200 {
            let mut message = rendered_message(&text);
            budget.add_html(&mut message);
            let html = message.html.expect("every message carries HTML");
            total += html.len();
            if index < fit {
                assert!(html.starts_with("<blockquote>"), "message {index}");
                rendered += html.len();
            } else {
                // Past the budget: the whole text, escaped, and no parsing.
                assert_eq!(html, markdown::plain_html(&text), "message {index}");
            }
        }
        assert!(rendered >= MAX_HTML_BYTES_PER_RESPONSE);
        assert!(rendered < MAX_HTML_BYTES_PER_RESPONSE + markdown::html_limit(&text));
        // What follows the budget costs a small multiple of its own text.
        assert!(total - rendered <= 200 * (6 * text.len() + 11));

        // A tombstone has no HTML and uses none of the budget.
        let mut budget = HtmlBudget::default();
        let mut tombstone = rendered_message("");
        tombstone.deleted_at = Some("2026-01-02T00:00:00Z".to_owned());
        budget.add_html(&mut tombstone);
        assert_eq!(tombstone.html.as_deref(), Some(""));
        assert_eq!(budget.rendered, 0);
    }
}
