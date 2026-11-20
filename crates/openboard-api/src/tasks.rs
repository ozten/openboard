//! Wire types for tasks.
//!
//! A task is a thread whose root message carries board-kept state: see
//! [`Task`]. The state is also the label [`TASK_LABEL`] on the root, so every
//! label filter (`read --label board.task=ready`, `wait --label ...`) works
//! on it. Labels under `board.` are reserved: only the board writes them.
//!
//! # States
//!
//! - `waiting`: a task it needs has not completed yet.
//! - `ready`: it can be claimed.
//! - `claimed`: a worker holds it until its lease ends.
//! - `blocked`: it is held for a decision, with a reason and a kind.
//! - `completed`: done, with an optional result.
//! - `failed`: a worker gave up on it, with a reason. Reopening makes it
//!   ready again.
//! - `canceled`: its creator called it off, with a reason.
//!
//! # Routes
//!
//! All under `/v1/boards/{board}`:
//!
//! | Method and path | Body | Answer |
//! | --- | --- | --- |
//! | `POST /tasks` | [`CreateTaskRequest`] | [`CreateTaskResponse`] |
//! | `POST /tasks/batch` | [`CreateTasksRequest`] | [`CreateTasksResponse`] |
//! | `GET /tasks` | query [`TasksQuery`] | [`TaskList`] |
//! | `GET /tasks/{id}` | | [`TaskView`] |
//! | `POST /tasks/claim` | [`ClaimReadyRequest`] | [`ClaimReadyResponse`] |
//! | `POST /tasks/{id}/claim` | [`TaskActionRequest`] (`lease_secs`) | [`Claim`] |
//! | `POST /tasks/{id}/renew` | `epoch`, `token`, `lease_secs` | [`Claim`] |
//! | `POST /tasks/{id}/release` | `epoch`, `token` | [`TaskResponse`] |
//! | `POST /tasks/{id}/complete` | `epoch`, `token`, `result` | [`TaskResponse`] |
//! | `POST /tasks/{id}/fail` | `epoch`, `token`, `reason` | [`TaskResponse`] |
//! | `POST /tasks/{id}/block` | `reason`, `kind`, and the claim if held | [`TaskResponse`] |
//! | `POST /tasks/{id}/unblock` | | [`TaskResponse`] |
//! | `POST /tasks/{id}/reopen` | | [`TaskResponse`] |
//! | `POST /tasks/{id}/assign` | `assignee` or `pool` | [`TaskResponse`] |
//! | `POST /tasks/{id}/needs` | `needs` | [`TaskResponse`] |
//! | `POST /tasks/{id}/cancel` | `reason` | [`TaskResponse`] |
//!
//! # Claims and fencing
//!
//! Every claim increments the task's `epoch` and gets a fresh random
//! `token`. Renewing, releasing, completing, failing and blocking a claimed
//! task need both. A call whose claim has ended, because its lease ran out,
//! it was released or settled, or the task was claimed again since, fails
//! with [`codes::CLAIM_LOST`] and changes nothing. A call from someone who
//! is not the holder, or with the wrong token, fails with
//! [`codes::CLAIM_NOT_HELD`]. There are never two valid claims on one task.
//!
//! # Creators
//!
//! The actor that creates a task is its creator, which never changes: the principal under the profile
//! it declared. Calling without a profile, a principal counts as the creator of the tasks any of
//! its profiles created. Only the creator, or a holder of the task-manage
//! permission, can block an unclaimed task, unblock, reopen, assign, change
//! `needs` and cancel; others get [`codes::NOT_TASK_CREATOR`].
//!
//! A ready claim takes only tasks that are assigned to the caller, or that
//! are unassigned and created under the caller's principal, unless the caller
//! names other creators in [`ClaimReadyRequest::from`]. A claim by ID takes
//! any ready task that is not assigned to someone else.
//!
//! [`codes::CLAIM_LOST`]: crate::codes::CLAIM_LOST
//! [`codes::CLAIM_NOT_HELD`]: crate::codes::CLAIM_NOT_HELD
//! [`codes::NOT_TASK_CREATOR`]: crate::codes::NOT_TASK_CREATOR

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Actor, Message, ThreadSummary};

/// The reserved label on a task's root that holds its state.
pub const TASK_LABEL: &str = "board.task";
/// Lease given when a claim asks for none, in seconds.
pub const DEFAULT_LEASE_SECS: u64 = 300;
/// Longest lease, in seconds.
pub const MAX_LEASE_SECS: u64 = 300;
/// Shortest lease, in seconds.
pub const MIN_LEASE_SECS: u64 = 5;
/// Most tasks one batch creates.
pub const MAX_TASKS_PER_BATCH: usize = 15;
/// Most prerequisites one task has.
pub const MAX_NEEDS_PER_TASK: usize = 32;
/// Most creators a ready claim names in [`ClaimReadyRequest::from`].
pub const MAX_CLAIM_FROM: usize = 32;
/// Most label conditions on a ready claim.
pub const MAX_CLAIM_LABELS: usize = 8;
/// Longest block kind, in bytes.
pub const MAX_BLOCK_KIND_LEN: usize = 64;
/// Default and largest page of a task listing.
pub const DEFAULT_TASKS_PER_PAGE: u32 = 50;
pub const MAX_TASKS_PER_PAGE: u32 = 500;

