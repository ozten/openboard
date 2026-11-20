//! Tasks: the state machine, leases with fencing, dependencies and batches.
//!
//! A task is a row in `tasks` keyed by its root message. Its state is kept
//! in that row and mirrored, in the same transaction, in the reserved label
//! `board.task` on the root, so label filters see it. Every change of state
//! appends one `task.*` event to the board's log, chained like every other.
//!
//! Every write here is one immediate transaction on the store's only
//! connection, so writes are serialised: two claims of one task cannot both
//! see it ready, and there are never two valid claims on one task. A lease
//! that has run out is ended at the start of every task write on its board,
//! before anything else is decided, and by the server's upkeep task.

use std::collections::{BTreeMap, HashMap, HashSet};

use openboard_api::event_types as ev;
use openboard_api::tasks::{states, Trust, TASK_LABEL};
use openboard_api::{Message, Thread, ThreadSummary};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, TransactionBehavior};

use super::{
    actor_from_row, append_event, channel_clause, ensure_board, fill_messages, head,
    insert_message, load_message, message_from_row, random_hex, upsert_actor, BoardRef,
    LabelFilter, MentionInput, PostTarget, Result, Store, StoreError, Writer,
    MAX_LABELS_PER_MESSAGE, MESSAGE_COLUMNS,
};
use crate::authz::Principal;
use crate::identity::parse_actor_kind;
use crate::names::{handle_key, ChannelSel};
use crate::time::{now_ms, now_rfc3339, rfc3339_from_ms};

/// Why a task write was refused. Each maps to one API error code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskError {
    NotATask,
    Exists,
    Claimed,
    Assigned,
    Waiting,
    Blocked,
    Finished,
    /// The action needs another state. Holds the state the task is in.
    State(String),
    NotCreator,
    ClaimLost,
    ClaimNotHeld,
    /// Holds what is wrong, without any message text.
    InvalidNeeds(String),
    InvalidAssignee,
}

impl From<TaskError> for StoreError {
    fn from(error: TaskError) -> Self {
        StoreError::Task(error)
    }
}

/// Who a task is kept for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assignee {
    pub principal_id: String,
    /// `None` keeps it for any profile of the principal.
    pub profile: Option<String>,
    /// As shown: `name` or `name/profile`.
    pub handle: String,
}

/// How an assignee was written, before it is resolved against the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AssigneeSpec {
    /// A profile of the creator's principal: `worker-1`.
    Profile(String),
    /// A principal by handle, with an optional profile: `@name`,
    /// `@name/profile`, or `name/profile`.
    Handle {
        handle: String,
        profile: Option<String>,
    },
}

/// One creator a ready claim may take tasks from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CreatorSel {
    Actor(String),
    /// A principal by handle, or one profile of it.
    Handle {
        handle: String,
        profile: Option<String>,
    },
}

/// A claim's proof: its epoch and token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimProof {
    pub epoch: u64,
    pub token: String,
}

/// What a new task is made from.
pub(crate) enum Brief {
    /// A new thread with this text.
    New {
        channel: String,
        text: String,
        mentions: MentionInput,
    },
    /// The thread that holds this message.
    Thread(String),
}

pub(crate) struct NewTask {
    pub brief: Brief,
    pub labels: BTreeMap<String, String>,
    pub needs: Vec<String>,
    pub assignee: Option<AssigneeSpec>,
    /// Claim it in the same write, with this lease in milliseconds.
    pub claim: Option<i64>,
    /// Key and request fingerprint.
    pub idempotency: Option<(String, String)>,
}

/// A prerequisite in a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Need {
    Key(String),
    Id(String),
}

pub(crate) struct BatchEntry {
    pub key: Option<String>,
    pub text: String,
    pub labels: BTreeMap<String, String>,
    pub needs: Vec<Need>,
    pub assignee: Option<AssigneeSpec>,
    pub mentions: MentionInput,
}

/// A claim as the store returns it.
pub(crate) struct ClaimOut {
    pub message: Message,
    pub epoch: u64,
    pub token: String,
    pub until_ms: i64,
    pub lease_ms: i64,
    pub trust: Trust,
    /// Position of the claim's event; `None` for a renewal.
    pub seq: Option<i64>,
}

pub(crate) struct TaskCreated {
    pub board: BoardRef,
    pub message: Message,
    pub claim: Option<ClaimOut>,
    /// Position of the last event appended, or of the head for a duplicate.
    pub seq: i64,
    pub duplicate: bool,
}

pub(crate) struct TasksCreated {
    pub board: BoardRef,
    pub messages: Vec<Message>,
    pub seq: i64,
    pub duplicate: bool,
}

/// A change to one task, other than a claim.
pub(crate) enum Act {
    Renew(ClaimProof, Option<i64>),
    Release(ClaimProof),
    Complete(ClaimProof, Option<String>),
    Fail(ClaimProof, String),
    Block {
        claim: Option<ClaimProof>,
        reason: String,
        kind: Option<String>,
    },
    Unblock,
    Reopen,
    Assign(Option<AssigneeSpec>),
    Needs(Vec<String>),
    Cancel(String),
}

pub(crate) struct TaskChanged {
    pub message: Message,
    /// Position of the last event appended, `None` when nothing changed.
    pub seq: Option<i64>,
    /// Set by a renewal.
    pub claim: Option<ClaimOut>,
}

pub(crate) struct TaskPage {
    /// Oldest first, each with its creation position.
    pub tasks: Vec<(i64, ThreadSummary)>,
    pub more: bool,
    /// The board's event position when the page was read.
    pub head: i64,
}

/// What a listing asks for.
pub(crate) struct TaskQuery {
    pub channel: Option<ChannelSel>,
    pub states: Vec<&'static str>,
    pub label: Option<LabelFilter>,
    pub assignee: Option<String>,
    pub after: Option<i64>,
    pub limit: u32,
}

/// The row of a task, as decisions need it.
#[derive(Debug, Clone)]
struct Row {
    id: String,
    channel: String,
    state: String,
    creator_actor: String,
    creator_principal: String,
    assignee_principal: Option<String>,
    assignee_profile: Option<String>,
    epoch: u64,
    lease_token: Option<String>,
    holder_actor: Option<String>,
    lease_ms: Option<i64>,
}

const ROW_COLUMNS: &str =
    "id, channel, state, creator_actor, creator_principal, assignee_principal, \
                           assignee_profile, epoch, lease_token, holder_actor, lease_ms";

