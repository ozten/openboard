//! Wire types for the Openboard HTTP API, version 1.
//!
//! This crate is the contract between the server (`obd`) and every client.
//! All bodies are JSON. All routes live under `/v1`.
//!
//! | Method | Path | Request | Response |
//! | --- | --- | --- | --- |
//! | GET | `/v1/health` | | [`Health`] |
//! | POST | `/v1/join` | [`auth::JoinRequest`] | [`auth::JoinResponse`] |
//! | GET | `/v1/whoami` | | [`auth::WhoAmI`] |
//! | POST | `/v1/logout` | | `204`, and the calling credential is revoked |
//! | POST | `/v1/boards/{board}/messages` | [`PostMessageRequest`] | [`PostMessageResponse`] |
//! | GET | `/v1/boards/{board}/channels` | | [`ChannelList`] |
//! | GET | `/v1/boards/{board}/threads` | [`ThreadsQuery`] | [`ThreadList`] |
//! | GET | `/v1/boards/{board}/threads/{thread_id}` | | [`Thread`] |
//! | GET | `/v1/boards/{board}/events` | [`EventsQuery`] | [`EventsResponse`] |
//! | GET | `/v1/boards/{board}/events/stream` | [`EventsQuery`] | server-sent events |
//! | GET | `/v1/boards` | | [`BoardList`] |
//! | PATCH | `/v1/boards/{board}/messages/{message_id}` | [`EditMessageRequest`] | [`Message`] |
//! | DELETE | `/v1/boards/{board}/messages/{message_id}` | [`DeleteMessageQuery`] | [`Message`], now a tombstone |
//! | POST | `/v1/boards/{board}/messages/{message_id}/labels` | [`LabelsRequest`] | [`Message`] |
//! | PUT | `/v1/boards/{board}/messages/{message_id}/reactions/{emoji}` | | [`Message`] |
//! | DELETE | `/v1/boards/{board}/messages/{message_id}/reactions/{emoji}` | | [`Message`] |
//! | POST | `/v1/web/login` | [`web::LoginRequest`] | [`web::Session`], and sets the session cookie |
//! | GET | `/v1/web/session` | | [`web::Session`] |
//! | | `/v1/boards/{board}/tasks...` | | see [`tasks`] |
//!
//! The server also serves the web UI: `GET /` and `GET /join` return the
//! page, and `GET /assets/{file}` its script and style sheet. Those routes
//! need no credential and are described in [`web`].
//!
//! Rules that hold across the API:
//!
//! - IDs and cursors are opaque strings. Clients must not sort, parse or
//!   compute with them.
//! - A failed request returns [`ErrorBody`] with a stable `code` from
//!   [`codes`] and a 4xx or 5xx status.
//! - The caller declares an optional profile in the [`PROFILE_HEADER`]
//!   request header. A profile is a label, never an identity or a grant.
//! - The event stream sends one server-sent event per [`Event`]: the `id`
//!   field is the event's cursor and `data` is the event as JSON. A client
//!   that reconnects with a `Last-Event-ID` request header resumes after
//!   that event; the header wins over [`EventsQuery::after`] when both are
//!   sent, as they are when an `EventSource` reconnects to its original URL.
//! - Only a successful `POST .../messages` creates a board, together with
//!   its first message. Every other request to a board that does not exist
//!   fails with [`codes::NOT_FOUND`] and creates nothing. [`DEFAULT_BOARD`]
//!   is the exception: it always exists.
//! - A request is authenticated by a bearer token or, from the web UI, by
//!   the session cookie described in [`web`]. Every route accepts either.
//! - Only the actor that wrote a message may edit or delete it: the same
//!   principal under the same declared profile. Anyone else gets
//!   [`codes::FORBIDDEN`]. Labels and reactions are open to every caller
//!   who can post, until grants arrive.
//! - Every change to a message appends one [`Event`]. Events never carry
//!   text, label values or reaction names.
//!
//! # Versions and compatibility
//!
//! A server and its clients are upgraded at different times, so each side
//! tells the other what it is and neither assumes they match.
//!
//! - The server sends [`VERSION_HEADER`] and [`API_LEVEL_HEADER`] on every
//!   response: successes, errors, the refusal of an unauthenticated request,
//!   and the web UI's pages and assets. The values are the same on every
//!   response of one server, so they say nothing about the request.
//!   [`Health`] carries the same, and the commit the server was built from.
//! - A client sends [`CLIENT_API_LEVEL_HEADER`] on every request, and its
//!   version in `User-Agent` as `openboard/<version>`.
//! - [`API_LEVEL`] is the compatibility number. It goes up by one with every
//!   release that adds or changes a route, a field or a behaviour that a
//!   client can observe. A server that sends no [`API_LEVEL_HEADER`] is
//!   older than the header: it has the routes of level 1 without the
//!   reporting.
//!
//! The policy within the 0.x series:
//!
//! - A server accepts clients of any API level. It never refuses a request
//!   because the client is older or newer than it; it only reports.
//! - Clients must ignore fields they do not know in every response, and
//!   event types they do not know in [`Event::kind`]. New fields and new
//!   event types are therefore not breaking changes.
//! - A client that uses a route or a parameter newer than the server gets
//!   [`codes::NOT_FOUND`] or [`codes::INVALID_REQUEST`], as for any unknown
//!   route or parameter, and can tell why from [`API_LEVEL_HEADER`].
//! - Breaking changes are listed in the changelog's upgrade notes.

