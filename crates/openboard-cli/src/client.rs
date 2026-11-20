//! The HTTP client for one board.
//!
//! It talks only to the configured board URL: no proxy from the environment,
//! no redirects, no other hosts.
//!
//! A local board is plain HTTP and gets no token, whatever the environment
//! holds. A network board is HTTPS with its certificate pinned (see
//! [`crate::tls`]) and gets the bearer token on every request.

use std::time::Duration;

use std::sync::Arc;

use openboard_api::auth::{parse_fingerprint, JoinRequest, JoinResponse, WhoAmI};
use openboard_api::tasks::{
    ClaimReadyRequest, ClaimReadyResponse, CreateTaskRequest, CreateTaskResponse,
    CreateTasksRequest, CreateTasksResponse, TaskActionRequest, TaskList, TaskView, TasksQuery,
};
use openboard_api::{
    codes as api_codes, BoardList, ChannelList, EditMessageRequest, ErrorBody, EventsQuery,
    EventsResponse, Health, LabelsRequest, Message, PostMessageRequest, PostMessageResponse,
    Thread, ThreadList, ThreadsQuery, API_LEVEL, CLIENT_API_LEVEL_HEADER, PROFILE_HEADER,
};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Method, RequestBuilder, Response, Url};
use serde::de::DeserializeOwned;

use crate::compat::{self, Requirement, Seen, ServerInfo, GRANTS, TASKS};
use crate::error::{codes, Failure};
use crate::logins::Secret;
use crate::target::{bad_url, Target};
use crate::tls::{self, Refusal, Verdict};

/// What to do when a board no longer accepts a login.
pub const NEW_INVITE_HINT: &str = "ask the board's operator for a new invite (`openboard invite \
                                   <name>` on the board's host), then run `openboard join \
                                   <invite>`";

/// How long to wait for a TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an ordinary request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Extra time allowed on top of a long-poll's own wait.
const LONG_POLL_GRACE: Duration = Duration::from_secs(30);

pub struct Client {
    http: reqwest::Client,
    base: Url,
    board: String,
    profile: Option<String>,
    /// Sent as a bearer token. Only ever set for an `https://` base.
    token: Option<Secret>,
    /// The pinned fingerprint of an `https://` board.
    fingerprint: Option<String>,
    /// What the certificate verifier decided, for error reports.
    verdict: Arc<Verdict>,
    /// See [`Target::assumed`].
    assumed: bool,
    /// What the board has said about its version in this invocation.
    seen: Seen,
}

/// A write to one existing message, for choosing the hint when it fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Edit,
    Delete,
    Label,
    React,
}

impl Change {
    fn verb(self) -> &'static str {
        match self {
            Change::Edit => "edit",
            Change::Delete => "delete",
            Change::Label => "label",
            Change::React => "react to",
        }
    }
}

/// What to do about a local board that cannot be reached. When the board
/// was only assumed, because no URL was given and no login is stored, the
/// caller may have meant a network board it has not joined, or has logged
/// out of.
fn local_unreachable_hint(assumed: bool) -> &'static str {
    if assumed {
        "start a board with `openboard serve`, or run `openboard join <invite>` to use a \
         network board: no login is stored and no --url (or BOARD_URL) was given, so the \
         local default was tried"
    } else {
        "start a board with `openboard serve`, or point --url (or BOARD_URL) at a running board"
    }
}

impl Client {
    pub fn new(target: &Target, board: &str, profile: Option<&str>) -> Result<Self, Failure> {
        let url = target.url.as_str();
        let base = Url::parse(url)
            .map_err(|error| bad_url(url, &format!("is not a valid URL: {error}")))?;
        if base.host_str().is_none() {
            return Err(bad_url(url, "names no host"));
        }
        let (pin, token) = match base.scheme() {
            // A local board. No token is ever sent over plain HTTP.
            "http" => (None, None),
            "https" => {
                let pin = target
                    .fingerprint
                    .as_deref()
                    .and_then(parse_fingerprint)
                    .ok_or_else(|| {
                        Failure::usage(format!(
                            "no certificate fingerprint is known for {url}, so the connection \
                             cannot be pinned"
                        ))
                        .next(format!(
                            "{NEW_INVITE_HINT}; the invite carries the fingerprint"
                        ))
                    })?;
                (Some(pin), target.token.clone())
            }
            _ => return Err(bad_url(url, "must start with http:// or https://")),
        };
        let network = pin.is_some();
        let failed = |detail: String| {
            Failure::new(
                codes::REQUEST_FAILED,
                format!("cannot set up the HTTP client: {detail}"),
            )
        };
        // Every client gets this configuration, so TLS can only ever succeed
        // against the pinned certificate. A local board's client pins none.
        let (tls, verdict) = tls::client_config(pin).map_err(|error| failed(error.to_string()))?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(format!("openboard/{}", openboard_api::build::VERSION))
            .use_preconfigured_tls(tls)
            .https_only(network)
            .build()
            .map_err(|error| failed(error.to_string()))?;
        Ok(Self {
            http,
            base,
            board: board.to_owned(),
            profile: profile.map(str::to_owned),
            token,
            fingerprint: target.fingerprint.clone().filter(|_| network),
            verdict,
            assumed: target.assumed,
            seen: Seen::default(),
        })
    }