fn row(conn: &Connection, board: &BoardRef, id: &str) -> Result<Option<Row>> {
    Ok(conn
        .query_row(
            &format!("SELECT {ROW_COLUMNS} FROM tasks WHERE board_id = ?1 AND id = ?2"),
            params![board.id, id],
            |r| {
                Ok(Row {
                    id: r.get(0)?,
                    channel: r.get(1)?,
                    state: r.get(2)?,
                    creator_actor: r.get(3)?,
                    creator_principal: r.get(4)?,
                    assignee_principal: r.get(5)?,
                    assignee_profile: r.get(6)?,
                    epoch: r.get::<_, i64>(7)?.max(0) as u64,
                    lease_token: r.get(8)?,
                    holder_actor: r.get(9)?,
                    lease_ms: r.get(10)?,
                })
            },
        )
        .optional()?)
}

/// The task a message belongs to: the thread's root, when it is a task.
fn task_of(conn: &Connection, board: &BoardRef, message_id: &str) -> Result<Row> {
    let root: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM messages WHERE board_id = ?1 AND id = ?2",
            params![board.id, message_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(root) = root else {
        return Err(StoreError::MessageMissing);
    };
    row(conn, board, &root)?.ok_or(TaskError::NotATask.into())
}

/// Whether `by` is the task's creator: the same actor, or the same principal
/// calling without a profile.
fn is_creator(task: &Row, by: &Writer) -> bool {
    by.actor.id == task.creator_actor
        || (by.actor.profile.is_none() && by.principal_id == task.creator_principal)
}

fn may_claim_assigned(task: &Row, by: &Writer) -> bool {
    match &task.assignee_principal {
        None => true,
        Some(principal) => {
            *principal == by.principal_id
                && match (&task.assignee_profile, &by.actor.profile) {
                    (None, _) => true,
                    (Some(wanted), Some(mine)) => wanted.eq_ignore_ascii_case(mine),
                    (Some(_), None) => false,
                }
        }
    }
}

/// The error for a task that cannot be claimed because of its state.
fn not_claimable(state: &str) -> TaskError {
    match state {
        states::CLAIMED => TaskError::Claimed,
        states::WAITING => TaskError::Waiting,
        states::BLOCKED => TaskError::Blocked,
        _ => TaskError::Finished,
    }
}

/// Checks a claim's proof against the task. The order matters: an epoch
/// from an earlier claim has ended; a matching epoch with the wrong holder
/// or token is not the caller's; and a claim that matches but whose task
/// has moved on, because it was settled, released or expired, has ended.
fn check_claim(task: &Row, by: &Writer, proof: &ClaimProof) -> std::result::Result<(), TaskError> {
    if proof.epoch < task.epoch {
        return Err(TaskError::ClaimLost);
    }
    let token_ok = task
        .lease_token
        .as_deref()
        .is_some_and(|token| crate::identity::constant_time_eq(token, &proof.token));
    if proof.epoch > task.epoch
        || !token_ok
        || task.holder_actor.as_deref() != Some(by.actor.id.as_str())
    {
        return Err(TaskError::ClaimNotHeld);
    }
    if task.state != states::CLAIMED {
        return Err(TaskError::ClaimLost);
    }
    Ok(())
}

/// Whether every task this one needs has completed.
fn needs_done(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT NOT EXISTS (SELECT 1 FROM task_needs n JOIN tasks p ON p.id = n.needs_id
                            WHERE n.task_id = ?1 AND p.state <> ?2)",
        params![id, states::COMPLETED],
        |r| r.get(0),
    )?)
}

/// Sets the state, who changed it and when, mirrors it in the label and
/// appends the event. Returns the event's position.
fn change(
    conn: &Connection,
    board: &BoardRef,
    task: &Row,
    by: &Writer,
    state: &str,
    kind: &str,
) -> Result<i64> {
    let now = now_rfc3339();
    upsert_actor(conn, by)?;
    conn.execute(
        "UPDATE tasks SET state = ?1, changed_actor = ?2, changed_at = ?3 WHERE id = ?4",
        params![state, by.actor.id, now, task.id],
    )?;
    conn.execute(
        "INSERT INTO labels (message_id, thread_id, key, value) VALUES (?1, ?1, ?2, ?3)
         ON CONFLICT (message_id, key) DO UPDATE SET value = excluded.value",
        params![task.id, TASK_LABEL, state],
    )?;
    append_event(
        conn,
        board,
        kind,
        (&task.channel, &task.id, &task.id),
        by,
        &now,
    )
}

/// Ends the current claim's hold on the task: no holder, no lease. The
/// token stays, so a late call with it is told its claim was lost.
fn end_lease(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("UPDATE tasks SET lease_until_ms = NULL WHERE id = ?1", [id])?;
    Ok(())
}

/// Readies the waiting tasks whose last unfinished prerequisite was
/// `completed`. Returns the position of the last event appended.
fn ready_dependents(
    conn: &Connection,
    board: &BoardRef,
    completed: &str,
    by: &Writer,
) -> Result<Option<i64>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {ROW_COLUMNS} FROM tasks t WHERE t.state = ?2 AND t.id IN
            (SELECT task_id FROM task_needs WHERE needs_id = ?1)
         ORDER BY t.created_seq"
    ))?;
    let waiting: Vec<String> = stmt
        .query_map(params![completed, states::WAITING], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut last = None;
    for id in waiting {
        if needs_done(conn, &id)? {
            if let Some(task) = row(conn, board, &id)? {
                last = Some(change(
                    conn,
                    board,
                    &task,
                    by,
                    states::READY,
                    ev::TASK_READY,
                )?);
            }
        }
    }
    Ok(last)
}