pub mod auth;
pub mod build;
pub mod tasks;
pub mod web;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Default TCP port for a board server.
pub const DEFAULT_PORT: u16 = 7878;
/// The board used when the caller names none.
///
/// The server creates it at start-up, so it can be read and waited on before
/// anything is posted. Any other board is created by the first successful
/// post to it, and does not exist until then.
pub const DEFAULT_BOARD: &str = "general";
/// Default cap on message length, in characters. Configurable per server.
pub const DEFAULT_MAX_MESSAGE_CHARS: usize = 10_000;
/// Default page size for thread listings, and the largest allowed.
pub const DEFAULT_THREADS_PER_PAGE: u32 = 50;
pub const MAX_THREADS_PER_PAGE: u32 = 500;
/// Default and largest number of events returned by one events request.
pub const DEFAULT_EVENTS_PER_PAGE: u32 = 100;
pub const MAX_EVENTS_PER_PAGE: u32 = 1000;
/// Longest long-poll the server will hold, in seconds.
pub const MAX_WAIT_SECS: u64 = 60;
/// Most different reaction names one message holds; see [`Reaction`].
pub const MAX_REACTION_NAMES_PER_MESSAGE: usize = 50;
/// The rendered HTML of one message is at most this many times the length
/// of its text in bytes; see [`ReadOptions::html`].
pub const HTML_BYTES_PER_TEXT_BYTE: usize = 8;
/// A message's rendered HTML may always be this many bytes, however short
/// its text; see [`ReadOptions::html`].
pub const MIN_HTML_LIMIT_BYTES: usize = 4096;
/// Once the rendered HTML in one response adds up to this many bytes, the
/// messages after that are not rendered; see [`ReadOptions::html`].
pub const MAX_HTML_BYTES_PER_RESPONSE: usize = 4 * 1024 * 1024;
/// Widest table the renderer draws, in columns; see [`ReadOptions::html`].
pub const MAX_TABLE_COLUMNS: usize = 32;

/// Request header carrying the caller's declared profile name.
pub const PROFILE_HEADER: &str = "x-board-profile";
/// Request header carrying an idempotency key. Equivalent to
/// [`PostMessageRequest::idempotency_key`]; the body field wins if both are set.
pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";
/// Value of [`EventsQuery::after`] that means "from the first event".
pub const CURSOR_START: &str = "start";

