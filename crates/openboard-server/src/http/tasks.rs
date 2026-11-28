//! The task routes. See `openboard_api::tasks` for the contract.
//!
//! Every handler authorizes through the same seam as the message routes:
//! creating needs `CreateTask` (and `Post`, since a task is a thread),
//! claiming and settling need `ClaimTask`, and a creator's changes need
//! `CreateTask` from the creator or `ManageTasks` from anyone else. Who holds
//! a claim and who created a task are fixed rules the store applies on top.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, PathRejection, QueryRejection};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use openboard_api::tasks::{
    actions, BatchTask, Claim, ClaimReadyRequest, ClaimReadyResponse, CreateTaskRequest,
    CreateTaskResponse, CreateTasksRequest, CreateTasksResponse, NeedRef, TaskActionRequest,
    TaskList, TaskResponse, TaskView, TasksQuery, DEFAULT_TASKS_PER_PAGE, MAX_BLOCK_KIND_LEN,
    MAX_CLAIM_FROM, MAX_CLAIM_LABELS, MAX_LEASE_SECS, MAX_TASKS_PER_BATCH, MAX_TASKS_PER_PAGE,
    MIN_LEASE_SECS,
};
use openboard_api::{codes, MAX_WAIT_SECS};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use super::{
    bad_path, bad_query, invalid_channel, invalid_label, message_too_large, page_size,
    parse_label_filter, read_json, stopping, thread_not_found, ApiResult, AppError, AppState,
    Caller, Identity, Shared, MAX_IDEMPOTENCY_KEY_LEN,
};
use crate::authz::Action;
use crate::chain::hex;
use crate::names::{self, ChannelSel};
use crate::presence::ListenKind;
use crate::store::{
    event_cursor, parse_cursor, Act, AssigneeSpec, BatchEntry, BoardRef, Brief, ClaimOut,
    ClaimProof, CreatorSel, LabelFilter, Need, NewTask, PostTarget, TaskError, TaskQuery,
    MAX_LABELS_PER_MESSAGE,
};
use crate::Mode;

/// The error for a task created or claimed in an intake channel (S4).
fn intake_refused() -> AppError {
    AppError::new(
        codes::INTAKE_CHANNEL,
        "this is an intake channel: tasks cannot be created or claimed here",
    )
    .next(
        "promote the request into a work channel with `openboard tasks create <work-channel> \
         --from <message-id>`",
    )
}

/// The error for a refused task write.
pub(super) fn task_error(error: TaskError) -> AppError {
    match error {
        TaskError::NotATask => AppError::new(codes::NOT_A_TASK, "the thread is not a task")
            .next("make it one with `openboard tasks create <id>`, or list tasks with `openboard tasks`"),
        TaskError::Exists => AppError::new(codes::TASK_EXISTS, "the thread is a task already")
            .next("read it with `openboard tasks show <id>`"),
        TaskError::Claimed => AppError::new(codes::TASK_CLAIMED, "someone holds the task")
            .next("claim another task, or wait for this one's lease to end"),
        TaskError::Assigned => AppError::new(
            codes::TASK_ASSIGNED,
            "the task is kept for someone else",
        )
        .next("claim a task kept for you or for anyone; the creator can return it with `tasks assign <id> --pool`"),
        TaskError::Waiting => AppError::new(
            codes::TASK_WAITING,
            "the task needs tasks that have not completed",
        )
        .next("claim it once what it needs has completed; `openboard tasks show <id>` lists them"),
        TaskError::Blocked => AppError::new(codes::TASK_BLOCKED, "the task is blocked")
            .next("its creator unblocks it with `openboard tasks unblock <id>`"),
        TaskError::Finished => AppError::new(
            codes::TASK_FINISHED,
            "the task is completed, failed or canceled",
        )
        .next("a failed task can be reopened by its creator with `openboard tasks reopen <id>`"),
        TaskError::State(state) => AppError::new(
            codes::TASK_STATE,
            format!("the task is {state}, and that change needs another state"),
        )
        .next("read the task's state with `openboard tasks show <id>`"),
        TaskError::NotCreator => AppError::new(
            codes::NOT_TASK_CREATOR,
            "only the task's creator may do this",
        )
        .next("use the login and --as profile that created the task"),
        TaskError::ClaimLost => AppError::new(
            codes::CLAIM_LOST,
            "the claim has ended: its lease ran out, it was released or settled, or the task was claimed again",
        )
        .next("nothing was changed; claim a task again before working on it"),
        TaskError::ClaimNotHeld => AppError::new(
            codes::CLAIM_NOT_HELD,
            "the caller does not hold this claim",
        )
        .next("use the login and --as profile that claimed the task, with its epoch and token"),
        TaskError::InvalidNeeds(detail) => AppError::new(
            codes::INVALID_NEEDS,
            format!("the task's needs are not valid: {detail}"),
        )
        .next("name tasks on this board, without cycles; in a batch, use {\"key\": ...} for entries of the same batch"),
        TaskError::InvalidAssignee => AppError::new(
            codes::INVALID_ASSIGNEE,
            "the assignee names nobody on this board",
        )
        .next("use a profile name such as `worker-1`, or `@name` or `@name/profile` for an enrolled agent or person"),
    }
}