/// Ends every lease on the board that has run out by `now`: the task is
/// ready again and a `task.expired` event names the holder. Returns the
/// position of the last event appended.
fn expire_due(conn: &Connection, board: &BoardRef, now: i64) -> Result<Option<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT t.id, a.id, a.kind, a.handle, a.profile, a.principal FROM tasks t
         JOIN actors a ON a.id = t.holder_actor
         WHERE t.board_id = ?1 AND t.state = ?2 AND t.lease_until_ms <= ?3
         ORDER BY t.lease_until_ms",
    )?;
    let due: Vec<(String, Writer)> = stmt
        .query_map(params![board.id, states::CLAIMED, now], |r| {
            Ok((
                r.get(0)?,
                Writer {
                    actor: actor_from_row(r, 1)?,
                    principal_id: r.get(5)?,
                    credential_id: None,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut last = None;
    for (id, holder) in due {
        if let Some(task) = row(conn, board, &id)? {
            end_lease(conn, &id)?;
            last = Some(change(
                conn,
                board,
                &task,
                &holder,
                states::READY,
                ev::TASK_EXPIRED,
            )?);
        }
    }
    Ok(last)
}

fn trust(conn: &Connection, board: &BoardRef, id: &str) -> Result<Trust> {
    let (brief_by, creator, edited) = conn.query_row(
        "SELECT a.id, a.kind, a.handle, a.profile, o.id, o.kind, o.handle, o.profile,
                m.revision > t.brief_revision
         FROM tasks t JOIN messages m ON m.id = t.id
         JOIN actors a ON a.id = m.actor_id JOIN actors o ON o.id = t.creator_actor
         WHERE t.board_id = ?1 AND t.id = ?2",
        params![board.id, id],
        |r| Ok((actor_from_row(r, 0)?, actor_from_row(r, 4)?, r.get(8)?)),
    )?;
    Ok(Trust {
        brief_by,
        creator,
        brief_edited: edited,
        bridged: false,
    })
}

/// Clamps a lease to what the board allows.
fn lease_or_default(lease_ms: Option<i64>) -> i64 {
    use openboard_api::tasks::{DEFAULT_LEASE_SECS, MAX_LEASE_SECS, MIN_LEASE_SECS};
    let ms = lease_ms.unwrap_or(DEFAULT_LEASE_SECS as i64 * 1000);
    ms.clamp(MIN_LEASE_SECS as i64 * 1000, MAX_LEASE_SECS as i64 * 1000)
}

/// Claims `task` for `by`, which has been checked to be allowed to.
fn take(
    conn: &Connection,
    board: &BoardRef,
    task: &Row,
    by: &Writer,
    lease_ms: Option<i64>,
) -> Result<ClaimOut> {
    let lease_ms = lease_or_default(lease_ms);
    let until_ms = now_ms().saturating_add(lease_ms);
    let token = format!("tkn_{}", random_hex(conn, 16)?);
    let epoch = task.epoch + 1;
    upsert_actor(conn, by)?;
    conn.execute(
        "UPDATE tasks SET epoch = ?1, lease_token = ?2, holder_actor = ?3, lease_until_ms = ?4,
                          lease_ms = ?5, block_kind = NULL, reason = NULL
         WHERE id = ?6",
        params![
            epoch as i64,
            token,
            by.actor.id,
            until_ms,
            lease_ms,
            task.id
        ],
    )?;
    let seq = change(conn, board, task, by, states::CLAIMED, ev::TASK_CLAIMED)?;
    Ok(ClaimOut {
        message: load_message(conn, board, &task.id)?,
        epoch,
        token,
        until_ms,
        lease_ms,
        trust: trust(conn, board, &task.id)?,
        seq: Some(seq),
    })
}

/// Finds a principal by handle. A local board's one principal is in no table.
fn find_principal(conn: &Connection, handle: &str, local: bool) -> Result<Option<Principal>> {
    let key = handle_key(handle);
    if local {
        let principal = Principal::local();
        return Ok((key == handle_key(&principal.handle)).then_some(principal));
    }
    let found: Option<(String, String, String)> = conn
        .query_row(
            "SELECT id, kind, handle FROM principals WHERE handle_key = ?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(found.and_then(|(id, kind, handle)| {
        parse_actor_kind(&kind).map(|kind| Principal { id, kind, handle })
    }))
}

fn principal_handle(conn: &Connection, principal_id: &str, local: bool) -> Result<String> {
    if local {
        return Ok(Principal::local().handle);
    }
    conn.query_row(
        "SELECT handle FROM principals WHERE id = ?1",
        [principal_id],
        |r| r.get(0),
    )
    .optional()?
    .ok_or(TaskError::InvalidAssignee.into())
}

fn resolve_assignee(
    conn: &Connection,
    spec: &AssigneeSpec,
    creator_principal: &str,
    local: bool,
) -> Result<Assignee> {
    match spec {
        AssigneeSpec::Profile(profile) => {
            let name = principal_handle(conn, creator_principal, local)?;
            Ok(Assignee {
                principal_id: creator_principal.to_owned(),
                profile: Some(profile.clone()),
                handle: format!("{name}/{profile}"),
            })
        }
        AssigneeSpec::Handle { handle, profile } => {
            let principal =
                find_principal(conn, handle, local)?.ok_or(TaskError::InvalidAssignee)?;
            Ok(Assignee {
                handle: match profile {
                    Some(profile) => format!("{}/{profile}", principal.handle),
                    None => principal.handle.clone(),
                },
                principal_id: principal.id,
                profile: profile.clone(),
            })
        }
    }
}

/// Checks a list of prerequisites of the task `id`: each a different task
/// on this board, not `id` itself, and not depending on `id`.
fn check_needs(
    conn: &Connection,
    board: &BoardRef,
    id: Option<&str>,
    needs: &[String],
) -> Result<()> {
    use openboard_api::tasks::MAX_NEEDS_PER_TASK;
    if needs.len() > MAX_NEEDS_PER_TASK {
        return Err(TaskError::InvalidNeeds(format!(
            "a task needs at most {MAX_NEEDS_PER_TASK} others"
        ))
        .into());
    }
    let mut seen = HashSet::new();
    for need in needs {
        if !seen.insert(need.as_str()) {
            return Err(TaskError::InvalidNeeds("a prerequisite is named twice".into()).into());
        }
        if Some(need.as_str()) == id {
            return Err(TaskError::InvalidNeeds("a task cannot need itself".into()).into());
        }
        if row(conn, board, need)?.is_none() {
            return Err(TaskError::InvalidNeeds(format!(
                "`{}` is not a task on this board",
                clip(need)
            ))
            .into());
        }
    }
    let Some(id) = id else {
        return Ok(());
    };
    // A cycle exists if `id` is reachable from a new prerequisite.
    let mut stack: Vec<String> = needs.to_vec();
    let mut visited: HashSet<String> = HashSet::new();
    let mut stmt = conn.prepare_cached("SELECT needs_id FROM task_needs WHERE task_id = ?1")?;
    while let Some(next) = stack.pop() {
        if next == id {
            return Err(TaskError::InvalidNeeds("the change would make a cycle".into()).into());
        }
        if !visited.insert(next.clone()) {
            continue;
        }
        let more: Vec<String> = stmt
            .query_map([&next], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        stack.extend(more);
    }
    Ok(())
}

/// An ID cut short for an error message. IDs are opaque but untrusted.
fn clip(text: &str) -> String {
    text.chars()
        .take(40)
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn set_labels(conn: &Connection, id: &str, labels: &BTreeMap<String, String>) -> Result<()> {
    for (key, value) in labels {
        conn.execute(
            "INSERT INTO labels (message_id, thread_id, key, value) VALUES (?1, ?1, ?2, ?3)
             ON CONFLICT (message_id, key) DO UPDATE SET value = excluded.value",
            params![id, key, value],
        )?;
    }
    // The state takes one place more.
    let total: i64 = conn.query_row(
        "SELECT count(*) FROM labels WHERE message_id = ?1 AND key <> ?2",
        params![id, TASK_LABEL],
        |r| r.get(0),
    )?;
    if total as usize >= MAX_LABELS_PER_MESSAGE {
        return Err(StoreError::TooManyLabels);
    }
    Ok(())
}

/// Inserts the task row for the existing root `id`, its label and its
/// `task.created` event. `waiting` decides the first state.
#[allow(clippy::too_many_arguments)]
fn insert_task(
    conn: &Connection,
    board: &BoardRef,
    id: &str,
    channel: &str,
    by: &Writer,
    assignee: Option<&Assignee>,
    waiting: bool,
    revision: u32,
) -> Result<(Row, i64)> {
    let state = if waiting {
        states::WAITING
    } else {
        states::READY
    };
    let now = now_rfc3339();
    upsert_actor(conn, by)?;
    let created_seq = head(conn, board.id)? + 1;
    conn.execute(
        "INSERT INTO tasks (id, board_id, channel, state, creator_actor, creator_principal,
                            assignee_principal, assignee_profile, assignee_handle,
                            changed_actor, changed_at, brief_revision, created_seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?5, ?10, ?11, ?12)",
        params![
            id,
            board.id,
            channel,
            state,
            by.actor.id,
            by.principal_id,
            assignee.map(|a| &a.principal_id),
            assignee.and_then(|a| a.profile.as_ref()),
            assignee.map(|a| &a.handle),
            now,
            revision,
            created_seq
        ],
    )?;
    conn.execute(
        "INSERT INTO labels (message_id, thread_id, key, value) VALUES (?1, ?1, ?2, ?3)
         ON CONFLICT (message_id, key) DO UPDATE SET value = excluded.value",
        params![id, TASK_LABEL, state],
    )?;
    let seq = append_event(conn, board, ev::TASK_CREATED, (channel, id, id), by, &now)?;
    let task = row(conn, board, id)?
        .ok_or_else(|| StoreError::Other("a task vanished after it was created".into()))?;
    Ok((task, seq))
}

fn insert_needs(conn: &Connection, id: &str, needs: &[String]) -> Result<()> {
    conn.execute("DELETE FROM task_needs WHERE task_id = ?1", [id])?;
    for need in needs {
        conn.execute(
            "INSERT INTO task_needs (task_id, needs_id) VALUES (?1, ?2)",
            params![id, need],
        )?;
    }
    Ok(())
}

/// Looks up an idempotency key. `Ok(None)` when it is new.
fn earlier_keyed(
    conn: &Connection,
    board: &BoardRef,
    by: &Writer,
    key: &(String, String),
) -> Result<Option<Vec<String>>> {
    let found: Option<(String, String)> = conn
        .query_row(
            "SELECT fingerprint, task_ids FROM task_keys
             WHERE board_id = ?1 AND actor_id = ?2 AND key = ?3",
            params![board.id, by.actor.id, key.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match found {
        None => Ok(None),
        Some((fingerprint, _)) if fingerprint != key.1 => Err(StoreError::IdempotencyConflict),
        Some((_, ids)) => serde_json::from_str(&ids)
            .map(Some)
            .map_err(|error| StoreError::Other(format!("a stored key is unreadable: {error}"))),
    }
}

fn record_key(
    conn: &Connection,
    board: &BoardRef,
    by: &Writer,
    key: &(String, String),
    ids: &[String],
) -> Result<()> {
    let ids = serde_json::to_string(ids)
        .map_err(|error| StoreError::Other(format!("cannot record task IDs: {error}")))?;
    conn.execute(
        "INSERT INTO task_keys (board_id, actor_id, key, fingerprint, task_ids)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![board.id, by.actor.id, key.0, key.1, ids],
    )?;
    Ok(())
}

/// Called by an edit of a message, in its transaction. When the message is
/// a task's brief, an edit by anyone but the creator holds the task, and any
/// edit ends a claim on it (design S6). Returns the event appended, if any.
pub(super) fn on_brief_edited(
    conn: &Connection,
    board: &BoardRef,
    message_id: &str,
    by: &Writer,
) -> Result<Option<i64>> {
    let Some(task) = row(conn, board, message_id)? else {
        return Ok(None);
    };
    if states::is_finished(&task.state) {
        return Ok(None);
    }
    if !is_creator(&task, by) {
        if task.state == states::BLOCKED {
            return Ok(None);
        }
        end_lease(conn, &task.id)?;
        conn.execute(
            "UPDATE tasks SET block_kind = 'brief_edited', reason = 'the brief was edited by \
             someone other than the task''s creator' WHERE id = ?1",
            [&task.id],
        )?;
        return Ok(Some(change(
            conn,
            board,
            &task,
            by,
            states::BLOCKED,
            ev::TASK_BLOCKED,
        )?));
    }
    if task.state == states::CLAIMED {
        end_lease(conn, &task.id)?;
        return Ok(Some(change(
            conn,
            board,
            &task,
            by,
            states::READY,
            ev::TASK_RELEASED,
        )?));
    }
    Ok(None)
}

/// Called by a delete of a message, in its transaction. Deleting a task's
/// root cancels the task unless it completed. Its result and reasons go
/// with the text.
pub(super) fn on_root_deleted(
    conn: &Connection,
    board: &BoardRef,
    message_id: &str,
    by: &Writer,
) -> Result<Option<i64>> {
    let Some(task) = row(conn, board, message_id)? else {
        return Ok(None);
    };
    conn.execute(
        "UPDATE tasks SET reason = NULL, result = NULL, block_kind = NULL WHERE id = ?1",
        [&task.id],
    )?;
    conn.execute(
        "UPDATE task_keys SET fingerprint = '' WHERE board_id = ?1 AND fingerprint <> ''
         AND EXISTS (SELECT 1 FROM json_each(task_keys.task_ids) WHERE value = ?2)",
        params![board.id, task.id],
    )?;
    if matches!(task.state.as_str(), states::COMPLETED | states::CANCELED) {
        return Ok(None);
    }
    end_lease(conn, &task.id)?;
    let seq = change(conn, board, &task, by, states::CANCELED, ev::TASK_CANCELED)?;
    // The label went with the tombstone's other labels; it does not come back.
    conn.execute("DELETE FROM labels WHERE message_id = ?1", [&task.id])?;
    Ok(Some(seq))
}

/// Erases the digest an idempotency key kept of a brief whose text changed.
pub(super) fn forget_task_fingerprint(conn: &Connection, board: &BoardRef, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE task_keys SET fingerprint = '' WHERE board_id = ?1 AND fingerprint <> ''
         AND EXISTS (SELECT 1 FROM json_each(task_keys.task_ids) WHERE value = ?2)",
        params![board.id, id],
    )?;
    Ok(())
}

impl Store {
    /// Creates one task: a new thread, or an existing one made a task.
    pub(crate) fn create_task(
        &self,
        target: PostTarget,
        by: &Writer,
        task: NewTask,
        local: bool,
    ) -> Result<TaskCreated> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let board = match target {
            PostTarget::Board(board) => board,
            PostTarget::NewBoard(name) => {
                ensure_board(&tx, &name, Some((&by.principal_id, &by.actor.handle)))?
            }
        };
        expire_due(&tx, &board, now_ms())?;
        if let Some(key) = &task.idempotency {
            if let Some(ids) = earlier_keyed(&tx, &board, by, key)? {
                let id = ids.first().cloned().unwrap_or_default();
                let message = load_message(&tx, &board, &id)?;
                let seq = head(&tx, board.id)?;
                return Ok(TaskCreated {
                    board,
                    message,
                    claim: None,
                    seq,
                    duplicate: true,
                });
            }
        }
        check_needs(&tx, &board, None, &task.needs)?;
        let assignee = match &task.assignee {
            Some(spec) => Some(resolve_assignee(&tx, spec, &by.principal_id, local)?),
            None => None,
        };
        let (id, channel, revision) = match task.brief {
            Brief::New {
                channel,
                text,
                mentions,
            } => {
                let inserted = insert_message(&tx, &board, &channel, None, by, &text, &mentions)?;
                (inserted.id, channel, 1)
            }
            Brief::Thread(message_id) => {
                let found: Option<(String, String, u32, bool)> = tx
                    .query_row(
                        "SELECT r.id, r.channel, r.revision, r.deleted_at IS NOT NULL
                         FROM messages m JOIN messages r ON r.id = m.thread_id
                         WHERE m.board_id = ?1 AND m.id = ?2",
                        params![board.id, message_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?;
                let Some((root, channel, revision, deleted)) = found else {
                    return Err(StoreError::ThreadMissing);
                };
                if deleted {
                    return Err(StoreError::MessageDeleted);
                }
                if row(&tx, &board, &root)?.is_some() {
                    return Err(TaskError::Exists.into());
                }
                (root, channel, revision)
            }
        };
        set_labels(&tx, &id, &task.labels)?;
        let waiting = task.needs.iter().try_fold(false, |waiting, need| {
            Ok::<_, StoreError>(
                waiting || row(&tx, &board, need)?.is_some_and(|p| p.state != states::COMPLETED),
            )
        })?;
        let (row_now, mut seq) = insert_task(
            &tx,
            &board,
            &id,
            &channel,
            by,
            assignee.as_ref(),
            waiting,
            revision,
        )?;
        insert_needs(&tx, &id, &task.needs)?;
        let claim = match task.claim {
            None => None,
            Some(lease) => {
                if waiting {
                    return Err(TaskError::Waiting.into());
                }
                if !may_claim_assigned(&row_now, by) {
                    return Err(TaskError::Assigned.into());
                }
                let claim = take(&tx, &board, &row_now, by, Some(lease))?;
                seq = claim.seq.unwrap_or(seq);
                Some(claim)
            }
        };
        if let Some(key) = &task.idempotency {
            record_key(&tx, &board, by, key, std::slice::from_ref(&id))?;
        }
        let message = load_message(&tx, &board, &id)?;
        tx.commit()?;
        Ok(TaskCreated {
            board,
            message,
            claim,
            seq,
            duplicate: false,
        })
    }

    /// Creates a batch of tasks in one channel, all or none.
    pub(crate) fn create_tasks(
        &self,
        target: PostTarget,
        by: &Writer,
        channel: &str,
        entries: Vec<BatchEntry>,
        idempotency: Option<(String, String)>,
        local: bool,
    ) -> Result<TasksCreated> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let board = match target {
            PostTarget::Board(board) => board,
            PostTarget::NewBoard(name) => {
                ensure_board(&tx, &name, Some((&by.principal_id, &by.actor.handle)))?
            }
        };
        expire_due(&tx, &board, now_ms())?;
        if let Some(key) = &idempotency {
            if let Some(ids) = earlier_keyed(&tx, &board, by, key)? {
                let messages = ids
                    .iter()
                    .map(|id| load_message(&tx, &board, id))
                    .collect::<Result<Vec<_>>>()?;
                let seq = head(&tx, board.id)?;
                return Ok(TasksCreated {
                    board,
                    messages,
                    seq,
                    duplicate: true,
                });
            }
        }

        // Keys, their positions, and the dependencies among the entries.
        let mut by_key: HashMap<&str, usize> = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            if let Some(key) = &entry.key {
                if by_key.insert(key.as_str(), index).is_some() {
                    return Err(TaskError::InvalidNeeds(format!(
                        "the key `{}` is used by two entries",
                        clip(key)
                    ))
                    .into());
                }
            }
        }
        let mut internal: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
        let mut external: Vec<Vec<String>> = vec![Vec::new(); entries.len()];
        for (index, entry) in entries.iter().enumerate() {
            for need in &entry.needs {
                match need {
                    Need::Key(key) => {
                        let Some(&other) = by_key.get(key.as_str()) else {
                            return Err(TaskError::InvalidNeeds(format!(
                                "no entry has the key `{}`",
                                clip(key)
                            ))
                            .into());
                        };
                        if other == index {
                            return Err(TaskError::InvalidNeeds(
                                "an entry cannot need itself".into(),
                            )
                            .into());
                        }
                        internal[index].push(other);
                    }
                    Need::Id(id) => external[index].push(id.clone()),
                }
            }
            check_needs(&tx, &board, None, &external[index])?;
            if internal[index].len() + external[index].len()
                > openboard_api::tasks::MAX_NEEDS_PER_TASK
            {
                return Err(TaskError::InvalidNeeds(format!(
                    "a task needs at most {} others",
                    openboard_api::tasks::MAX_NEEDS_PER_TASK
                ))
                .into());
            }
        }
        // Kahn's algorithm: a batch whose entries cannot all be ordered has
        // a cycle.
        let mut indegree: Vec<usize> = internal.iter().map(Vec::len).collect();
        let mut ready: Vec<usize> = (0..entries.len()).filter(|i| indegree[*i] == 0).collect();
        let mut ordered = 0;
        while let Some(done) = ready.pop() {
            ordered += 1;
            for (index, needs) in internal.iter().enumerate() {
                for need in needs {
                    if *need == done {
                        indegree[index] -= 1;
                        if indegree[index] == 0 {
                            ready.push(index);
                        }
                    }
                }
            }
        }
        if ordered != entries.len() {
            return Err(TaskError::InvalidNeeds("the batch's needs make a cycle".into()).into());
        }

        let mut ids: Vec<String> = Vec::with_capacity(entries.len());
        let mut seq = head(&tx, board.id)?;
        for (index, entry) in entries.iter().enumerate() {
            let assignee = match &entry.assignee {
                Some(spec) => Some(resolve_assignee(&tx, spec, &by.principal_id, local)?),
                None => None,
            };
            let inserted =
                insert_message(&tx, &board, channel, None, by, &entry.text, &entry.mentions)?;
            set_labels(&tx, &inserted.id, &entry.labels)?;
            let waiting = !internal[index].is_empty()
                || external[index].iter().try_fold(false, |waiting, need| {
                    Ok::<_, StoreError>(
                        waiting
                            || row(&tx, &board, need)?
                                .is_some_and(|p| p.state != states::COMPLETED),
                    )
                })?;
            let (_, created) = insert_task(
                &tx,
                &board,
                &inserted.id,
                channel,
                by,
                assignee.as_ref(),
                waiting,
                1,
            )?;
            seq = created;
            ids.push(inserted.id);
        }
        for index in 0..entries.len() {
            let mut needs: Vec<String> = internal[index].iter().map(|i| ids[*i].clone()).collect();
            needs.extend(external[index].iter().cloned());
            insert_needs(&tx, &ids[index], &needs)?;
        }
        if let Some(key) = &idempotency {
            record_key(&tx, &board, by, key, &ids)?;
        }
        let messages = ids
            .iter()
            .map(|id| load_message(&tx, &board, id))
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(TasksCreated {
            board,
            messages,
            seq,
            duplicate: false,
        })
    }

    /// Claims one task by its ID, or the ID of any message in its thread.
    pub(crate) fn claim_task(
        &self,
        board: &BoardRef,
        by: &Writer,
        id: &str,
        lease_ms: Option<i64>,
    ) -> Result<ClaimOut> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_due(&tx, board, now_ms())?;
        let task = task_of(&tx, board, id)?;
        if task.state != states::READY {
            return Err(not_claimable(&task.state).into());
        }
        if !may_claim_assigned(&task, by) {
            return Err(TaskError::Assigned.into());
        }
        let claim = take(&tx, board, &task, by, lease_ms)?;
        tx.commit()?;
        Ok(claim)
    }

    /// Claims the oldest ready task in `channel` that the caller may take:
    /// one kept for it first, then an unassigned one from a creator it
    /// takes from. `Ok(None)` when there is none.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn claim_ready(
        &self,
        board: &BoardRef,
        by: &Writer,
        channel: &ChannelSel,
        labels: &[LabelFilter],
        from: &[CreatorSel],
        lease_ms: Option<i64>,
        local: bool,
    ) -> Result<Option<ClaimOut>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_due(&tx, board, now_ms())?;

        let mut creator_principals = vec![by.principal_id.clone()];
        let mut creator_actors: Vec<String> = Vec::new();
        for creator in from {
            match creator {
                CreatorSel::Actor(id) => creator_actors.push(id.clone()),
                CreatorSel::Handle { handle, profile } => {
                    // A creator nobody has is skipped: it created nothing.
                    if let Some(principal) = find_principal(&tx, handle, local)? {
                        match profile {
                            None => creator_principals.push(principal.id),
                            Some(profile) => {
                                creator_actors.push(principal.actor(Some(profile)).id);
                            }
                        }
                    }
                }
            }
        }

        let mut values = vec![
            Value::Integer(board.id),
            Value::Text(states::READY.to_owned()),
        ];
        let mut sql = format!(
            "SELECT t.id FROM tasks t WHERE t.board_id = ? AND t.state = ? AND {}",
            channel_clause("t.channel", channel, &mut values)
        );
        // Kept for the caller, or unassigned and from a creator it takes from.
        sql.push_str(
            " AND ((t.assignee_principal = ? AND (t.assignee_profile IS NULL OR \
             (? IS NOT NULL AND lower(t.assignee_profile) = lower(?))))",
        );
        values.push(Value::Text(by.principal_id.clone()));
        let profile = by.actor.profile.clone().map_or(Value::Null, Value::Text);
        values.push(profile.clone());
        values.push(profile);
        sql.push_str(" OR (t.assignee_principal IS NULL AND (t.creator_principal IN (");
        sql.push_str(&vec!["?"; creator_principals.len()].join(", "));
        sql.push(')');
        values.extend(creator_principals.into_iter().map(Value::Text));
        if !creator_actors.is_empty() {
            sql.push_str(" OR t.creator_actor IN (");
            sql.push_str(&vec!["?"; creator_actors.len()].join(", "));
            sql.push(')');
            values.extend(creator_actors.into_iter().map(Value::Text));
        }
        sql.push_str(")))");
        for label in labels {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM labels l WHERE l.message_id = t.id AND l.key = ?",
            );
            values.push(Value::Text(label.key.clone()));
            if !label.values.is_empty() {
                sql.push_str(" AND l.value IN (");
                sql.push_str(&vec!["?"; label.values.len()].join(", "));
                sql.push(')');
                values.extend(label.values.iter().cloned().map(Value::Text));
            }
            sql.push(')');
        }
        sql.push_str(" ORDER BY (t.assignee_principal IS NULL), t.created_seq LIMIT 1");
        let found: Option<String> = tx
            .query_row(&sql, params_from_iter(values), |r| r.get(0))
            .optional()?;
        let claim = match found {
            None => None,
            Some(id) => {
                let task = row(&tx, board, &id)?
                    .ok_or_else(|| StoreError::Other("a ready task vanished".into()))?;
                Some(take(&tx, board, &task, by, lease_ms)?)
            }
        };
        tx.commit()?;
        Ok(claim)
    }

    /// Applies one change to a task. `manage` is whether the caller holds
    /// the task-manage permission in the task's channel, which stands in
    /// for creatorship.
    pub(crate) fn task_act(
        &self,
        board: &BoardRef,
        by: &Writer,
        id: &str,
        act: Act,
        manage: &dyn Fn(&str) -> bool,
        local: bool,
    ) -> Result<TaskChanged> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expired = expire_due(&tx, board, now_ms())?;
        let task = task_of(&tx, board, id)?;
        let creator = is_creator(&task, by) || manage(&task.channel);
        let require_creator = || {
            if creator {
                Ok(())
            } else {
                Err(StoreError::from(TaskError::NotCreator))
            }
        };
        let mut claim = None;
        let seq: Option<i64> = match act {
            Act::Renew(proof, lease) => {
                check_claim(&task, by, &proof)?;
                let lease_ms = lease_or_default(lease.or(task.lease_ms));
                let until_ms = now_ms().saturating_add(lease_ms);
                tx.execute(
                    "UPDATE tasks SET lease_until_ms = ?1, lease_ms = ?2 WHERE id = ?3",
                    params![until_ms, lease_ms, task.id],
                )?;
                claim = Some(ClaimOut {
                    message: load_message(&tx, board, &task.id)?,
                    epoch: task.epoch,
                    token: proof.token,
                    until_ms,
                    lease_ms,
                    trust: trust(&tx, board, &task.id)?,
                    seq: None,
                });
                None
            }
            Act::Release(proof) => {
                check_claim(&task, by, &proof)?;
                end_lease(&tx, &task.id)?;
                Some(change(
                    &tx,
                    board,
                    &task,
                    by,
                    states::READY,
                    ev::TASK_RELEASED,
                )?)
            }
            Act::Complete(proof, result) => {
                check_claim(&task, by, &proof)?;
                end_lease(&tx, &task.id)?;
                tx.execute(
                    "UPDATE tasks SET result = ?1 WHERE id = ?2",
                    params![result, task.id],
                )?;
                let done = change(&tx, board, &task, by, states::COMPLETED, ev::TASK_COMPLETED)?;
                Some(ready_dependents(&tx, board, &task.id, by)?.unwrap_or(done))
            }
            Act::Fail(proof, reason) => {
                check_claim(&task, by, &proof)?;
                end_lease(&tx, &task.id)?;
                tx.execute(
                    "UPDATE tasks SET reason = ?1 WHERE id = ?2",
                    params![reason, task.id],
                )?;
                Some(change(
                    &tx,
                    board,
                    &task,
                    by,
                    states::FAILED,
                    ev::TASK_FAILED,
                )?)
            }
            Act::Block {
                claim: proof,
                reason,
                kind,
            } => {
                match (&proof, task.state.as_str()) {
                    (Some(proof), _) => check_claim(&task, by, proof)?,
                    (None, states::READY | states::WAITING) => require_creator()?,
                    (None, states::CLAIMED) => {
                        return Err(if task.holder_actor.as_deref() == Some(&by.actor.id) {
                            TaskError::ClaimNotHeld
                        } else {
                            TaskError::Claimed
                        }
                        .into())
                    }
                    (None, states::BLOCKED) => return Err(TaskError::Blocked.into()),
                    (None, _) => return Err(TaskError::Finished.into()),
                }
                end_lease(&tx, &task.id)?;
                tx.execute(
                    "UPDATE tasks SET reason = ?1, block_kind = ?2 WHERE id = ?3",
                    params![reason, kind, task.id],
                )?;
                Some(change(
                    &tx,
                    board,
                    &task,
                    by,
                    states::BLOCKED,
                    ev::TASK_BLOCKED,
                )?)
            }
            Act::Unblock => {
                require_creator()?;
                if task.state != states::BLOCKED {
                    return Err(TaskError::State(task.state.clone()).into());
                }
                tx.execute(
                    "UPDATE tasks SET reason = NULL, block_kind = NULL WHERE id = ?1",
                    [&task.id],
                )?;
                let next = if needs_done(&tx, &task.id)? {
                    states::READY
                } else {
                    states::WAITING
                };
                Some(change(&tx, board, &task, by, next, ev::TASK_UNBLOCKED)?)
            }
            Act::Reopen => {
                require_creator()?;
                if task.state != states::FAILED {
                    return Err(TaskError::State(task.state.clone()).into());
                }
                tx.execute("UPDATE tasks SET reason = NULL WHERE id = ?1", [&task.id])?;
                let next = if needs_done(&tx, &task.id)? {
                    states::READY
                } else {
                    states::WAITING
                };
                Some(change(&tx, board, &task, by, next, ev::TASK_REOPENED)?)
            }
            Act::Assign(spec) => {
                require_creator()?;
                if states::is_finished(&task.state) {
                    return Err(TaskError::Finished.into());
                }
                let assignee = match &spec {
                    Some(spec) => {
                        Some(resolve_assignee(&tx, spec, &task.creator_principal, local)?)
                    }
                    None => None,
                };
                tx.execute(
                    "UPDATE tasks SET assignee_principal = ?1, assignee_profile = ?2,
                                      assignee_handle = ?3 WHERE id = ?4",
                    params![
                        assignee.as_ref().map(|a| &a.principal_id),
                        assignee.as_ref().and_then(|a| a.profile.as_ref()),
                        assignee.as_ref().map(|a| &a.handle),
                        task.id
                    ],
                )?;
                let state = task.state.clone();
                Some(change(&tx, board, &task, by, &state, ev::TASK_ASSIGNED)?)
            }
            Act::Needs(needs) => {
                require_creator()?;
                match task.state.as_str() {
                    states::CLAIMED => return Err(TaskError::Claimed.into()),
                    state if states::is_finished(state) => return Err(TaskError::Finished.into()),
                    _ => {}
                }
                check_needs(&tx, board, Some(&task.id), &needs)?;
                insert_needs(&tx, &task.id, &needs)?;
                let next = match task.state.as_str() {
                    states::BLOCKED => states::BLOCKED,
                    _ if needs_done(&tx, &task.id)? => states::READY,
                    _ => states::WAITING,
                };
                Some(change(&tx, board, &task, by, next, ev::TASK_NEEDS_CHANGED)?)
            }
            Act::Cancel(reason) => {
                require_creator()?;
                if matches!(task.state.as_str(), states::COMPLETED | states::CANCELED) {
                    return Err(TaskError::Finished.into());
                }
                end_lease(&tx, &task.id)?;
                tx.execute(
                    "UPDATE tasks SET reason = ?1, block_kind = NULL WHERE id = ?2",
                    params![reason, task.id],
                )?;
                Some(change(
                    &tx,
                    board,
                    &task,
                    by,
                    states::CANCELED,
                    ev::TASK_CANCELED,
                )?)
            }
        };
        let message = load_message(&tx, board, &task.id)?;
        tx.commit()?;
        Ok(TaskChanged {
            message,
            seq: seq.or(expired),
            claim,
        })
    }

    /// Ends every lease that has run out, on every board. Returns, for each
    /// board where events were appended, its ID and the last position.
    pub(crate) fn expire_leases(&self) -> Result<Vec<(i64, i64)>> {
        let mut conn = self.conn();
        let now = now_ms();
        let due: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM tasks WHERE state = ?1 AND lease_until_ms <= ?2)",
            params![states::CLAIMED, now],
            |r| r.get(0),
        )?;
        if !due {
            return Ok(Vec::new());
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let boards: Vec<BoardRef> = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT b.id, b.name, b.tag FROM boards b JOIN tasks t ON t.board_id = b.id
                 WHERE t.state = ?1 AND t.lease_until_ms <= ?2",
            )?;
            let rows = stmt.query_map(params![states::CLAIMED, now], |r| {
                Ok(BoardRef {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    tag: r.get(2)?,
                })
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut appended = Vec::new();
        for board in boards {
            if let Some(seq) = expire_due(&tx, &board, now)? {
                appended.push((board.id, seq));
            }
        }
        tx.commit()?;
        Ok(appended)
    }

    /// The task holding `id`, with its thread and trust summary.
    pub(crate) fn task_view(&self, board: &BoardRef, id: &str) -> Result<(Thread, Trust)> {
        let root = {
            let conn = self.conn();
            task_of(&conn, board, id)?.id
        };
        let thread = self
            .thread(board, &root)?
            .ok_or(StoreError::ThreadMissing)?;
        let trust = trust(&self.conn(), board, &root)?;
        Ok((thread, trust))
    }

    /// The trust summary of the thread rooted at `root`, if it is a task.
    pub(crate) fn trust_of(
        &self,
        conn: &Connection,
        board: &BoardRef,
        root: &str,
    ) -> Result<Option<Trust>> {
        if row(conn, board, root)?.is_none() {
            return Ok(None);
        }
        trust(conn, board, root).map(Some)
    }

    /// Tasks, oldest first, after creation position `after`.
    pub(crate) fn tasks(&self, board: &BoardRef, query: &TaskQuery) -> Result<TaskPage> {
        let mut values = vec![Value::Integer(board.id)];
        let mut sql = format!(
            "SELECT t.created_seq, th.reply_count, th.last_activity_at, {MESSAGE_COLUMNS}
             FROM tasks t
             JOIN threads th ON th.root_id = t.id
             JOIN messages m ON m.id = t.id
             JOIN actors a ON a.id = m.actor_id
             WHERE t.board_id = ?"
        );
        if let Some(channel) = &query.channel {
            sql.push_str(" AND ");
            sql.push_str(&channel_clause("t.channel", channel, &mut values));
        }
        if !query.states.is_empty() {
            sql.push_str(" AND t.state IN (");
            sql.push_str(&vec!["?"; query.states.len()].join(", "));
            sql.push(')');
            values.extend(query.states.iter().map(|s| Value::Text((*s).to_owned())));
        }
        if let Some(label) = &query.label {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM labels l WHERE l.message_id = t.id AND l.key = ?",
            );
            values.push(Value::Text(label.key.clone()));
            if !label.values.is_empty() {
                sql.push_str(" AND l.value IN (");
                sql.push_str(&vec!["?"; label.values.len()].join(", "));
                sql.push(')');
                values.extend(label.values.iter().cloned().map(Value::Text));
            }
            sql.push(')');
        }
        if let Some(assignee) = &query.assignee {
            sql.push_str(" AND lower(t.assignee_handle) = lower(?)");
            values.push(Value::Text(assignee.clone()));
        }
        if let Some(after) = query.after {
            sql.push_str(" AND t.created_seq > ?");
            values.push(Value::Integer(after));
        }
        sql.push_str(" ORDER BY t.created_seq LIMIT ?");
        values.push(Value::Integer(i64::from(query.limit) + 1));

        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let head = head(&tx, board.id)?;
        let mut stmt = tx.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                ThreadSummary {
                    reply_count: r.get::<_, i64>(1)?.max(0) as u64,
                    last_activity_at: r.get(2)?,
                    root: message_from_row(r, 3, &board.name)?,
                },
            ))
        })?;
        let mut tasks = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let more = tasks.len() > query.limit as usize;
        tasks.truncate(query.limit as usize);
        let mut roots: Vec<&mut Message> = tasks.iter_mut().map(|(_, s)| &mut s.root).collect();
        fill_messages(&tx, &mut roots)?;
        Ok(TaskPage { tasks, more, head })
    }
}