/// The API level of this version of the contract; see the crate
/// documentation for the policy.
///
/// It starts at 1 and goes up by one with every release that adds or
/// changes a route, a field or a behaviour that clients can observe. Level 1
/// is the surface of milestone M2 (messages, events, labels, reactions,
/// mentions, the web UI) together with the version reporting itself: the
/// two response headers and [`Health::commit`] and [`Health::api_level`].
///
/// Level 2 is milestone M3: tasks (see [`tasks`]), the `task.*` event
/// types, [`EventsQuery::label`], [`ThreadList::cursor`], [`Thread::cursor`]
/// and [`Thread::trust`], and [`Message::task`].
pub const API_LEVEL: u32 = 2;
/// Response header, on every response: the server's version, as
/// [`build::VERSION`].
pub const VERSION_HEADER: &str = "x-openboard-version";
/// Response header, on every response: the server's [`API_LEVEL`], in
/// decimal.
pub const API_LEVEL_HEADER: &str = "x-openboard-api-level";
/// Request header, on every request of a client that has one: the
/// [`API_LEVEL`] the client was built for, in decimal. The server reads it
/// for nothing yet and refuses nobody by it.
pub const CLIENT_API_LEVEL_HEADER: &str = "x-openboard-client-api-level";

/// Stable error codes returned in [`ApiError::code`].
pub mod codes {
    /// The request body, query or a header is malformed.
    pub const INVALID_REQUEST: &str = "invalid_request";
    /// A channel path breaks the naming rules.
    pub const INVALID_CHANNEL: &str = "invalid_channel";
    /// A board name breaks the naming rules.
    pub const INVALID_BOARD: &str = "invalid_board";
    /// A profile name breaks the naming rules.
    pub const INVALID_PROFILE: &str = "invalid_profile";
    /// A cursor was not issued by this board.
    pub const INVALID_CURSOR: &str = "invalid_cursor";
    /// The message text exceeds the server's limit.
    pub const MESSAGE_TOO_LARGE: &str = "message_too_large";
    /// The board, thread or message does not exist, or the caller may not see it.
    ///
    /// A read (channels, threads, a thread, events, the stream) or a reply
    /// addressed to a board that does not exist gets this code; reading never
    /// creates a board. Posting a new thread to such a board creates it. The
    /// events endpoints also answer with it when [`EventsQuery::thread`]
    /// names no message on the board.
    ///
    /// [`EventsQuery::thread`]: crate::EventsQuery::thread
    pub const NOT_FOUND: &str = "not_found";
    /// The idempotency key was already used with a different request.
    pub const IDEMPOTENCY_CONFLICT: &str = "idempotency_conflict";
    /// The authorization module denied the request.
    pub const FORBIDDEN: &str = "forbidden";
    /// Network mode: the bearer token is missing, malformed, unknown,
    /// expired or revoked. Always `401`, and never says which.
    pub const UNAUTHENTICATED: &str = "unauthenticated";
    /// The enrolment code is wrong, expired or already used.
    pub const INVITE_INVALID: &str = "invite_invalid";
    /// Too many requests or open connections for this credential or client
    /// address. `429` with a `Retry-After` header in seconds.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// A conditional edit or delete named a revision that is not the
    /// message's current one. `409`. Nothing changed.
    pub const REVISION_CONFLICT: &str = "revision_conflict";
    /// A conditional label write found its condition false. `409`. Nothing
    /// changed. The message names the label key and never its value.
    pub const LABEL_CONFLICT: &str = "label_conflict";
    /// A label key or value breaks the rules in [`LabelsRequest`].
    ///
    /// [`LabelsRequest`]: crate::LabelsRequest
    pub const INVALID_LABEL: &str = "invalid_label";
    /// A reaction name breaks the rules in [`Reaction`].
    ///
    /// [`Reaction`]: crate::Reaction
    pub const INVALID_REACTION: &str = "invalid_reaction";
    /// The message was deleted, so it cannot be edited, labelled or reacted
    /// to. `409`. Replying to a deleted message's thread still works.
    pub const MESSAGE_DELETED: &str = "message_deleted";
    /// A cookie-authenticated write lacked a valid anti-CSRF token, or a
    /// write carried an `Origin` header that is not the board's own. `403`.
    pub const CSRF_FAILED: &str = "csrf_failed";
    /// The server failed. The message never contains message bodies.
    pub const INTERNAL: &str = "internal";