/// What `TaskActionRequest::assignee` and friends hold, parsed.
fn parse_assignee(raw: &str) -> ApiResult<AssigneeSpec> {
    let raw = raw.trim();
    let invalid = || {
        AppError::new(
            codes::INVALID_ASSIGNEE,
            "an assignee is a profile name, `@name` or `@name/profile`",
        )
    };
    let (explicit, rest) = match raw.strip_prefix('@') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    if rest.is_empty()
        || rest.len() > 255
        || rest.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(invalid());
    }
    match rest.rsplit_once('/') {
        Some((handle, profile)) if !handle.is_empty() => {
            names::validate_profile(profile).map_err(|_| invalid())?;
            Ok(AssigneeSpec::Handle {
                handle: handle.to_owned(),
                profile: Some(profile.to_owned()),
            })
        }
        Some(_) => Err(invalid()),
        None if explicit => Ok(AssigneeSpec::Handle {
            handle: rest.to_owned(),
            profile: None,
        }),
        None => {
            names::validate_profile(rest).map_err(|_| invalid())?;
            Ok(AssigneeSpec::Profile(rest.to_owned()))
        }
    }
}

/// A creator named in a ready claim's `from`.
fn parse_creator(raw: &str) -> ApiResult<CreatorSel> {
    let raw = raw.trim();
    if raw.starts_with("act_") {
        return Ok(CreatorSel::Actor(raw.to_owned()));
    }
    match parse_assignee(&format!("@{}", raw.trim_start_matches('@'))) {
        Ok(AssigneeSpec::Handle { handle, profile }) => Ok(CreatorSel::Handle { handle, profile }),
        _ => Err(AppError::invalid(
            "a creator in `from` is an actor ID, `name` or `name/profile`",
        )),
    }
}

fn lease_ms(lease_secs: Option<u64>) -> ApiResult<Option<i64>> {
    match lease_secs {
        None => Ok(None),
        Some(secs) if (MIN_LEASE_SECS..=MAX_LEASE_SECS).contains(&secs) => {
            Ok(Some(secs as i64 * 1000))
        }
        Some(_) => Err(AppError::invalid(format!(
            "a lease is {MIN_LEASE_SECS} to {MAX_LEASE_SECS} seconds"
        ))),
    }
}

fn check_labels(labels: &BTreeMap<String, String>) -> ApiResult<()> {
    for (key, value) in labels {
        names::validate_label_key(key).map_err(invalid_label)?;
        names::validate_label_value(value).map_err(invalid_label)?;
    }
    // One place is kept for the task's state.
    if labels.len() >= MAX_LABELS_PER_MESSAGE {
        return Err(invalid_label(format!(
            "a task holds at most {} labels of its own",
            MAX_LABELS_PER_MESSAGE - 1
        )));
    }
    Ok(())
}

fn check_key(key: &Option<String>) -> ApiResult<()> {
    if key
        .as_ref()
        .is_some_and(|key| key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_LEN)
    {
        return Err(AppError::invalid("an idempotency key is 1 to 255 bytes"));
    }
    Ok(())
}

/// A free-text field of a task change: a reason or a result.
fn check_note(state: &AppState, what: &str, text: &str) -> ApiResult<()> {
    if text.trim().is_empty() {
        return Err(AppError::invalid(format!("the {what} is empty")));
    }
    let (chars, limit) = (text.chars().count(), state.max_message_chars);
    if chars > limit {
        return Err(message_too_large(format!(
            "the {what} has {chars} characters and the limit is {limit}"
        )));
    }
    Ok(())
}

