//! The HTTP surface: routes, request validation and error mapping.
//!
//! Handlers never log request or message content, tokens or enrolment codes
//! (design P15).
//!
//! Every request passes [`gate`] before anything else looks at it. On a
//! local board the gate checks the `Host` header. On a network board it
//! authenticates the bearer token, so a caller without a valid one learns
//! nothing about boards, routes or the shape of valid requests.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, PathRejection, QueryRejection};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use openboard_api::auth::{JoinRequest, JoinResponse, WhoAmI};
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
use crate::identity::{log_kinds, token_lookup_hash, AuthOutcome, Credential, LogEntry};
use crate::limits::{
    address_key, Hit, Limits, RateTable, WaiterGuard, Waiters, FAILURE_LOG_PER_MINUTE,
};
use crate::names::{self, ChannelSel};
use crate::store::{
    event_cursor, parse_cursor, thread_cursor, BoardRef, EventFilter, NewPost, PostTarget, Store,
    StoreError,
};
use crate::tls::ClientAddr;
use crate::Mode;

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
    mode: Mode,
    max_message_chars: usize,
    limits: Limits,
    /// Requests per credential.
    requests: RateTable<String>,
    /// Enrolment attempts per client address.
    joins: RateTable<IpAddr>,
    /// Failed authentications per client address.
    auth_failures: RateTable<IpAddr>,
    /// Failure entries written to the security log, over all clients.
    failure_log: RateTable<()>,
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
    pub shutdown: watch::Receiver<bool>,
    pub in_use: mpsc::Sender<()>,
}

pub(crate) fn router(parts: Parts) -> Router {
    let max_message_chars = parts.max_message_chars;
    let state = Arc::new(AppState {
        store: parts.store,
        authz: parts.authz,
        hub: Hub::default(),
        mode: parts.mode,
        max_message_chars,
        limits: parts.limits,
        requests: RateTable::new(),
        joins: RateTable::new(),
        auth_failures: RateTable::new(),
        failure_log: RateTable::new(),
        waiters: Arc::new(Waiters::default()),
        shutdown: parts.shutdown,
        _in_use: parts.in_use,
    });
    // Room for the largest allowed text with every character escaped.
    let body_limit = max_message_chars
        .saturating_mul(12)
        .saturating_add(16 * 1024);
    Router::new()
        .route(HEALTH_PATH, get(health))
        .route(JOIN_PATH, post(join))
        .route("/v1/whoami", get(whoami))
        .route("/v1/logout", post(logout))
        .route("/v1/boards/{board}/messages", post(post_message))
        .route("/v1/boards/{board}/channels", get(list_channels))
        .route("/v1/boards/{board}/threads", get(list_threads))
        .route("/v1/boards/{board}/threads/{thread_id}", get(get_thread))
        .route("/v1/boards/{board}/events", get(get_events))
        .route("/v1/boards/{board}/events/stream", get(stream_events))
        .fallback(unknown_route)
        .method_not_allowed_fallback(wrong_method)
        .layer(DefaultBodyLimit::max(body_limit))
        // The outermost layer: it runs before routing, extraction and
        // every handler.
        .layer(middleware::from_fn_with_state(Arc::clone(&state), gate))
        .with_state(state)
}

/// The two routes a network board answers without a token.
const HEALTH_PATH: &str = "/v1/health";
const JOIN_PATH: &str = "/v1/join";

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
            codes::NOT_FOUND => StatusCode::NOT_FOUND,
            codes::FORBIDDEN | codes::INVITE_INVALID => StatusCode::FORBIDDEN,
            codes::UNAUTHENTICATED => StatusCode::UNAUTHORIZED,
            codes::RATE_LIMITED => StatusCode::TOO_MANY_REQUESTS,
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
            retry_after: None,
        }
    }

    /// The one answer to every request a network board cannot authenticate:
    /// no token, a malformed one, an unknown one, an expired one or a
    /// revoked one. It never varies, so it tells the caller nothing.
    fn unauthenticated() -> Self {
        Self::new(
            codes::UNAUTHENTICATED,
            "this board needs a valid bearer token",
        )
        .next("enrol with an invite from the board's operator, or ask for a new one if your token has expired or been revoked")
    }

    /// The one answer to every enrolment code that cannot be used.
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

/// Who a request comes from, as [`gate`] established it. Handlers read it
/// from the request and never from anything the client sent.
#[derive(Debug, Clone)]
struct Identity {
    principal: Principal,
    /// The credential behind an authenticated request on a network board.
    credential: Option<Credential>,
    client: Option<IpAddr>,
}