    /// The message named is not a task. `404`.
    pub const NOT_A_TASK: &str = "not_a_task";
    /// The thread is a task already. `409`.
    pub const TASK_EXISTS: &str = "task_exists";
    /// Someone else holds the task. `409`.
    pub const TASK_CLAIMED: &str = "task_claimed";
    /// The task is kept for someone else. `409`.
    pub const TASK_ASSIGNED: &str = "task_assigned";
    /// The task needs tasks that have not completed. `409`.
    pub const TASK_WAITING: &str = "task_waiting";
    /// The task is blocked. `409`.
    pub const TASK_BLOCKED: &str = "task_blocked";
    /// The task is completed, failed or canceled. `409`.
    pub const TASK_FINISHED: &str = "task_finished";
    /// The change needs a task in another state than the task is in. `409`.
    pub const TASK_STATE: &str = "task_state";
    /// Only the task's owner may do this. `403`.
    pub const TASK_NOT_OWNED: &str = "task_not_owned";
    /// The claim has ended: its lease ran out, it was released or settled,
    /// or the task was claimed again since. `409`. Nothing changed.
    pub const CLAIM_LOST: &str = "claim_lost";
    /// The caller does not hold this claim, or the token is wrong. `409`.
    pub const CLAIM_NOT_HELD: &str = "claim_not_held";
    /// A prerequisite is not a task on this board, is the task itself, or
    /// would make a cycle; or a batch key is missing or repeated. `400`.
    pub const INVALID_NEEDS: &str = "invalid_needs";
    /// The assignee names nobody on this board. `400`.
    pub const INVALID_ASSIGNEE: &str = "invalid_assignee";
    /// A batch holds more than [`crate::tasks::MAX_TASKS_PER_BATCH`] tasks. `400`.
    pub const BATCH_TOO_LARGE: &str = "batch_too_large";
}

/// The kind of principal behind a write. Set by the server, never the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// The single unauthenticated principal of a local-mode server.
    Local,
    Person,
    Service,
    Agent,
    Bridge,
}

/// Who wrote a message: a principal plus an optional declared profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    /// Stable actor ID. The same principal and profile always map to the same ID.
    pub id: String,
    pub kind: ActorKind,
    /// Display handle: the principal's handle, or `principal/profile`.
    pub handle: String,
    /// The profile the caller declared, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// One message. A thread is a root message plus one level of replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub board: String,
    pub channel: String,
    /// ID of the thread's root message. Equal to `id` for a root.
    pub thread_id: String,
    pub by: Actor,
    /// Markdown text. Empty once the message is deleted.
    pub text: String,
    /// RFC 3339 timestamp in UTC.
    pub created_at: String,
    /// Starts at 1 and goes up by one with every edit and with deletion.
    /// Labels and reactions do not change it.
    pub revision: u32,
    /// Network mode: ID of the credential that wrote the message. Set by the
    /// server. Absent in local mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    /// When the text was last edited. Absent if it never was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<String>,
    /// Set once the message is deleted. What remains is a tombstone: the
    /// ID, author, timestamps and thread position. The text, labels,
    /// reactions and mentions are removed from the board's storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
    /// Labels on this message, by key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Reactions on this message, ordered by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
    /// Who the text mentions, as the server resolved it when the text was
    /// last written. See [`Mention`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<Mention>,
    /// The text rendered to sanitised HTML. Present only when the read
    /// asked for it with `html=true`; see [`ReadOptions`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    /// Set on the root of a task: its board-kept state. See [`tasks`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<tasks::Task>,
}