/// A digest of a create request without its key, to tell a retry from a
/// reuse of the key.
fn fingerprint<T: serde::Serialize>(domain: &str, request: &T) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update(b"\n");
    hasher.update(serde_json::to_vec(request).unwrap_or_default());
    hex(&hasher.finalize())
}

fn claim_view(board: &BoardRef, claim: ClaimOut, head: i64) -> Claim {
    Claim {
        task: claim.message,
        epoch: claim.epoch,
        token: claim.token,
        until: crate::time::rfc3339_from_ms(claim.until_ms),
        lease_secs: (claim.lease_ms / 1000).max(0) as u64,
        trust: claim.trust,
        cursor: event_cursor(&board.tag, claim.seq.unwrap_or(head)),
    }
}

impl AppState {
    fn local(&self) -> bool {
        self.mode == Mode::Local
    }

    /// The board a new task goes to, created with it if it does not exist,
    /// once the caller may post and create a task in `channel` there.
    async fn task_target(
        self: &std::sync::Arc<Self>,
        caller: &Caller,
        board_name: &str,
        channel: &str,
    ) -> ApiResult<PostTarget> {
        names::validate_channel(channel).map_err(invalid_channel)?;
        match self.find_board(board_name).await? {
            Some(board) => {
                self.seen(caller, &board);
                self.require(caller, Action::Post, &board.name, Some(channel))?;
                self.require(caller, Action::CreateTask, &board.name, Some(channel))?;
                match self.intake(board.id, channel).await? {
                    None => {
                        self.require(caller, Action::CreateChannel, &board.name, Some(channel))?
                    }
                    Some(true) => return Err(intake_refused()),
                    Some(false) => {}
                }
                Ok(PostTarget::Board(board))
            }
            None => {
                self.require(caller, Action::CreateBoard, board_name, None)?;
                self.require(caller, Action::Post, board_name, Some(channel))?;
                self.require(caller, Action::CreateTask, board_name, Some(channel))?;
                self.require(caller, Action::CreateChannel, board_name, Some(channel))?;
                Ok(PostTarget::NewBoard(board_name.to_owned()))
            }
        }
    }

    /// Whether a channel is an intake channel; `None` when it does not exist.
    async fn intake(
        self: &std::sync::Arc<Self>,
        board_id: i64,
        channel: &str,
    ) -> ApiResult<Option<bool>> {
        let path = channel.to_owned();
        self.db(move |state| state.store.channel_intake(board_id, &path))
            .await
    }

    /// Refuses prerequisites in channels the caller may not read, with the
    /// error for a task that does not exist (Z15). The store checks the rest.
    async fn readable_needs(
        self: &std::sync::Arc<Self>,
        caller: &Caller,
        board: &BoardRef,
        ids: Vec<String>,
    ) -> ApiResult<()> {
        if self.mode != Mode::Network || ids.is_empty() {
            return Ok(());
        }
        let board_id = board.id;
        let places = self
            .db(move |state| {
                ids.into_iter()
                    .map(|id| Ok((state.store.thread_of(board_id, &id)?, id)))
                    .collect::<Result<Vec<_>, crate::store::StoreError>>()
            })
            .await?;
        for (place, id) in places {
            if let Some((_, channel)) = place {
                if !self.allows(caller, Action::Read, &board.name, Some(&channel)) {
                    let shown: String = id.chars().take(40).collect();
                    return Err(task_error(TaskError::InvalidNeeds(format!(
                        "`{shown}` is not a task on this board"
                    ))));
                }
            }
        }
        Ok(())
    }

    /// The board and the channel of the thread holding `id`, once the caller
    /// may read it. Anything it may not read is reported as missing.
    async fn task_place(
        self: &std::sync::Arc<Self>,
        caller: &Caller,
        board_name: &str,
        id: &str,
    ) -> ApiResult<(BoardRef, String)> {
        let board = self.board_named(caller, board_name).await?;
        // As in `message_target`: an unreadable board answers the same
        // whether it exists or not.
        self.require(caller, Action::Read, &board.name, None)?;
        let (board_id, id) = (board.id, id.to_owned());
        let (_root, channel) = self
            .db(move |state| state.store.thread_of(board_id, &id))
            .await?
            .filter(|(_, channel)| self.allows(caller, Action::Read, &board.name, Some(channel)))
            .ok_or_else(thread_not_found)?;
        Ok((board, channel))
    }
}

