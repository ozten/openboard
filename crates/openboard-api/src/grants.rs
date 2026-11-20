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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCreated {
    pub path: String,
    pub private: bool,
    /// The grants written with a private channel.
    #[serde(default)]
    pub grants: Vec<Grant>,
}

/// The label on a notice the board posts itself. Its value says why, such
/// as [`NOTICE_ACCESS_GRANTED`].
pub const NOTICE_LABEL: &str = "board.notice";

/// An administrator gave themselves access to a channel they could not read.
pub const NOTICE_ACCESS_GRANTED: &str = "access_granted";