    /// What the board said about itself on its latest answer. `None` until
    /// it has answered anything.
    pub fn server(&self) -> Option<ServerInfo> {
        self.seen.info()
    }

    /// Rewrites the hint of a failure that a board too old for
    /// `requirement` explains; see [`compat::explain`].
    pub fn needs(&self, requirement: &Requirement, failure: Failure) -> Failure {
        compat::explain(failure, self.server().as_ref(), requirement)
    }

    fn is_network(&self) -> bool {
        self.base.scheme() == "https"
    }

    /// `<base>/v1/<rest...>`, with every segment escaped.
    fn root(&self, rest: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.set_query(None);
        url.set_fragment(None);
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty().push("v1").extend(rest);
        }
        url
    }

    /// `<base>/v1/boards/<board>/<rest...>`, with every segment escaped.
    fn endpoint(&self, rest: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.set_query(None);
        url.set_fragment(None);
        if let Ok(mut segments) = url.path_segments_mut() {
            segments
                .pop_if_empty()
                .extend(["v1", "boards", self.board.as_str()])
                .extend(rest);
        }
        url
    }

    fn request(&self, method: Method, url: Url, timeout: Option<Duration>) -> RequestBuilder {
        let mut request = self
            .http
            .request(method, url)
            .header(CLIENT_API_LEVEL_HEADER, API_LEVEL.to_string());
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        // An empty profile header is an error, so it is only sent when set.
        if let Some(profile) = &self.profile {
            request = request.header(PROFILE_HEADER, profile);
        }
        if let (Some(token), true) = (&self.token, self.is_network()) {
            let mut value =
                reqwest::header::HeaderValue::try_from(format!("Bearer {}", token.expose()))
                    // A token with characters no header can carry is no token; the
                    // board will answer `unauthenticated`.
                    .unwrap_or_else(|_| {
                        reqwest::header::HeaderValue::from_static("Bearer invalid")
                    });
            value.set_sensitive(true);
            request = request.header(AUTHORIZATION, value);
        }
        request
    }

    async fn send(&self, request: RequestBuilder) -> Result<Response, Failure> {
        let response = request
            .send()
            .await
            .map_err(|error| self.transport(&error))?;
        // Every answer says which version of the board gave it.
        self.seen.record(response.headers());
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok());
        let body = response
            .bytes()
            .await
            .map_err(|error| self.transport(&error))?;
        let mut failure = match serde_json::from_slice::<ErrorBody>(&body) {
            Ok(body) => self.api_failure(body, retry_after),
            Err(_) => Failure::new(
                codes::BAD_RESPONSE,
                format!(
                    "{} answered HTTP {status} without an Openboard error body",
                    self.base
                ),
            )
            .next("check that --url or BOARD_URL points at an Openboard server"),
        };
        failure.status = Some(status.as_u16());
        Err(failure)
    }

    async fn json<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T, Failure> {
        let body = self
            .send(request)
            .await?
            .bytes()
            .await
            .map_err(|error| self.transport(&error))?;
        serde_json::from_slice(&body).map_err(|error| {
            Failure::new(
                codes::BAD_RESPONSE,
                format!(
                    "{} sent a response this client cannot read: {error}",
                    self.base
                ),
            )
            .next("check that --url or BOARD_URL points at an Openboard server of this version")
        })
    }

    /// Turns a server error into a failure, replacing API-level hints with
    /// the CLI command that does the same thing.
    fn api_failure(&self, body: ErrorBody, retry_after: Option<u64>) -> Failure {
        let error = body.error;
        let next = match error.code.as_str() {
            api_codes::INVALID_CURSOR => Some(
                "use a cursor printed by this board; omit --after to start from now, or run \
                 `openboard tail --from-start` to see every event"
                    .to_owned(),
            ),
            api_codes::INVALID_PROFILE => {
                Some("fix or drop --as (or BOARD_AS), for example `--as agent-a`".to_owned())
            }
            // The usual cause is a wrong thread ID. Requests that name no
            // thread replace this hint; see `board_missing`.
            api_codes::NOT_FOUND => Some(
                "run `openboard channels`, then `openboard read <channel>` to find thread IDs"
                    .to_owned(),
            ),
            api_codes::INVALID_CHANNEL => Some(
                "use a path like `research/web`: lowercase letters, digits, '_' and '-' joined \
                 by '/'"
                    .to_owned(),
            ),
            api_codes::IDEMPOTENCY_CONFLICT => Some("use a new --key for a new message".to_owned()),
            api_codes::UNAUTHENTICATED => Some(format!(
                "this login is not accepted (any more): it was revoked, has expired or is not \
                 for this board; {NEW_INVITE_HINT}"
            )),
            api_codes::INVITE_INVALID => Some(
                "the invite has expired, was already used or was mistyped; an invite works \
                 once, so ask the board's operator for a new one (`openboard invite <name>` on \
                 the board's host)"
                    .to_owned(),
            ),
            api_codes::RATE_LIMITED => Some(match retry_after {
                Some(seconds) => format!("retry after {seconds} seconds"),
                None => "wait a minute, then retry".to_owned(),
            }),
            // Requests that name a message say more; see `about_message`.
            api_codes::REVISION_CONFLICT => Some(
                "the message changed since that revision: read it again with `openboard read \
                 --thread <id>`, then retry with its current --revision"
                    .to_owned(),
            ),
            api_codes::LABEL_CONFLICT => Some(
                "an --if or --if-unset condition did not hold, so nothing changed: read the \
                 message again with `openboard read --thread <id>` to see its labels"
                    .to_owned(),
            ),
            api_codes::INVALID_LABEL => Some(
                "write a label as key=value: the key is 1 to 64 lowercase letters, digits, '.', \
                 '_' or '-' and does not start with `board.`; the value is 1 to 256 bytes with \
                 no control characters; a message holds at most 32 labels"
                    .to_owned(),
            ),
            api_codes::INVALID_REACTION => Some(
                "use a name, not an emoji character: 1 to 32 lowercase letters, digits, '_', \
                 '+' or '-', such as `thumbsup` or `+1`"
                    .to_owned(),
            ),
            api_codes::MESSAGE_DELETED => Some(
                "a deleted message cannot be edited, labelled or reacted to; `openboard reply` \
                 to its thread still works"
                    .to_owned(),
            ),
            // The CLI sends a token and never a cookie or an `Origin`
            // header, so something between it and the board added one.
            api_codes::CSRF_FAILED => Some(format!(
                "the board took this for a request from a web page, which this command never \
                 sends: check that --url (or BOARD_URL) is the board's own address ({}) and not \
                 a proxy that adds `Origin` or `Cookie` headers",
                self.base
            )),
            _ => error.next,
        };
        let mut failure = Failure::new(error.code, error.message);
        failure.next = next;
        failure.retry_after = retry_after;
        failure
    }

    /// Rewrites the hint of a `not_found` answer to a request that names no
    /// thread. There the only thing that can be missing is the board, and a
    /// board exists only once something has been posted to it.
    fn board_missing(&self, failure: Failure) -> Failure {
        if failure.code != api_codes::NOT_FOUND {
            return failure;
        }
        failure.next(format!(
            "board `{}` does not exist yet: the first `openboard post` to it creates it; if \
             you meant another board, check --board (or BOARD_NAME)",
            self.board
        ))
    }

    /// Rewrites the hint of a failed request that names one message, so it
    /// gives the command to run next with that message's ID in it.
    fn about_message(&self, id: &str, change: Change, failure: Failure) -> Failure {
        // The ID came from the command line. Only one that looks like an ID
        // is repeated in a command the caller may paste.
        let plain = !id.is_empty()
            && id.len() <= 64
            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        let id = if plain { id } else { "<message-id>" };
        let read = format!("`openboard read --thread {id}`");
        let next = match (failure.code.as_str(), change) {
            (api_codes::NOT_FOUND, _) => format!(
                "check the message ID: `openboard read <channel>` lists threads and `openboard \
                 read --thread <id>` shows every message in one with its ID; a message is only \
                 found on its own board (this is `{}`), so check --board (or BOARD_NAME) too",
                self.board
            ),
            (api_codes::FORBIDDEN, Change::Edit | Change::Delete) => format!(
                "you can only {} a message you wrote, under the same login and the same --as \
                 profile: `openboard whoami` shows who you are now and {read} shows the author; \
                 to answer someone else's message use `openboard reply {id} <text>`",
                change.verb()
            ),
            (api_codes::REVISION_CONFLICT, Change::Edit | Change::Delete) => format!(
                "the message was edited or deleted since that revision, and nothing changed: \
                 run {read} to see it and its current revision, then retry with that \
                 --revision, or without --revision to {} it whatever it says now",
                change.verb()
            ),
            (api_codes::LABEL_CONFLICT, _) => format!(
                "an --if or --if-unset condition did not hold, so nothing changed; another \
                 agent may have got there first: run {read} to see the labels as they are now"
            ),
            (api_codes::MESSAGE_DELETED, _) => format!(
                "a deleted message cannot be edited, labelled or reacted to; `openboard reply \
                 {id} <text>` still adds to its thread"
            ),
            _ => return failure,
        };
        failure.next(next)
    }

    fn transport(&self, error: &reqwest::Error) -> Failure {
        // The innermost cause is the useful part, e.g. "Connection refused".
        let mut cause: &dyn std::error::Error = error;
        while let Some(inner) = cause.source() {
            cause = inner;
        }
        let base = &self.base;
        if let Some(refusal) = self.verdict.refusal() {
            return self.certificate_mismatch(&refusal);
        }
        if error.is_connect() && self.is_network() {
            Failure::new(
                codes::CONNECTION_FAILED,
                format!("cannot reach the board at {base}: {cause}"),
            )
            .next(format!(
                "check that the board is running (`openboard serve --network` on its host) and \
                 that this machine can reach {base}; if the board moved to another address, ask \
                 its operator for a new invite"
            ))
        } else if error.is_connect() {
            Failure::new(
                codes::CONNECTION_FAILED,
                format!("cannot reach a board at {base}: {cause}"),
            )
            .next(local_unreachable_hint(self.assumed))
        } else if error.is_timeout() {
            Failure::new(
                codes::REQUEST_TIMEOUT,
                format!("the board at {base} did not answer in time"),
            )
            .next("check that the board is running, then retry")
        } else if error.is_builder() {
            Failure::usage(format!("cannot build the request: {cause}"))
                .next("check --url, --board and --as for stray characters")
        } else {
            Failure::new(
                codes::REQUEST_FAILED,
                format!("the request to {base} failed: {cause}"),
            )
            .next("check that the board is running, then retry")
        }
    }

    /// The peer did not prove it is the pinned board. Nothing was sent to
    /// it: the handshake failed before any request.
    fn certificate_mismatch(&self, refusal: &Refusal) -> Failure {
        let base = &self.base;
        let pinned = self.fingerprint.as_deref().unwrap_or("none");
        let message = match refusal {
            Refusal::WrongCertificate { presented } => format!(
                "the server at {base} presented a certificate that is not the board's: this \
                 client pins {pinned}, the server showed {presented}"
            ),
            Refusal::BadSignature => format!(
                "the server at {base} presented the board's certificate ({pinned}) but could \
                 not prove it holds its private key"
            ),
            Refusal::NoPin => format!(
                "the server at {base} asked for TLS, but this client has no certificate pinned \
                 for it"
            ),
        };
        Failure::new(codes::CERTIFICATE_MISMATCH, message).next(
            "the board's identity changed or the connection is being intercepted; nothing was \
             sent. Do not retry: get a fresh invite from the board's operator (`openboard invite \
             <name>` on the board's host) and run `openboard join <invite>`",
        )
    }

    /// Swaps a single-use enrolment code for a token. Sends no token.
    pub async fn join(&self, code: &str) -> Result<JoinResponse, Failure> {
        let body = serde_json::to_vec(&JoinRequest {
            code: code.to_owned(),
        })
        .map_err(|_| Failure::new(codes::IO, "cannot encode the join request"))?;
        let request = self
            .request(Method::POST, self.root(&["join"]), Some(REQUEST_TIMEOUT))
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        let bytes = self
            .send(request)
            .await?
            .bytes()
            .await
            .map_err(|error| self.transport(&error))?;
        // The body holds the token, so a parse error is never quoted.
        serde_json::from_slice(&bytes).map_err(|_| {
            Failure::new(
                codes::BAD_RESPONSE,
                format!("{} sent a join response this client cannot read", self.base),
            )
            .next(
                "the invite may have been used up: ask the board's operator for a new one, and \
                 check that both sides run the same version of openboard",
            )
        })
    }

    /// The board's health check, which says what it runs. Needs no token,
    /// but sends one when there is one.
    pub async fn health(&self) -> Result<Health, Failure> {
        self.json(self.request(Method::GET, self.root(&["health"]), Some(REQUEST_TIMEOUT)))
            .await
    }

    pub async fn whoami(&self) -> Result<WhoAmI, Failure> {
        self.json(self.request(Method::GET, self.root(&["whoami"]), Some(REQUEST_TIMEOUT)))
            .await
    }

    /// Asks the board to revoke the credential this client uses.
    pub async fn logout(&self) -> Result<(), Failure> {
        self.send(self.request(Method::POST, self.root(&["logout"]), Some(REQUEST_TIMEOUT)))
            .await
            .map(|_| ())
    }

    pub async fn post_message(
        &self,
        request: &PostMessageRequest,
    ) -> Result<PostMessageResponse, Failure> {
        let body = serde_json::to_vec(request)
            .map_err(|error| Failure::new(codes::IO, format!("cannot encode the post: {error}")))?;
        let request = self
            .request(
                Method::POST,
                self.endpoint(&["messages"]),
                Some(REQUEST_TIMEOUT),
            )
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        self.json(request).await
    }

    pub async fn channels(&self) -> Result<ChannelList, Failure> {
        self.json(self.request(
            Method::GET,
            self.endpoint(&["channels"]),
            Some(REQUEST_TIMEOUT),
        ))
        .await
        .map_err(|failure| self.board_missing(failure))
    }

    pub async fn threads(&self, query: &ThreadsQuery) -> Result<ThreadList, Failure> {
        let mut url = self.endpoint(&["threads"]);
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("channel", &query.channel);
            if let Some(limit) = query.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
            if let Some(cursor) = &query.cursor {
                pairs.append_pair("cursor", cursor);
            }
            if let Some(label) = &query.label {
                pairs.append_pair("label", label);
            }
        }
        self.json(self.request(Method::GET, url, Some(REQUEST_TIMEOUT)))
            .await
            .map_err(|failure| self.board_missing(failure))
            .map_err(|failure| match failure.code.as_str() {
                api_codes::INVALID_LABEL => failure.next(
                    "write --label as `key`, `key=value` or `key=a|b` (quote the last form for \
                     the shell): a key is lowercase letters, digits, '.', '_' or '-'",
                ),
                _ => failure,
            })
    }

    /// The boards on this server that the caller may read.
    pub async fn boards(&self) -> Result<BoardList, Failure> {
        self.json(self.request(Method::GET, self.root(&["boards"]), Some(REQUEST_TIMEOUT)))
            .await
    }

    /// A write to one message, which answers with the message as it is now.
    async fn change_message(
        &self,
        id: &str,
        change: Change,
        request: RequestBuilder,
    ) -> Result<Message, Failure> {
        self.json(request)
            .await
            .map_err(|failure| self.about_message(id, change, failure))
    }

    fn with_json<T: serde::Serialize>(
        request: RequestBuilder,
        body: &T,
    ) -> Result<RequestBuilder, Failure> {
        let body = serde_json::to_vec(body).map_err(|error| {
            Failure::new(codes::IO, format!("cannot encode the request: {error}"))
        })?;
        Ok(request.header(CONTENT_TYPE, "application/json").body(body))
    }

    pub async fn edit_message(
        &self,
        id: &str,
        request: &EditMessageRequest,
    ) -> Result<Message, Failure> {
        let url = self.endpoint(&["messages", id]);
        let http = self.request(Method::PATCH, url, Some(REQUEST_TIMEOUT));
        self.change_message(id, Change::Edit, Self::with_json(http, request)?)
            .await
    }

    pub async fn delete_message(
        &self,
        id: &str,
        revision: Option<u32>,
    ) -> Result<Message, Failure> {
        let mut url = self.endpoint(&["messages", id]);
        if let Some(revision) = revision {
            url.query_pairs_mut()
                .append_pair("revision", &revision.to_string());
        }
        let http = self.request(Method::DELETE, url, Some(REQUEST_TIMEOUT));
        self.change_message(id, Change::Delete, http).await
    }

    pub async fn label_message(
        &self,
        id: &str,
        request: &LabelsRequest,
    ) -> Result<Message, Failure> {
        let url = self.endpoint(&["messages", id, "labels"]);
        let http = self.request(Method::POST, url, Some(REQUEST_TIMEOUT));
        self.change_message(id, Change::Label, Self::with_json(http, request)?)
            .await
    }

    /// Adds (`add`) or removes the caller's reaction `name`.
    pub async fn react(&self, id: &str, name: &str, add: bool) -> Result<Message, Failure> {
        let url = self.endpoint(&["messages", id, "reactions", name]);
        let method = if add { Method::PUT } else { Method::DELETE };
        let http = self.request(method, url, Some(REQUEST_TIMEOUT));
        self.change_message(id, Change::React, http).await
    }

    /// The thread that holds `id`, which may be any message ID in it.
    pub async fn thread(&self, id: &str) -> Result<Thread, Failure> {
        self.json(self.request(
            Method::GET,
            self.endpoint(&["threads", id]),
            Some(REQUEST_TIMEOUT),
        ))
        .await
    }

    /// `POST /tasks`.
    pub async fn create_task(
        &self,
        request: &CreateTaskRequest,
    ) -> Result<CreateTaskResponse, Failure> {
        let http = self.request(
            Method::POST,
            self.endpoint(&["tasks"]),
            Some(REQUEST_TIMEOUT),
        );
        self.json(Self::with_json(http, request)?)
            .await
            .map_err(|failure| self.needs(TASKS, failure))
    }

    /// `GET /grants`.
    pub async fn grants(&self) -> Result<openboard_api::grants::GrantList, Failure> {
        let http = self.request(
            Method::GET,
            self.endpoint(&["grants"]),
            Some(REQUEST_TIMEOUT),
        );
        self.json(http)
            .await
            .map_err(|failure| self.needs(GRANTS, failure))
    }

    /// `POST /grants`.
    pub async fn add_grant(
        &self,
        request: &openboard_api::grants::NewGrant,
    ) -> Result<openboard_api::grants::GrantChanged, Failure> {
        let http = self.request(
            Method::POST,
            self.endpoint(&["grants"]),
            Some(REQUEST_TIMEOUT),
        );
        self.json(Self::with_json(http, request)?)
            .await
            .map_err(|failure| self.needs(GRANTS, failure))
    }

    /// `DELETE /grants/{id}`.
    pub async fn remove_grant(
        &self,
        id: &str,
    ) -> Result<openboard_api::grants::GrantChanged, Failure> {
        let http = self.request(
            Method::DELETE,
            self.endpoint(&["grants", id]),
            Some(REQUEST_TIMEOUT),
        );
        self.json(http)
            .await
            .map_err(|failure| self.needs(GRANTS, failure))
    }

    /// `GET /explain`.
    pub async fn explain(
        &self,
        channel: &str,
        for_: Option<&str>,
    ) -> Result<openboard_api::grants::Explained, Failure> {
        let mut url = self.endpoint(&["explain"]);
        url.query_pairs_mut().append_pair("channel", channel);
        if let Some(who) = for_ {
            url.query_pairs_mut().append_pair("for", who);
        }
        let http = self.request(Method::GET, url, Some(REQUEST_TIMEOUT));
        self.json(http)
            .await
            .map_err(|failure| self.needs(GRANTS, failure))
    }

    /// `POST /channels`.
    pub async fn create_channel(
        &self,
        request: &openboard_api::grants::NewChannel,
    ) -> Result<openboard_api::grants::ChannelCreated, Failure> {
        let http = self.request(
            Method::POST,
            self.endpoint(&["channels"]),
            Some(REQUEST_TIMEOUT),
        );
        self.json(Self::with_json(http, request)?)
            .await
            .map_err(|failure| self.needs(GRANTS, failure))
    }

    /// `POST /tasks/batch`.
    pub async fn create_tasks(
        &self,
        request: &CreateTasksRequest,
    ) -> Result<CreateTasksResponse, Failure> {
        let url = self.endpoint(&["tasks", "batch"]);
        let http = self.request(Method::POST, url, Some(REQUEST_TIMEOUT));
        self.json(Self::with_json(http, request)?)
            .await
            .map_err(|failure| self.needs(TASKS, failure))
    }

    /// `GET /tasks`.
    pub async fn tasks(&self, query: &TasksQuery) -> Result<TaskList, Failure> {
        let mut url = self.endpoint(&["tasks"]);
        {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in [
                ("channel", &query.channel),
                ("state", &query.state),
                ("label", &query.label),
                ("for", &query.assignee),
                ("cursor", &query.cursor),
            ] {
                if let Some(value) = value {
                    pairs.append_pair(name, value);
                }
            }
            if let Some(limit) = query.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
        }
        if url.query() == Some("") {
            url.set_query(None);
        }
        self.json(self.request(Method::GET, url, Some(REQUEST_TIMEOUT)))
            .await
            .map_err(|failure| self.needs(TASKS, failure))
    }

    /// `GET /tasks/{id}`.
    pub async fn task(&self, id: &str) -> Result<TaskView, Failure> {
        self.json(self.request(
            Method::GET,
            self.endpoint(&["tasks", id]),
            Some(REQUEST_TIMEOUT),
        ))
        .await
        .map_err(|failure| self.needs(TASKS, failure))
    }

    /// `POST /tasks/claim`, held up to `request.wait` seconds.
    pub async fn claim_ready(
        &self,
        request: &ClaimReadyRequest,
    ) -> Result<ClaimReadyResponse, Failure> {
        let hold = Duration::from_secs(request.wait.unwrap_or(0));
        let url = self.endpoint(&["tasks", "claim"]);
        let http = self.request(Method::POST, url, Some(hold + LONG_POLL_GRACE));
        self.json(Self::with_json(http, request)?)
            .await
            .map_err(|failure| self.needs(TASKS, failure))
    }

    /// `POST /tasks/{id}/{action}`. A claim or a renewal answers with a
    /// [`Claim`], everything else with a [`TaskResponse`].
    pub async fn task_action<T: DeserializeOwned>(
        &self,
        id: &str,
        action: &str,
        request: &TaskActionRequest,
    ) -> Result<T, Failure> {
        let url = self.endpoint(&["tasks", id, action]);
        let http = self.request(Method::POST, url, Some(REQUEST_TIMEOUT));
        self.json(Self::with_json(http, request)?)
            .await
            .map_err(|failure| self.needs(TASKS, failure))
    }

    fn events_url(&self, rest: &[&str], query: &EventsQuery) -> Url {
        let mut url = self.endpoint(rest);
        {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in [
                ("after", &query.after),
                ("channel", &query.channel),
                ("thread", &query.thread),
                ("not_from", &query.not_from),
                ("types", &query.types),
                ("label", &query.label),
            ] {
                if let Some(value) = value {
                    pairs.append_pair(name, value);
                }
            }
            for (name, flag) in [
                ("not_from_self", query.not_from_self),
                ("mentions_self", query.mentions_self),
            ] {
                if let Some(flag) = flag {
                    pairs.append_pair(name, if flag { "true" } else { "false" });
                }
            }
            if let Some(wait) = query.wait {
                pairs.append_pair("wait", &wait.to_string());
            }
            if let Some(limit) = query.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
        }
        if url.query() == Some("") {
            url.set_query(None);
        }
        url
    }

    /// One events request. With `query.wait` set this is a long-poll.
    pub async fn events(&self, query: &EventsQuery) -> Result<EventsResponse, Failure> {
        let timeout = Duration::from_secs(query.wait.unwrap_or(0)) + LONG_POLL_GRACE;
        self.json(self.request(
            Method::GET,
            self.events_url(&["events"], query),
            Some(timeout),
        ))
        .await
        .map_err(|failure| self.events_not_found(query, failure))
    }

    /// With a thread filter, `not_found` most likely means the thread.
    fn events_not_found(&self, query: &EventsQuery, failure: Failure) -> Failure {
        match query.thread {
            Some(_) => failure,
            None => self.board_missing(failure),
        }
    }

    /// Opens the server-sent event stream. The response has no overall
    /// timeout; the caller watches for silence.
    pub async fn event_stream(&self, query: &EventsQuery) -> Result<Response, Failure> {
        let request = self
            .request(
                Method::GET,
                self.events_url(&["events", "stream"], query),
                None,
            )
            .header(ACCEPT, "text/event-stream");
        self.send(request)
            .await
            .map_err(|failure| self.events_not_found(query, failure))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Origin;

    fn target(url: &str, fingerprint: Option<String>, token: Option<&str>) -> Target {
        Target {
            url: url.to_owned(),
            fingerprint,
            token: token.map(|token| Secret::new(token.to_owned())),
            principal: None,
            credential_id: None,
            origin: Origin::Local,
            assumed: false,
        }
    }

    #[tokio::test]
    async fn an_unreachable_assumed_board_points_at_join_as_well_as_serve() {
        tls::install_provider();
        // A port nothing listens on: bound, then closed again.
        let port = {
            let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            taken.local_addr().unwrap().port()
        };
        let url = format!("http://127.0.0.1:{port}");
        let hint_for = |assumed: bool| {
            let target = Target {
                assumed,
                ..target(&url, None, None)
            };
            let client = Client::new(&target, "general", None).unwrap();
            async move {
                let failure = client.channels().await.unwrap_err();
                assert_eq!(failure.code, codes::CONNECTION_FAILED);
                failure.next.unwrap()
            }
        };
        let given = hint_for(false).await;
        assert!(given.starts_with("start a board with `openboard serve`"));
        assert!(given.contains("--url"), "{given}");
        assert!(!given.contains("openboard join"), "{given}");
        // After the last login is gone, or before the first join.
        let assumed = hint_for(true).await;
        assert!(assumed.starts_with("start a board with `openboard serve`"));
        assert!(assumed.contains("`openboard join <invite>`"), "{assumed}");
        assert!(!assumed.contains(". "), "one sentence: {assumed}");
    }

    fn local(url: &str, board: &str) -> Result<Client, Failure> {
        tls::install_provider();
        Client::new(&target(url, None, None), board, None)
    }

    #[test]
    fn endpoints_escape_every_segment() {
        let client = local("http://127.0.0.1:7878/", "Team Board").unwrap();
        assert_eq!(
            client.endpoint(&["threads", "a/b?c"]).as_str(),
            "http://127.0.0.1:7878/v1/boards/Team%20Board/threads/a%2Fb%3Fc"
        );
        assert_eq!(
            client.root(&["whoami"]).as_str(),
            "http://127.0.0.1:7878/v1/whoami"
        );
        let query = EventsQuery {
            after: Some("ev_1".into()),
            channel: Some("research/".into()),
            not_from: Some("local/agent-a".into()),
            not_from_self: Some(true),
            wait: Some(60),
            ..EventsQuery::default()
        };
        assert_eq!(
            client.events_url(&["events"], &query).as_str(),
            "http://127.0.0.1:7878/v1/boards/Team%20Board/events?after=ev_1&channel=research%2F&not_from=local%2Fagent-a&not_from_self=true&wait=60"
        );
        assert_eq!(
            client
                .events_url(&["events", "stream"], &EventsQuery::default())
                .as_str(),
            "http://127.0.0.1:7878/v1/boards/Team%20Board/events/stream"
        );
    }

    #[test]
    fn only_http_and_pinned_https_urls_are_accepted() {
        assert!(local("http://localhost:7878", "general").is_ok());
        for bad in ["https://127.0.0.1:7878", "127.0.0.1:7878", "", "ftp://x"] {
            let failure = local(bad, "general").err().expect(bad);
            assert_eq!(failure.exit, crate::error::EXIT_USAGE, "{bad}");
        }
        let pinned = target(
            "https://127.0.0.1:7878",
            Some(openboard_api::auth::fingerprint(&[3; 32])),
            Some("obt_x"),
        );
        assert!(Client::new(&pinned, "general", None).is_ok());
    }

    fn authorization(client: &Client) -> Option<String> {
        let request = client
            .request(Method::GET, client.root(&["whoami"]), None)
            .build()
            .unwrap();
        request
            .headers()
            .get(AUTHORIZATION)
            .map(|value| value.to_str().unwrap().to_owned())
    }

    #[test]
    fn a_token_goes_to_https_boards_only() {
        tls::install_provider();
        let fingerprint = Some(openboard_api::auth::fingerprint(&[3; 32]));
        let network = Client::new(
            &target("https://127.0.0.1:7878", fingerprint.clone(), Some("obt_x")),
            "general",
            None,
        )
        .unwrap();
        assert_eq!(authorization(&network).as_deref(), Some("Bearer obt_x"));
        // Even a target that wrongly carries a token gets none over HTTP.
        let plain = Client::new(
            &target("http://127.0.0.1:7878", fingerprint, Some("obt_x")),
            "general",
            None,
        )
        .unwrap();
        assert_eq!(authorization(&plain), None);
    }
}