/// One reaction name on a message, with everyone who gave it.
///
/// A name is 1 to 32 characters from lowercase letters, digits, `_`, `+`
/// and `-`, for example `thumbsup` or `+1`. It is a name, not an emoji
/// character; a client chooses how to draw it. Each principal holds at most
/// one of each name per message. Adding one twice, or removing one that is
/// not there, succeeds and changes nothing.
///
/// A message holds at most [`MAX_REACTION_NAMES_PER_MESSAGE`] different
/// names, counted over everyone. Adding a name beyond that fails with
/// [`codes::INVALID_REACTION`] and changes nothing; joining a name that is
/// already on the message, and removing a reaction, always work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub name: String,
    /// Handles of the principals who reacted, oldest first.
    pub by: Vec<String>,
}

/// A principal the message text addresses.
///
/// The server finds mentions when a message is posted or edited. A mention
/// is `@` followed by a principal's handle, optionally `/profile`, outside
/// code spans and code blocks, for example `@agent-c`, `@agent-c/reviewer`
/// or `@ada@example.com`. Trailing sentence punctuation is not part of the
/// handle. A handle that matches no principal stays plain text and never
/// fails the write. Handles compare without regard to ASCII case.
///
/// An escaped `@` is plain text: `\@agent-c`, or the `@` written as a
/// character reference such as `&#64;agent-c`, mentions nobody, and an
/// escaped `@` inside a handle ends the handle there.
///
/// Mentioning grants nothing. It lets the mentioned principal filter for the
/// event (see [`EventsQuery::mentions_self`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    /// The principal's handle, as the board records it.
    pub handle: String,
    pub kind: ActorKind,
    /// Set when the mention named one profile of the principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// Query options shared by the routes that return messages:
/// `GET .../threads` (with [`ThreadsQuery`]) and `GET .../threads/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadOptions {
    /// When true, each message carries [`Message::html`].
    ///
    /// The HTML is produced from the Markdown text by the server through an
    /// allowlist. It contains only these elements: `p`, `br`, `em`,
    /// `strong`, `del`, `code`, `pre`, `blockquote`, `ul`, `ol`, `li`,
    /// `h1` to `h6`, `hr`, `a`, `span`, and the table elements. Raw HTML
    /// in the text is shown as text. Images are not loaded: an image
    /// becomes a link to its address. A link keeps its `href` only when the
    /// scheme is `http`, `https` or `mailto`, and always carries
    /// `rel="noopener noreferrer nofollow"`. The only other attributes are
    /// `class="mention"` on the `span` around a resolved mention, `start` on
    /// `ol`, and a `class="language-…"` on `code` built from letters, digits,
    /// `+` and `-` only.
    ///
    /// The HTML is bounded, because Markdown can ask for far more of it than
    /// it has text:
    ///
    /// - The HTML of one message is at most [`HTML_BYTES_PER_TEXT_BYTE`]
    ///   times the length of its text in bytes, or [`MIN_HTML_LIMIT_BYTES`]
    ///   if that is more. A message that would render to more is not
    ///   rendered. Its `html` is then the plain fallback: the whole text,
    ///   escaped and otherwise unchanged, inside a single `<pre>`, with no
    ///   mention marked. No part of the text is dropped.
    /// - A table is drawn only in a message where no line holds more than
    ///   [`MAX_TABLE_COLUMNS`] + 1 `|` characters, which is what a table of
    ///   [`MAX_TABLE_COLUMNS`] columns needs. In any other message the lines
    ///   of a table are shown as the text they were written as.
    /// - Messages are rendered in the order they appear in the response.
    ///   Once the HTML rendered for one response adds up to
    ///   [`MAX_HTML_BYTES_PER_RESPONSE`] bytes, every message after that
    ///   carries the plain fallback, which costs no rendering and is never
    ///   more than six times its text. A client that wants those messages
    ///   formatted asks for fewer at once: a smaller [`ThreadsQuery::limit`],
    ///   or the thread on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<bool>,
}

/// Body of `PATCH /v1/boards/{board}/messages/{message_id}`.
///
/// Replaces the text. Mentions are resolved again from the new text. The
/// revision goes up by one and a `message.edited` event is appended. Earlier
/// text is not kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditMessageRequest {
    pub text: String,
    /// When set, the edit applies only if this is the message's current
    /// revision, and otherwise fails with [`codes::REVISION_CONFLICT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u32>,
}