/// The first thing every request meets.
///
/// Local mode has no auth, so a web page must not be able to reach the board
/// by pointing a hostname it controls at loopback (DNS rebinding). Requests
/// whose `Host` is not a loopback name are refused.
///
/// Network mode authenticates instead: clients address the board by its LAN
/// address, and only the health check and enrolment are answered without a
/// valid token. An unauthenticated request is refused here, before routing,
/// so unknown routes, bad paths and bad bodies all look the same to it.
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
                client,
            }
        }
        Mode::Network => {
            let path = request.uri().path();
            if path == HEALTH_PATH || path == JOIN_PATH {
                Identity {
                    principal: Principal::anonymous(),
                    credential: None,
                    client,
                }
            } else {
                match state.authenticate(request.headers(), client).await {
                    Ok(credential) => Identity {
                        principal: credential.principal(),
                        credential: Some(credential),
                        client,
                    },
                    Err(refused) => return refused.into_response(),
                }
            }
        }
    };
    request.extensions_mut().insert(identity);
    next.run(request).await
}

/// The token of an `Authorization: Bearer <token>` header.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

impl AppState {
    /// Resolves the bearer token to its credential, and counts the request
    /// against that credential's rate limit. Every failure is the same
    /// `401`, except that a client address with too many failures is told
    /// to slow down.
    async fn authenticate(
        self: &Arc<Self>,
        headers: &HeaderMap,
        client: Option<IpAddr>,
    ) -> ApiResult<Credential> {
        // Text that cannot be a token is refused without a lookup. A token
        // is found by its digest, never by comparing secrets.
        let presented = bearer_token(headers);
        let outcome = match presented.and_then(token_lookup_hash) {
            None => None,
            Some(hash) => Some(
                self.db(move |state| state.store.authenticate(&hash))
                    .await?,
            ),
        };
        let (reason, known) = match outcome {
            Some(AuthOutcome::Valid(credential)) => {
                return match self
                    .requests
                    .hit(credential.id.clone(), self.limits.requests_per_minute)
                {
                    Hit::Allowed => Ok(credential),
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
            Some(AuthOutcome::Expired(credential)) => ("expired token", Some(credential)),
            Some(AuthOutcome::Revoked(credential)) => ("revoked token", Some(credential)),
            Some(AuthOutcome::Unknown) => ("unknown token", None),
            None if presented.is_some() => ("malformed token", None),
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
                    self.log(
                        log_kinds::RATE_LIMITED,
                        None,
                        client,
                        Some("failed authentication"),
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

    /// Logs a failed authentication or enrolment, within a budget shared by
    /// all clients so that failures cannot fill the disk. When the budget
    /// for the minute is spent, one entry says so and the rest are dropped.
    async fn log_failure(
        self: &Arc<Self>,
        kind: &'static str,
        credential: Option<&Credential>,
        client: Option<IpAddr>,
        reason: &'static str,
    ) {
        match self.failure_log.hit((), FAILURE_LOG_PER_MINUTE) {
            Hit::Allowed => self.log(kind, credential, client, Some(reason)).await,
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

/// Who is calling: the principal [`gate`] established, plus the profile the
/// caller declared. The profile header only picks a label.
struct Caller {
    principal: Principal,
    profile: Option<String>,
    /// The credential the caller authenticated with, on a network board.
    credential_id: Option<String>,
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
        })
    }

    /// The actor the caller writes as.
    fn actor(&self) -> openboard_api::Actor {
        self.principal.actor(self.profile.as_deref())
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
    let caller = Caller::new(&identity, &headers)?;
    state.require_global(&caller.principal, Action::Health)?;
    Ok(Json(Health {
        status: "ok".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        mode: state.mode.as_str().to_owned(),
    }))
}

/// `POST /v1/join`: swaps a single-use enrolment code for a token.
async fn join(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<Response> {
    if state.mode == Mode::Local {
        return Err(AppError::invalid(
            "this is a local board: it has no enrolment and needs no token",
        ));
    }
    // Counted before anything is read, successful or not.
    if let Hit::Limited { retry_after, first } = state
        .joins
        .hit(client_key(identity.client), state.limits.joins_per_minute)
    {
        if first {
            state
                .log(
                    log_kinds::RATE_LIMITED,
                    None,
                    identity.client,
                    Some("enrolment"),
                )
                .await;
        }
        return Err(AppError::rate_limited(retry_after));
    }
    state.require_global(&identity.principal, Action::Enrol)?;
    if !is_json(&headers) {
        return Err(
            AppError::invalid("the request body must be sent as application/json")
                .status(StatusCode::UNSUPPORTED_MEDIA_TYPE),
        );
    }
    let body = body.map_err(|_| AppError::invalid("the request body could not be read"))?;
    // The error never quotes the body, which may hold a code.
    let request: JoinRequest = serde_json::from_slice(&body)
        .map_err(|_| AppError::invalid("the request body is not a valid join request"))?;

    let client = identity.client;
    let joined = state
        .db(move |state| state.store.join(&request.code, client))
        .await?;
    let Some((token, credential)) = joined else {
        state
            .log_failure(log_kinds::ENROL_FAILED, None, client, "unusable code")
            .await;
        return Err(AppError::invite_invalid());
    };
    let response = JoinResponse {
        token,
        credential: credential.info(),
    };
    // The token is in this response and nowhere else. Nothing may keep it.
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(response)).into_response())
}

/// `GET /v1/whoami`: the caller as the board records it.
async fn whoami(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
) -> ApiResult<Json<WhoAmI>> {
    let caller = Caller::new(&identity, &headers)?;
    state.require_global(&caller.principal, Action::Session)?;
    Ok(Json(WhoAmI {
        actor: caller.actor(),
        mode: state.mode.as_str().to_owned(),
        credential: identity.credential.as_ref().map(Credential::info),
    }))
}

/// `POST /v1/logout`: revokes the credential the caller authenticated with.
async fn logout(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
) -> ApiResult<StatusCode> {
    state.require_global(&identity.principal, Action::Session)?;
    let Some(credential) = identity.credential else {
        return Err(AppError::invalid(
            "this is a local board: there is no credential to revoke",
        ));
    };
    let client = identity.client;
    state
        .db(move |state| state.store.logout(&credential, client))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn post_message(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<(StatusCode, Json<PostMessageResponse>)> {
    let caller = Caller::new(&identity, &headers)?;
    let Path(board_name) = board.map_err(|_| bad_path())?;
    names::validate_board(&board_name)
        .map_err(|reason| AppError::new(codes::INVALID_BOARD, reason))?;
    let limit = state.max_message_chars;
    let too_large = |detail: String| {
        AppError::new(codes::MESSAGE_TOO_LARGE, detail)
            .next("shorten the message or split it across replies")
    };

    // Requiring JSON keeps a web page from posting here with a plain form.
    if !is_json(&headers) {
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

    // The author and the credential come from the gate, never the body.
    let actor = caller.actor();
    let idempotency = key.map(|key| (key, request_fingerprint(&request)));
    let post = NewPost {
        channel,
        thread_id,
        actor,
        principal_id: caller.principal.id.clone(),
        credential_id: caller.credential_id.clone(),
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
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
) -> ApiResult<Json<ChannelList>> {
    let caller = Caller::new(&identity, &headers)?;
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
    let caller = Caller::new(&identity, &headers)?;
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
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> ApiResult<Json<Thread>> {
    let caller = Caller::new(&identity, &headers)?;
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
        identity: &Identity,
        headers: &HeaderMap,
        board: Result<Path<String>, PathRejection>,
        query: &EventsQuery,
        resume: Option<&str>,
    ) -> ApiResult<Self> {
        let caller = Caller::new(identity, headers)?;
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

    /// Whether the caller's credential may still be used, read from the
    /// database, because a revocation is written by another process. Always
    /// true on a local board.
    async fn credential_usable(&self) -> ApiResult<bool> {
        match &self.caller.credential_id {
            None => Ok(true),
            Some(id) => {
                let id = id.clone();
                self.state
                    .db(move |state| state.store.credential_usable(&id))
                    .await
            }
        }
    }

    /// Ticks every [`Limits::credential_recheck`], starting one period from
    /// now, however often the scan is woken in between.
    fn recheck_timer(&self) -> tokio::time::Interval {
        let period = self
            .state
            .limits
            .credential_recheck
            .max(Duration::from_millis(10));
        let mut timer = tokio::time::interval_at(Instant::now() + period, period);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        timer
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
    // True once the request has been held open, after which the credential
    // it arrived with may have been revoked or have expired.
    let mut held = false;
    loop {
        let (events, more) = scan.next_page().await?;
        if !events.is_empty() || (!more && expired) {
            if held && !scan.credential_usable().await? {
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
        held = true;
        tokio::select! {
            changed = scan.wake.changed() => expired = changed.is_err(),
            () = tokio::time::sleep_until(deadline) => expired = true,
            () = stopping(&mut shutdown) => expired = true,
            _ = recheck.tick(), if has_credential => {
                if !scan.credential_usable().await? {
                    return Err(AppError::unauthenticated());
                }
            }
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

    let (tx, rx) = mpsc::channel::<SseEvent>(16);
    tokio::spawn(async move {
        // Held until the stream ends.
        let _place = place;
        let mut recheck = scan.recheck_timer();
        // False until the first batch: the request was authenticated a
        // moment ago.
        let mut held = false;
        loop {
            let Ok((events, more)) = scan.next_page().await else {
                return;
            };
            // A stream whose credential was revoked or has expired ends
            // here, before it delivers anything more.
            if held && !events.is_empty() && !scan.credential_usable().await.unwrap_or(false) {
                return;
            }
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
            held = true;
            tokio::select! {
                changed = scan.wake.changed() => if changed.is_err() { return },
                () = tx.closed() => return,
                () = stopping(&mut shutdown) => return,
                _ = recheck.tick(), if has_credential => {
                    if !scan.credential_usable().await.unwrap_or(false) {
                        return;
                    }
                }
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