/// Task state names, as [`Task::state`] and the [`TASK_LABEL`] label hold them.
pub mod states {
    pub const WAITING: &str = "waiting";
    pub const READY: &str = "ready";
    pub const CLAIMED: &str = "claimed";
    pub const BLOCKED: &str = "blocked";
    pub const COMPLETED: &str = "completed";
    pub const FAILED: &str = "failed";
    pub const CANCELED: &str = "canceled";

    /// Every state, in the order a task usually moves through them.
    pub const ALL: [&str; 7] = [
        WAITING, READY, CLAIMED, BLOCKED, COMPLETED, FAILED, CANCELED,
    ];
    /// The states of a task that is not finished.
    pub const OPEN: [&str; 4] = [WAITING, READY, CLAIMED, BLOCKED];

    /// Whether a task in `state` is finished: completed, failed or canceled.
    pub fn is_finished(state: &str) -> bool {
        matches!(state, COMPLETED | FAILED | CANCELED)
    }
}

/// The board-kept state of a task, on its root message as [`Message::task`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// One of [`states`].
    pub state: String,
    /// The actor that created the task.
    pub creator: Actor,
    /// Who the task is kept for: a handle, `name` or `name/profile`. Only
    /// that principal, under that profile when one is named, can claim it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// While claimed: who holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<Actor>,
    /// While claimed: when the lease ends, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until: Option<String>,
    /// How many times the task has been claimed. The current or latest
    /// claim has this epoch.
    pub epoch: u64,
    /// IDs of the tasks this one needs, which must complete first.
    #[serde(default)]
    pub needs: Vec<String>,
    /// While blocked: the kind given, such as `needs_input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_kind: Option<String>,
    /// While blocked, failed or canceled: the reason given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Once completed: the result given, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Who made the latest change of state.
    pub changed_by: Actor,
    /// When the state last changed, RFC 3339.
    pub changed_at: String,
    /// The root's revision when it became a task. A larger
    /// [`Message::revision`] means the brief was edited since.
    pub brief_revision: u32,
}

/// What a worker should know about where a brief came from before acting
/// on it (design requirement S5). Returned with every claim and every task
/// read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trust {
    /// Who wrote the brief, the task's root message.
    pub brief_by: Actor,
    /// Who made it a task. Usually the same actor.
    pub creator: Actor,
    /// True when the brief was edited after it became a task.
    pub brief_edited: bool,
    /// True when any message in the thread came through a bridge. Always
    /// false until bridges exist.
    pub bridged: bool,
}

/// A task with its whole thread, as `GET /tasks/{id}` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    /// The root, with [`Message::task`] set.
    pub task: Message,
    pub replies: Vec<Message>,
    pub trust: Trust,
}

/// Body of `POST /tasks`: a new task, or an existing thread made a task.
///
/// Exactly one of `channel` (with `text`) or `thread_id` must be set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTaskRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// The brief, Markdown. Required with `channel`, refused with `thread_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Make the thread rooted at this message, or holding it, a task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Labels to set on the root. Keys follow the label rules.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// IDs of tasks on this board that must complete first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// Keep the task for one actor: a profile of the caller's principal
    /// (`worker-1`) or a handle (`name`, `name/profile`).
    #[serde(default, rename = "for", skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Claim the task for the caller in the same write. Fails with
    /// `task_waiting` when it needs unfinished tasks.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub claim: bool,
    /// With `claim`: the lease, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_secs: Option<u64>,
    /// Retrying with the same key and request creates nothing and answers
    /// with the task created first, `duplicate: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Promote a request: the ID of a message in an intake channel this
    /// task carries out (S4). The task gets the `board.origin` label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTaskResponse {
    pub task: Message,
    /// Cursor of the last event the write appended.
    pub cursor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
    #[serde(default)]
    pub duplicate: bool,
}

/// A prerequisite in a batch: a key of another entry in the same batch,
/// or the ID of a task that exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NeedRef {
    Key { key: String },
    Id(String),
}

/// One entry of a batch. Any field not listed here is refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchTask {
    /// Names the entry for `needs` in other entries. Unique in the batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<NeedRef>,
    #[serde(default, rename = "for", skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
}