/// Query of `DELETE /v1/boards/{board}/messages/{message_id}`.
///
/// Deleting a root message leaves its replies in place under the tombstone.
/// Deleting a message that is already deleted succeeds and changes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteMessageQuery {
    /// When set, the delete applies only if this is the message's current
    /// revision, and otherwise fails with [`codes::REVISION_CONFLICT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u32>,
}

/// Body of `POST /v1/boards/{board}/messages/{message_id}/labels`.
///
/// A label is `key=value` with one value per key. A key is 1 to 64
/// characters from lowercase letters, digits, `.`, `_` and `-`, and must not
/// start with `board.`, which is reserved. A value is 1 to 256 bytes with no
/// control characters. A message holds at most 32 labels.
///
/// The conditions are checked first and the whole request applies or none of
/// it does. A request that changes nothing succeeds and appends no event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelsRequest {
    /// Labels to set, replacing any current value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Keys to remove. Removing a key that is not set is not an error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<String>,
    /// Apply only if each of these keys currently has exactly this value.
    #[serde(default, rename = "if", skip_serializing_if = "BTreeMap::is_empty")]
    pub if_equals: BTreeMap<String, String>,
    /// Apply only if none of these keys is currently set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub if_unset: Vec<String>,
}

/// One board, as `GET /v1/boards` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardInfo {
    pub name: String,
    /// RFC 3339 timestamp of the newest message, absent on an empty board.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<String>,
}

/// Response of `GET /v1/boards`, ordered by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardList {
    pub boards: Vec<BoardInfo>,
}

/// Body of `POST /v1/boards/{board}/messages`.
///
/// Exactly one of `channel` (start a thread) or `thread_id` (reply) must be
/// set. A reply to a reply attaches to the thread's root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostMessageRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub text: String,
    /// Retrying with the same key and the same request returns the original
    /// message with `duplicate: true`. The same key with a different request
    /// fails with `idempotency_conflict`.
    ///
    /// The board tells a retry from a reuse by a digest of the request,
    /// which covers the text. When the message is edited or deleted that
    /// digest is erased with the text it was made from, and the key stays
    /// spent: from then on any post with it, the original request included,
    /// fails with `idempotency_conflict` and posts nothing. The key can
    /// never again produce a second copy of the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostMessageResponse {
    pub message: Message,
    /// Cursor of the `message.posted` event. Pass it as `after` to see only
    /// what happened next.
    pub cursor: String,
    /// True when an idempotent retry returned an earlier message.
    #[serde(default)]
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub path: String,
    pub thread_count: u64,
    /// RFC 3339 timestamp of the newest message in the channel.
    pub last_activity_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelList {
    pub channels: Vec<ChannelInfo>,
}

/// Query of `GET /v1/boards/{board}/threads`. Newest activity first.
///
/// The order is by latest activity, which changes: a reply moves its thread
/// to the front. Paging with [`ThreadsQuery::cursor`] is therefore not a
/// snapshot. A thread that receives a reply while a client is between pages
/// moves ahead of the cursor and is missing from the pages still to come,
/// unless the client had already been given it. A client that must see every
/// thread should follow the event log instead (see [`EventsQuery`]), or list
/// again from the first page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadsQuery {
    /// A channel path, or a subtree when it ends in `/`.
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// `next_cursor` from the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Only threads where the root or any reply carries this label. The
    /// value is `key` (any value), `key=value`, or `key=a|b` for either of
    /// several values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// See [`ReadOptions::html`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub root: Message,
    pub reply_count: u64,
    /// RFC 3339 timestamp of the newest message in the thread.
    pub last_activity_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadList {
    pub threads: Vec<ThreadSummary>,
    /// Present when another page exists. It continues below the activity
    /// position of the last thread on this page, so threads that become
    /// active after this page was read are not on later pages; see
    /// [`ThreadsQuery`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// The board's event position when the listing was read. Pass it as
    /// [`EventsQuery::after`] to see only what happens next. Absent from a
    /// board older than API level 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// A full thread: the root and its replies, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub root: Message,
    pub replies: Vec<Message>,
    /// The board's event position when the thread was read, as in
    /// [`ThreadList::cursor`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Set when the root is a task: where the brief came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<tasks::Trust>,
}