/// The task columns [`fill_tasks`] reads, for messages that are task roots.
pub(super) fn fill_tasks(
    conn: &Connection,
    ids: &str,
    at: &HashMap<String, usize>,
    messages: &mut [&mut Message],
) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "SELECT t.id, t.state, t.assignee_handle, t.epoch, t.lease_until_ms, t.block_kind,
                t.reason, t.result, t.changed_at, t.brief_revision,
                o.id, o.kind, o.handle, o.profile,
                c.id, c.kind, c.handle, c.profile,
                h.id, h.kind, h.handle, h.profile,
                (SELECT json_group_array(n.needs_id) FROM task_needs n WHERE n.task_id = t.id)
         FROM tasks t
         JOIN actors o ON o.id = t.creator_actor
         JOIN actors c ON c.id = t.changed_actor
         LEFT JOIN actors h ON h.id = t.holder_actor
         WHERE t.id IN (SELECT value FROM json_each(?1))",
    )?;
    let mut rows = stmt.query([ids])?;
    while let Some(r) = rows.next()? {
        let id: String = r.get(0)?;
        let Some(index) = at.get(&id) else {
            continue;
        };
        let state: String = r.get(1)?;
        let claimed = state == states::CLAIMED;
        let holder: Option<String> = r.get(18)?;
        let needs: String = r.get(22)?;
        let mut needs: Vec<String> = serde_json::from_str(&needs).unwrap_or_default();
        needs.sort();
        let lease_until: Option<i64> = r.get(4)?;
        messages[*index].task = Some(openboard_api::tasks::Task {
            assignee: r.get(2)?,
            epoch: r.get::<_, i64>(3)?.max(0) as u64,
            lease_until: lease_until.filter(|_| claimed).map(rfc3339_from_ms),
            block_kind: r.get(5)?,
            reason: r.get(6)?,
            result: if state == states::COMPLETED {
                r.get(7)?
            } else {
                None
            },
            changed_at: r.get(8)?,
            brief_revision: r.get(9)?,
            creator: actor_from_row(r, 10)?,
            changed_by: actor_from_row(r, 14)?,
            holder: match (claimed, holder) {
                (true, Some(_)) => Some(actor_from_row(r, 18)?),
                _ => None,
            },
            needs,
            state,
        });
    }
    Ok(())
}
