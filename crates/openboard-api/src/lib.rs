//! Wire types for the Openboard HTTP API, version 1.
//!
//! This crate is the contract between the server (`obd`) and every client.
//! All bodies are JSON. All routes live under `/v1`.
//!
//! | Method | Path | Request | Response |
//! | --- | --- | --- | --- |
//! | GET | `/v1/health` | | [`Health`] |
//! | POST | `/v1/boards/{board}/messages` | [`PostMessageRequest`] | [`PostMessageResponse`] |
//! | GET | `/v1/boards/{board}/channels` | | [`ChannelList`] |
//! | GET | `/v1/boards/{board}/threads` | [`ThreadsQuery`] | [`ThreadList`] |
//! | GET | `/v1/boards/{board}/threads/{thread_id}` | | [`Thread`] |
//! | GET | `/v1/boards/{board}/events` | [`EventsQuery`] | [`EventsResponse`] |
//! | GET | `/v1/boards/{board}/events/stream` | [`EventsQuery`] | server-sent events |
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

/// Request header carrying the caller's declared profile name.
pub const PROFILE_HEADER: &str = "x-board-profile";
/// Request header carrying an idempotency key. Equivalent to
/// [`PostMessageRequest::idempotency_key`]; the body field wins if both are set.
pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";
/// Value of [`EventsQuery::after`] that means "from the first event".
pub const CURSOR_START: &str = "start";

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
    /// The server failed. The message never contains message bodies.
    pub const INTERNAL: &str = "internal";
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
    /// Markdown text.
    pub text: String,
    /// RFC 3339 timestamp in UTC.
    pub created_at: String,
    /// Starts at 1. Reserved for edits, which arrive in a later milestone.
    pub revision: u32,
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
}

/// A full thread: the root and its replies, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub root: Message,
    pub replies: Vec<Message>,
}

/// Event type names used in [`Event::kind`].
pub mod event_types {
    pub const MESSAGE_POSTED: &str = "message.posted";
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
    pub by: Actor,
    /// RFC 3339 timestamp in UTC.
    pub at: String,
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
    /// Long-poll: hold the request up to this many seconds until a matching
    /// event exists. Capped at [`MAX_WAIT_SECS`]. Ignored by the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
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
    pub version: String,
    /// `local` for a no-auth loopback server.
    pub mode: String,
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