/// Body of `POST /tasks/batch`: up to [`MAX_TASKS_PER_BATCH`] tasks in one
/// channel, created together or not at all. A dependency cycle among them
/// fails with `invalid_needs`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTasksRequest {
    pub channel: String,
    pub tasks: Vec<BatchTask>,
    /// Retrying with the same key and batch creates nothing and answers with
    /// the tasks created first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTasksResponse {
    /// The tasks, in the order of the request.
    pub tasks: Vec<Message>,
    pub cursor: String,
    #[serde(default)]
    pub duplicate: bool,
}

/// Query of `GET /tasks`. Oldest task first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TasksQuery {
    /// A channel, or a subtree when it ends in `/`. Omitted means every channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// A state, several joined by commas, `open` (the default: waiting,
    /// ready, claimed and blocked) or `all`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Only tasks whose root carries this label: `key`, `key=value` or
    /// `key=a|b`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Only tasks kept for this handle.
    #[serde(default, rename = "for", skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskList {
    /// Each task's root, with [`Message::task`], and its reply count.
    pub tasks: Vec<ThreadSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// The board's event position when the list was read. Pass it to
    /// `wait --after` to see what changes next.
    pub cursor: String,
}

/// A claim, as a claim or a renewal returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// The root, with [`Message::task`].
    pub task: Message,
    /// The claim's fencing number. Later calls must carry it.
    pub epoch: u64,
    /// The claim's secret. Later calls must carry it.
    pub token: String,
    /// When the lease ends, RFC 3339.
    pub until: String,
    /// How long the lease is, in seconds.
    pub lease_secs: u64,
    pub trust: Trust,
    /// Cursor of the event the claim appended. A renewal appends none and
    /// gives the board's current position.
    pub cursor: String,
}

/// Body of `POST /tasks/claim`: the oldest ready task in a channel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimReadyRequest {
    /// A channel, or a subtree when it ends in `/`.
    pub channel: String,
    /// Label conditions on the root, all of which must hold: `key`,
    /// `key=value` or `key=a|b`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Creators to take tasks from besides the caller's principal: actor IDs,
    /// handles of principals (`name`) or of one profile (`name/profile`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from: Vec<String>,
    /// Hold the request up to this many seconds until a task is ready.
    /// Capped at [`crate::MAX_WAIT_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimReadyResponse {
    /// `None` when no task was ready before the wait ended.
    pub claim: Option<Claim>,
}

/// Body of `POST /tasks/{id}/{action}`. Each action reads the fields it
/// needs and refuses none of the others.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskActionRequest {
    /// The claim, for renew, release, complete, fail, and block of a
    /// claimed task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Claim and renew: the lease, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_secs: Option<u64>,
    /// Complete: what came of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Fail, block and cancel: why. Required for each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Block: a kind in words you choose, such as `needs_input` or
    /// `awaiting_approval`. 1 to 64 lowercase letters, digits, `_`, `-`, `.`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Assign: who to keep the task for, as in [`CreateTaskRequest`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Assign: return the task to anyone who can claim it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pool: bool,
    /// Needs: the new list of prerequisites, replacing the old one. Empty
    /// clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResponse {
    pub task: Message,
    /// Cursor of the last event the change appended, or the board's current
    /// position when nothing changed.
    pub cursor: String,
}

/// The actions of `POST /tasks/{id}/{action}`.
pub mod actions {
    pub const CLAIM: &str = "claim";
    pub const RENEW: &str = "renew";
    pub const RELEASE: &str = "release";
    pub const COMPLETE: &str = "complete";
    pub const FAIL: &str = "fail";
    pub const BLOCK: &str = "block";
    pub const UNBLOCK: &str = "unblock";
    pub const REOPEN: &str = "reopen";
    pub const ASSIGN: &str = "assign";
    pub const NEEDS: &str = "needs";
    pub const CANCEL: &str = "cancel";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_need_is_a_key_or_an_id() {
        let needs: Vec<NeedRef> = serde_json::from_str(r#"[{"key":"fix"},"msg_1"]"#).unwrap();
        assert_eq!(
            needs,
            vec![
                NeedRef::Key { key: "fix".into() },
                NeedRef::Id("msg_1".into())
            ]
        );
    }

    #[test]
    fn a_batch_entry_refuses_unknown_fields() {
        let ok: BatchTask =
            serde_json::from_str(r#"{"key":"a","text":"t","for":"shipper","labels":{"x":"1"}}"#)
                .unwrap();
        assert_eq!(ok.assignee.as_deref(), Some("shipper"));
        assert!(serde_json::from_str::<BatchTask>(r#"{"text":"t","noSync":true}"#).is_err());
    }

    #[test]
    fn finished_states() {
        for state in states::ALL {
            assert_eq!(
                states::is_finished(state),
                !states::OPEN.contains(&state),
                "{state}"
            );
        }
    }
}