/// `POST /v1/boards/{board}/tasks`.
pub(super) async fn create_task(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<(StatusCode, Json<CreateTaskResponse>)> {
    let caller = state.caller(&identity, &headers).await?;
    let Path(board_name) = board.map_err(|_| bad_path())?;
    names::validate_board(&board_name)
        .map_err(|reason| AppError::new(codes::INVALID_BOARD, reason))?;
    let limit = state.max_message_chars;
    let mut request: CreateTaskRequest = read_json(&headers, body, "task request", || {
        message_too_large(format!(
            "the request body is too large for a brief of at most {limit} characters"
        ))
    })?;
    check_labels(&request.labels)?;
    check_key(&request.idempotency_key)?;
    let assignee = request
        .assignee
        .as_deref()
        .map(parse_assignee)
        .transpose()?;
    let lease = lease_ms(request.lease_secs)?;
    let claim = request
        .claim
        .then(|| lease.unwrap_or(openboard_api::tasks::DEFAULT_LEASE_SECS as i64 * 1000));
    let idempotency = request.idempotency_key.clone().map(|key| {
        let mut unkeyed = request.clone();
        unkeyed.idempotency_key = None;
        (key, fingerprint("openboard.task.v1", &unkeyed))
    });

    let (target, brief, channel) = match (&request.channel, &request.text, &request.thread_id) {
        (Some(channel), Some(text), None) => {
            state.check_text(text)?;
            let target = state.task_target(&caller, &board_name, channel).await?;
            (
                target,
                Brief::New {
                    channel: channel.clone(),
                    text: text.clone(),
                    mentions: Default::default(),
                },
                channel.clone(),
            )
        }
        (None, None, Some(thread_id)) => {
            let (board, channel) = state.task_place(&caller, &board_name, thread_id).await?;
            state.require(&caller, Action::CreateTask, &board.name, Some(&channel))?;
            if state.intake(board.id, &channel).await? == Some(true) {
                return Err(intake_refused());
            }
            if !request.labels.is_empty() {
                state.require(&caller, Action::Label, &board.name, Some(&channel))?;
            }
            (
                PostTarget::Board(board),
                Brief::Thread(thread_id.clone()),
                channel,
            )
        }
        _ => return Err(AppError::invalid(
            "set `channel` and `text` for a new task, or `thread_id` alone to make a thread a task",
        )),
    };
    if let PostTarget::Board(board) = &target {
        if claim.is_some() {
            state.require(&caller, Action::ClaimTask, &board.name, Some(&channel))?;
        }
        state
            .readable_needs(&caller, board, request.needs.clone())
            .await?;
    }
    // Promoting a request (S4): the task records where it came from. The
    // caller must read the request, and needs task.create where the task
    // goes, which was checked above.
    if let Some(origin) = request.origin.clone() {
        let PostTarget::Board(board) = &target else {
            return Err(thread_not_found());
        };
        if request.thread_id.is_some() {
            return Err(AppError::invalid(
                "`origin` promotes a request into a new task: give `channel` and `text`",
            ));
        }
        let (_, from) = state.task_place(&caller, &board.name, &origin).await?;
        if state.intake(board.id, &from).await? != Some(true) {
            return Err(AppError::invalid(
                "`origin` names a message in an intake channel; this one is in a work channel",
            )
            .next("make the thread itself a task with `openboard tasks create <id>`"));
        }
        if request.labels.len() + 1 >= MAX_LABELS_PER_MESSAGE {
            return Err(invalid_label(
                "a promoted task keeps one label for its origin",
            ));
        }
        request
            .labels
            .insert(openboard_api::grants::ORIGIN_LABEL.to_owned(), origin);
    }

    let by = caller.writer();
    let local = state.local();
    let created = state
        .db(move |state| {
            let brief = match brief {
                Brief::New { channel, text, .. } => Brief::New {
                    mentions: state.mentions_in(&text),
                    channel,
                    text,
                },
                other => other,
            };
            let task = NewTask {
                brief,
                labels: request.labels,
                needs: request.needs,
                assignee,
                claim,
                idempotency,
            };
            let created = state.store.create_task(target, &by, task, local)?;
            state.hub.publish(created.board.id, created.seq);
            Ok(created)
        })
        .await?;
    let status = if created.duplicate {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let board = created.board;
    Ok((
        status,
        Json(CreateTaskResponse {
            task: created.message,
            cursor: event_cursor(&board.tag, created.seq),
            claim: created
                .claim
                .map(|claim| claim_view(&board, claim, created.seq)),
            duplicate: created.duplicate,
        }),
    ))
}

/// `POST /v1/boards/{board}/tasks/batch`.
pub(super) async fn create_tasks(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<(StatusCode, Json<CreateTasksResponse>)> {
    let caller = state.caller(&identity, &headers).await?;
    let Path(board_name) = board.map_err(|_| bad_path())?;
    names::validate_board(&board_name)
        .map_err(|reason| AppError::new(codes::INVALID_BOARD, reason))?;
    let request: CreateTasksRequest = read_json(&headers, body, "batch of tasks", || {
        AppError::new(
            codes::BATCH_TOO_LARGE,
            "the request body is too large for a batch of tasks",
        )
        .status(StatusCode::PAYLOAD_TOO_LARGE)
    })?;
    if request.tasks.is_empty() {
        return Err(AppError::invalid("a batch holds at least one task"));
    }
    if request.tasks.len() > MAX_TASKS_PER_BATCH {
        return Err(AppError::new(
            codes::BATCH_TOO_LARGE,
            format!(
                "a batch holds at most {MAX_TASKS_PER_BATCH} tasks and this one has {}",
                request.tasks.len()
            ),
        )
        .next("split the plan into several batches; a later batch can need tasks an earlier one created, by ID"));
    }
    check_key(&request.idempotency_key)?;
    let mut entries = Vec::with_capacity(request.tasks.len());
    for BatchTask {
        key,
        text,
        labels,
        needs,
        assignee,
    } in &request.tasks
    {
        state.check_text(text)?;
        check_labels(labels)?;
        if key.as_ref().is_some_and(|key| {
            key.is_empty() || key.len() > 64 || key.chars().any(char::is_control)
        }) {
            return Err(AppError::invalid("a batch key is 1 to 64 characters"));
        }
        entries.push(BatchEntry {
            key: key.clone(),
            text: text.clone(),
            labels: labels.clone(),
            needs: needs
                .iter()
                .map(|need| match need {
                    NeedRef::Key { key } => Need::Key(key.clone()),
                    NeedRef::Id(id) => Need::Id(id.clone()),
                })
                .collect(),
            assignee: assignee.as_deref().map(parse_assignee).transpose()?,
            mentions: Default::default(),
        });
    }
    let target = state
        .task_target(&caller, &board_name, &request.channel)
        .await?;
    if let PostTarget::Board(board) = &target {
        let ids = entries
            .iter()
            .flat_map(|entry| entry.needs.iter())
            .filter_map(|need| match need {
                Need::Id(id) => Some(id.clone()),
                Need::Key(_) => None,
            })
            .collect();
        state.readable_needs(&caller, board, ids).await?;
    }
    let idempotency = request.idempotency_key.clone().map(|key| {
        let mut unkeyed = request.clone();
        unkeyed.idempotency_key = None;
        (key, fingerprint("openboard.tasks.v1", &unkeyed))
    });
    let by = caller.writer();
    let local = state.local();
    let channel = request.channel;
    let created = state
        .db(move |state| {
            for entry in &mut entries {
                entry.mentions = state.mentions_in(&entry.text);
            }
            let created =
                state
                    .store
                    .create_tasks(target, &by, &channel, entries, idempotency, local)?;
            state.hub.publish(created.board.id, created.seq);
            Ok(created)
        })
        .await?;
    let status = if created.duplicate {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((
        status,
        Json(CreateTasksResponse {
            tasks: created.messages,
            cursor: event_cursor(&created.board.tag, created.seq),
            duplicate: created.duplicate,
        }),
    ))
}

/// `GET /v1/boards/{board}/tasks`.
pub(super) async fn list_tasks(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    query: Result<Query<TasksQuery>, QueryRejection>,
) -> ApiResult<Json<TaskList>> {
    use openboard_api::tasks::states;
    let caller = state.caller(&identity, &headers).await?;
    let board = state.board(&caller, board).await?;
    let Query(query) = query.map_err(bad_query)?;
    let channel = match &query.channel {
        None => None,
        Some(raw) => Some(names::parse_channel_sel(raw).map_err(invalid_channel)?),
    };
    let exact = match &channel {
        Some(ChannelSel::Exact(path)) => Some(path.as_str()),
        _ => None,
    };
    state.require(&caller, Action::Read, &board.name, exact)?;
    let wanted: Vec<&'static str> = match query.state.as_deref().map(str::trim) {
        None | Some("") | Some("open") => states::OPEN.to_vec(),
        Some("all") => Vec::new(),
        Some(raw) => raw
            .split(',')
            .map(|name| {
                let name = name.trim();
                if name == "open" {
                    return Ok(states::OPEN.to_vec());
                }
                states::ALL
                    .iter()
                    .find(|known| **known == name)
                    .map(|known| vec![*known])
                    .ok_or_else(|| {
                        AppError::invalid("`state` names a state tasks do not have").next(format!(
                            "use `open`, `all` or names among {}",
                            states::ALL.join(", ")
                        ))
                    })
            })
            .collect::<ApiResult<Vec<_>>>()?
            .concat(),
    };
    let label = query.label.as_deref().map(parse_label_filter).transpose()?;
    let limit = page_size(query.limit, DEFAULT_TASKS_PER_PAGE, MAX_TASKS_PER_PAGE)?;
    let after = match &query.cursor {
        None => None,
        Some(cursor) => {
            Some(parse_cursor("tk", &board.tag, cursor).ok_or_else(AppError::invalid_cursor)?)
        }
    };
    let task_query = TaskQuery {
        channel,
        states: wanted,
        label,
        assignee: query.assignee.clone(),
        after,
        limit,
    };
    let listed_on = board.clone();
    let page = state
        .db(move |state| state.store.tasks(&listed_on, &task_query))
        .await?;
    let next_cursor = match page.tasks.last() {
        Some((seq, _)) if page.more => Some(format!("tk_{}_{seq:x}", board.tag)),
        _ => None,
    };
    let tasks = page
        .tasks
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
    Ok(Json(TaskList {
        tasks,
        next_cursor,
        cursor: event_cursor(&board.tag, page.head),
    }))
}

/// `GET /v1/boards/{board}/tasks/{task_id}`.
pub(super) async fn get_task(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> ApiResult<Json<TaskView>> {
    let caller = state.caller(&identity, &headers).await?;
    let Path((board_name, id)) = path.map_err(|_| bad_path())?;
    let (board, _channel) = state.task_place(&caller, &board_name, &id).await?;
    let (thread, trust) = state
        .db(move |state| state.store.task_view(&board, &id))
        .await?;
    Ok(Json(TaskView {
        task: thread.root,
        replies: thread.replies,
        trust,
    }))
}

/// `POST /v1/boards/{board}/tasks/claim`.
pub(super) async fn claim_ready(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<Json<ClaimReadyResponse>> {
    let mut caller = state.caller(&identity, &headers).await?;
    let board = state.board(&caller, board).await?;
    let request: ClaimReadyRequest = read_json(&headers, body, "claim request", || {
        AppError::invalid("the request body is too large for a claim request")
            .status(StatusCode::PAYLOAD_TOO_LARGE)
    })?;
    let channel = names::parse_channel_sel(&request.channel).map_err(invalid_channel)?;
    let exact = match &channel {
        ChannelSel::Exact(path) => Some(path.as_str()),
        ChannelSel::Subtree(_) => None,
    };
    state.require(&caller, Action::Read, &board.name, exact)?;
    state.require(&caller, Action::ClaimTask, &board.name, exact)?;
    if request.labels.len() > MAX_CLAIM_LABELS {
        return Err(AppError::invalid(format!(
            "a claim takes at most {MAX_CLAIM_LABELS} label conditions"
        )));
    }
    if request.from.len() > MAX_CLAIM_FROM {
        return Err(AppError::invalid(format!(
            "a claim names at most {MAX_CLAIM_FROM} creators"
        )));
    }
    let labels: Vec<LabelFilter> = request
        .labels
        .iter()
        .map(|raw| parse_label_filter(raw))
        .collect::<ApiResult<_>>()?;
    let from: Vec<CreatorSel> = request
        .from
        .iter()
        .map(|raw| parse_creator(raw))
        .collect::<ApiResult<_>>()?;
    let lease = lease_ms(request.lease_secs)?;
    let wait = Duration::from_secs(request.wait.unwrap_or(0).min(MAX_WAIT_SECS));
    let deadline = Instant::now() + wait;
    let (_place, _listening) = if wait.is_zero() {
        (None, None)
    } else {
        (
            state.waiter(&caller)?,
            state.listening(
                &caller,
                &board,
                Some(request.channel.clone()),
                ListenKind::ClaimWait,
            ),
        )
    };
    let mut shutdown = state.shutdown.clone();
    let mut wake = state.hub.subscribe(board.id);
    let by = caller.writer();
    let local = state.local();
    let mut first = true;
    loop {
        if !first {
            // A credential that stopped being usable while the request was
            // held open claims nothing.
            if let Some(id) = caller.credential_id.clone() {
                let usable = state
                    .db(move |state| state.store.credential_usable(&id))
                    .await?;
                if !usable {
                    return Err(AppError::unauthenticated());
                }
            }
            // And with the grants as they are now (Z14).
            state.refresh(&mut caller).await?;
            state.require(&caller, Action::ClaimTask, &board.name, exact)?;
        }
        first = false;
        wake.borrow_and_update();
        let (claim_on, by_now) = (board.clone(), by.clone());
        let (channel_now, labels_now, from_now) = (channel.clone(), labels.clone(), from.clone());
        let (claim_state, claim_caller) = (state.clone(), caller.clone());
        let claimed = state
            .db(move |state| {
                let can_claim = |channel: &str| {
                    claim_state.allows(
                        &claim_caller,
                        Action::ClaimTask,
                        &claim_on.name,
                        Some(channel),
                    )
                };
                let claimed = state.store.claim_ready(
                    &claim_on,
                    &by_now,
                    &channel_now,
                    &labels_now,
                    &from_now,
                    lease,
                    &can_claim,
                    local,
                )?;
                let head = state.store.head(claim_on.id)?;
                if let Some(seq) = claimed.as_ref().and_then(|claim| claim.seq) {
                    state.hub.publish(claim_on.id, seq);
                }
                Ok((claimed, head))
            })
            .await?;
        if let (Some(claim), head) = claimed {
            return Ok(Json(ClaimReadyResponse {
                claim: Some(claim_view(&board, claim, head)),
            }));
        }
        if Instant::now() >= deadline {
            return Ok(Json(ClaimReadyResponse { claim: None }));
        }
        tokio::select! {
            changed = wake.changed() => if changed.is_err() {
                return Ok(Json(ClaimReadyResponse { claim: None }));
            },
            () = tokio::time::sleep_until(deadline) => {}
            () = stopping(&mut shutdown) => return Ok(Json(ClaimReadyResponse { claim: None })),
        }
    }
}

/// `POST /v1/boards/{board}/tasks/{task_id}/{action}`.
pub(super) async fn task_action(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    path: Result<Path<(String, String, String)>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    let caller = state.caller(&identity, &headers).await?;
    let Path((board_name, id, action)) = path.map_err(|_| bad_path())?;
    let limit = state.max_message_chars;
    let request: TaskActionRequest = if body.as_ref().is_ok_and(Bytes::is_empty) {
        TaskActionRequest::default()
    } else {
        read_json(&headers, body, "task change", || {
            message_too_large(format!(
                "the request body is too large for a note of at most {limit} characters"
            ))
        })?
    };
    let known = [
        actions::CLAIM,
        actions::RENEW,
        actions::RELEASE,
        actions::COMPLETE,
        actions::FAIL,
        actions::BLOCK,
        actions::UNBLOCK,
        actions::REOPEN,
        actions::ASSIGN,
        actions::NEEDS,
        actions::CANCEL,
    ];
    if !known.contains(&action.as_str()) {
        return Err(AppError::new(codes::NOT_FOUND, "no such task action")
            .next(format!("use one of: {}", known.join(", "))));
    }
    let lease = lease_ms(request.lease_secs)?;
    let proof = match (request.epoch, request.token.clone()) {
        (Some(epoch), Some(token)) => Some(ClaimProof { epoch, token }),
        (None, None) => None,
        _ => {
            return Err(AppError::invalid(
                "a claim is given as both `epoch` and `token`",
            ))
        }
    };
    let need_proof = || {
        proof.clone().ok_or_else(|| {
            AppError::invalid("this change needs the claim: `epoch` and `token`").next(
                "claim the task first; `openboard tasks claim` keeps the claim for later commands",
            )
        })
    };
    let reason = |what: &str| -> ApiResult<String> {
        let text = request.reason.clone().unwrap_or_default();
        check_note(&state, what, &text)?;
        Ok(text)
    };

    let (board, channel) = state.task_place(&caller, &board_name, &id).await?;
    let claiming = matches!(
        action.as_str(),
        actions::CLAIM | actions::RENEW | actions::RELEASE | actions::COMPLETE | actions::FAIL
    ) || (action == actions::BLOCK && proof.is_some());
    if claiming {
        state.require(&caller, Action::ClaimTask, &board.name, Some(&channel))?;
        if action == actions::CLAIM && state.intake(board.id, &channel).await? == Some(true) {
            return Err(intake_refused());
        }
    } else if !state.allows(&caller, Action::CreateTask, &board.name, Some(&channel))
        && !state.allows(&caller, Action::ManageTasks, &board.name, Some(&channel))
    {
        return Err(AppError::forbidden());
    }
    let by = caller.writer();
    let local = state.local();

    if action == actions::CLAIM {
        let on = board.clone();
        let (claim, head) = state
            .db(move |state| {
                let claim = state.store.claim_task(&on, &by, &id, lease)?;
                if let Some(seq) = claim.seq {
                    state.hub.publish(on.id, seq);
                }
                Ok((claim, state.store.head(on.id)?))
            })
            .await?;
        return Ok(Json(claim_view(&board, claim, head)).into_response());
    }

    let act = match action.as_str() {
        actions::RENEW => Act::Renew(need_proof()?, lease),
        actions::RELEASE => Act::Release(need_proof()?),
        actions::COMPLETE => {
            if let Some(result) = &request.result {
                check_note(&state, "result", result)?;
            }
            Act::Complete(need_proof()?, request.result.clone())
        }
        actions::FAIL => Act::Fail(need_proof()?, reason("reason")?),
        actions::BLOCK => {
            let kind = request.kind.clone();
            if let Some(kind) = &kind {
                let ok = !kind.is_empty()
                    && kind.len() <= MAX_BLOCK_KIND_LEN
                    && kind.bytes().all(|b| {
                        b.is_ascii_lowercase()
                            || b.is_ascii_digit()
                            || matches!(b, b'_' | b'-' | b'.')
                    });
                if !ok {
                    return Err(AppError::invalid(format!(
                        "a block kind is 1 to {MAX_BLOCK_KIND_LEN} lowercase letters, digits, '_', '-' and '.'"
                    )));
                }
            }
            Act::Block {
                claim: proof.clone(),
                reason: reason("reason")?,
                kind,
            }
        }
        actions::UNBLOCK => Act::Unblock,
        actions::REOPEN => Act::Reopen,
        actions::ASSIGN => match (&request.assignee, request.pool) {
            (Some(raw), false) => Act::Assign(Some(parse_assignee(raw)?)),
            (None, true) => Act::Assign(None),
            _ => return Err(AppError::invalid(
                "set `assignee` to keep the task for someone, or `pool` to return it to everyone",
            )),
        },
        actions::NEEDS => match &request.needs {
            Some(needs) => {
                state.readable_needs(&caller, &board, needs.clone()).await?;
                Act::Needs(needs.clone())
            }
            None => return Err(AppError::invalid("set `needs`; an empty list clears it")),
        },
        actions::CANCEL => Act::Cancel(reason("reason")?),
        _ => return Err(AppError::internal(&"an unknown task action got through")),
    };

    let manage_state = state.clone();
    let manage_caller = caller.clone();
    let on = board.clone();
    let (changed, head) = state
        .db(move |state| {
            let manage = |channel: &str| {
                manage_state.allows(&manage_caller, Action::ManageTasks, &on.name, Some(channel))
            };
            let changed = state.store.task_act(&on, &by, &id, act, &manage, local)?;
            if let Some(seq) = changed.seq {
                state.hub.publish(on.id, seq);
            }
            Ok((changed, state.store.head(on.id)?))
        })
        .await?;
    if let Some(claim) = changed.claim {
        return Ok(Json(claim_view(&board, claim, head)).into_response());
    }
    Ok(Json(TaskResponse {
        task: changed.message,
        cursor: event_cursor(&board.tag, changed.seq.unwrap_or(head)),
    })
    .into_response())
}
