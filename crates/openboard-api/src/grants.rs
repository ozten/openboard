//! Wire types of grants, `explain` and channel creation (milestone M4).
//! `docs/permissions.md` describes the rules.

use serde::{Deserialize, Serialize};

/// `GET` lists, `POST` adds.
pub fn grants_path(board: &str) -> String {
    format!("/v1/boards/{board}/grants")
}

/// `DELETE` removes one grant.
pub fn grant_path(board: &str, id: &str) -> String {
    format!("/v1/boards/{board}/grants/{id}")
}

/// `GET ?channel=<path>[&for=<handle>]`.
pub fn explain_path(board: &str) -> String {
    format!("/v1/boards/{board}/explain")
}

/// `POST` creates a channel.
pub fn channels_path(board: &str) -> String {
    format!("/v1/boards/{board}/channels")
}

/// `GET` lists the caller's limited profiles on the board.
pub fn profiles_path(board: &str) -> String {
    format!("/v1/boards/{board}/profiles")
}

/// `PUT` limits one profile, `DELETE` lifts the limit.
pub fn profile_path(board: &str, profile: &str) -> String {
    format!("/v1/boards/{board}/profiles/{profile}")
}

/// A profile's limit on one board (A8): requests made as the profile get
/// the principal's access intersected with these entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLimit {
    pub profile: String,
    /// Entries such as `build/=read,post`.
    pub can: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLimits {
    pub profiles: Vec<ProfileLimit>,
}

/// The body of `PUT /v1/boards/{board}/profiles/{profile}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetProfileLimit {
    pub can: Vec<String>,
}

/// One grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub id: String,
    /// `*`, `build/` or `build/api`.
    pub pattern: String,
    /// The handle of a principal, a group (`agents`, `people`, `services`)
    /// or `everyone`.
    pub grantee: String,
    /// Verbs, comma-separated, as `none`, `all` or a list.
    pub verbs: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// It expired, and no longer applies.
    #[serde(default)]
    pub expired: bool,
    /// The handle that made it, or `operator`.
    pub created_by: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantList {
    /// The grants the caller may manage: those whose pattern lies within a
    /// grant of its own that includes `grant.manage`.
    pub grants: Vec<Grant>,
}

/// The body of `POST /v1/boards/{board}/grants`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewGrant {
    pub pattern: String,
    /// A handle, `agents`, `people`, `services` or `everyone`.
    pub grantee: String,
    pub verbs: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
}

/// What adding or removing a grant did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantChanged {
    pub grant: Grant,
    /// The caller's grant that authorized the change.
    pub authorized_by: String,
    /// Notices posted where an administrator's grant to themselves opened a
    /// channel they could not read before (Z9): one message ID each.
    #[serde(default)]
    pub notices: Vec<String>,
}

/// What a principal may do in one channel, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Explained {
    pub channel: String,
    /// The handle asked about: the caller's own unless `for` was given.
    pub principal: String,
    pub verbs: String,
    /// The grants that decide. Empty means no grant applies: no access.
    pub deciding: Vec<Grant>,
    /// The caller's profile or token removed some of what the grants give.
    /// Only for the caller's own access.
    #[serde(default)]
    pub narrowed: bool,
}

/// The body of `POST /v1/boards/{board}/channels`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewChannel {
    pub path: String,
    /// Close it to everyone but its creator, who gets `grant.manage` to admit
    /// others (Z7).
    #[serde(default)]
    pub private: bool,
    /// An intake channel (S4), where tasks cannot be created or claimed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub intake: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCreated {
    pub path: String,
    pub private: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub intake: bool,
    /// The grants written with a private channel.
    #[serde(default)]
    pub grants: Vec<Grant>,
}

/// `POST` sets a channel's mode: work or intake (S4). It takes
/// `grant.manage` in the channel.
pub fn channel_mode_path(board: &str) -> String {
    format!("/v1/boards/{board}/channels/mode")
}

/// The body of `POST /v1/boards/{board}/channels/mode`, and its answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMode {
    pub path: String,
    pub intake: bool,
}

/// The label a task promoted from an intake request carries: the request's
/// message ID (S4).
pub const ORIGIN_LABEL: &str = "board.origin";

/// The label on a notice the board posts itself. Its value says why, such
/// as [`NOTICE_ACCESS_GRANTED`].
pub const NOTICE_LABEL: &str = "board.notice";

/// An administrator gave themselves access to a channel they could not read.
pub const NOTICE_ACCESS_GRANTED: &str = "access_granted";