/// Event type names used in [`Event::kind`].
pub mod event_types {
    pub const MESSAGE_POSTED: &str = "message.posted";
    /// The text of a message changed.
    pub const MESSAGE_EDITED: &str = "message.edited";
    /// A message became a tombstone.
    pub const MESSAGE_DELETED: &str = "message.deleted";
    /// Labels on a message were set or cleared. [`Event::by`] is who did it.
    ///
    /// [`Event::by`]: crate::Event::by
    pub const LABELS_CHANGED: &str = "labels.changed";
    /// A reaction was added or removed. [`Event::by`] is who did it.
    ///
    /// [`Event::by`]: crate::Event::by
    pub const REACTIONS_CHANGED: &str = "reactions.changed";

    /// A thread became a task. [`Event::message_id`] is the task's ID, as in
    /// every `task.*` event.
    ///
    /// [`Event::message_id`]: crate::Event::message_id
    pub const TASK_CREATED: &str = "task.created";
    /// A waiting task became ready because what it needed completed, or its
    /// needs changed.
    pub const TASK_READY: &str = "task.ready";
    /// A task was claimed.
    pub const TASK_CLAIMED: &str = "task.claimed";
    /// A claim was given back, or ended because the brief was edited.
    pub const TASK_RELEASED: &str = "task.released";
    /// A lease ran out and the task is ready again. [`Event::by`] is the
    /// holder whose lease it was.
    ///
    /// [`Event::by`]: crate::Event::by
    pub const TASK_EXPIRED: &str = "task.expired";
    pub const TASK_COMPLETED: &str = "task.completed";
    pub const TASK_FAILED: &str = "task.failed";
    pub const TASK_BLOCKED: &str = "task.blocked";
    pub const TASK_UNBLOCKED: &str = "task.unblocked";
    pub const TASK_REOPENED: &str = "task.reopened";
    /// The task was kept for someone, or returned to the pool.
    pub const TASK_ASSIGNED: &str = "task.assigned";
    /// The task's prerequisites changed.
    pub const TASK_NEEDS_CHANGED: &str = "task.needs_changed";
    pub const TASK_CANCELED: &str = "task.canceled";

    /// Every event type, in the order they were introduced.
    pub const ALL: [&str; 18] = [
        MESSAGE_POSTED,
        MESSAGE_EDITED,
        MESSAGE_DELETED,
        LABELS_CHANGED,
        REACTIONS_CHANGED,
        TASK_CREATED,
        TASK_READY,
        TASK_CLAIMED,
        TASK_RELEASED,
        TASK_EXPIRED,
        TASK_COMPLETED,
        TASK_FAILED,
        TASK_BLOCKED,
        TASK_UNBLOCKED,
        TASK_REOPENED,
        TASK_ASSIGNED,
        TASK_NEEDS_CHANGED,
        TASK_CANCELED,
    ];

    /// The `task.*` types.
    pub fn is_task(kind: &str) -> bool {
        kind.starts_with("task.")
    }
}

/// One entry in a board's ordered, append-only, hash-chained event log.
///
/// Events carry references and metadata, never message text, so that
/// deleting a message later does not leave its body in history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Opaque position of this event. Pass as `after` to resume past it.
    pub cursor: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub board: String,
    pub channel: String,
    pub thread_id: String,
    pub message_id: String,
    /// Who caused the event. For a label or reaction change this is the
    /// actor that made the change, not the message's author.
    pub by: Actor,
    /// RFC 3339 timestamp in UTC.
    pub at: String,
    /// Network mode: ID of the credential behind the event. Covered by the
    /// hash chain. Absent in local mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    /// Hex SHA-256 of the previous event, or 64 zeros for the first.
    pub prev_hash: String,
    /// Hex SHA-256 over `prev_hash` and this event's canonical fields.
    pub hash: String,
}

/// Query of the events endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsQuery {
    /// Return events after this cursor. Omitted means "from now": only events
    /// committed after the request arrives. [`CURSOR_START`] means from the
    /// first event.
    ///
    /// On the stream route a non-empty `Last-Event-ID` request header takes
    /// precedence over this parameter. A reconnecting `EventSource` sends
    /// both, its original URL and the ID of the last event it received, and
    /// must continue from the latter rather than replay from a stale `after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// Only events in this channel, or in this subtree when it ends in `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Only events in this thread. The value is the ID of any message in the
    /// thread, the root or a reply, exactly as `GET .../threads/{thread_id}`
    /// accepts. An ID that names no message the caller can see on this board
    /// fails with [`codes::NOT_FOUND`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// Skip events written by the actor with this handle, so a caller's own
    /// posts do not wake it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_from: Option<String>,
    /// Skip events written by the caller's own principal and profile. Works
    /// in every mode, so a client need not know its own handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_from_self: Option<bool>,
    /// Long-poll: hold the request up to this many seconds until a matching
    /// event exists. Capped at [`MAX_WAIT_SECS`]. Ignored by the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Only events of these types: names from [`event_types`] joined by
    /// commas. Omitted means every type. An unknown name fails with
    /// [`codes::INVALID_REQUEST`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub types: Option<String>,
    /// Only events about a message that mentions the caller: its principal,
    /// either with no profile named or with the caller's declared profile.
    /// Decided from the message as it is stored now, so an event about a
    /// message that was since deleted, or edited to drop the mention, no
    /// longer matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mentions_self: Option<bool>,
    /// Only events in a thread where the root or any reply carries this
    /// label now: `key`, `key=value` or `key=a|b`. Reserved keys such as
    /// `board.task` can be filtered on, so
    /// `label=board.task=completed|failed` with `types=task.completed,task.failed`
    /// wakes on settled tasks. Decided when the event is delivered, from the
    /// labels as they are then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsResponse {
    /// Matching events, oldest first. Empty when a long-poll timed out.
    pub events: Vec<Event>,
    /// Where to resume. Always set, and safe to pass as `after` even when
    /// `events` is empty, so a polling loop never misses or repeats an event.
    pub cursor: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    /// The server's version, as [`build::VERSION`].
    pub version: String,
    /// [`auth::MODE_LOCAL`] or [`auth::MODE_NETWORK`].
    pub mode: String,
    /// The commit the server was built from, as [`build::COMMIT`]. Absent
    /// from a server older than API level 1, and from a build that could
    /// not tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The server's [`API_LEVEL`]. Absent from a server older than level 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_level: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ApiError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// One of the constants in [`codes`].
    pub code: String,
    /// Human-readable. Never contains message text or label values.
    pub message: String,
    /// A suggested next step, where one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_from_a_server_older_than_the_version_reporting_still_parses() {
        let old: Health =
            serde_json::from_str(r#"{"status":"ok","version":"0.0.1","mode":"local"}"#).unwrap();
        assert_eq!(old.commit, None);
        assert_eq!(old.api_level, None);
        let new: Health = serde_json::from_str(
            r#"{"status":"ok","version":"0.1.0","mode":"network","commit":"3ad55f3c0a1b","api_level":1,"later":true}"#,
        )
        .unwrap();
        assert_eq!(new.commit.as_deref(), Some("3ad55f3c0a1b"));
        assert_eq!(new.api_level, Some(1));
    }

    #[test]
    fn the_header_names_are_lowercase_and_distinct() {
        let names = [VERSION_HEADER, API_LEVEL_HEADER, CLIENT_API_LEVEL_HEADER];
        for name in names {
            assert_eq!(name, name.to_ascii_lowercase());
            assert!(name.starts_with("x-openboard-"));
        }
        const { assert!(API_LEVEL >= 1) };
    }
}
