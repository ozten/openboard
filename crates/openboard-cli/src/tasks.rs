//! `openboard tasks`: creating, listing, claiming and settling tasks, and
//! `tasks run`, which claims a task, runs a command and settles the task by
//! the command's exit code.
//!
//! A claim's epoch and token are kept in the configuration directory, one
//! private file per board, login, profile and task, so that later commands
//! need only the task's ID. `tasks run` hands them to the command it runs
//! in `BOARD_TASK_EPOCH` and `BOARD_TASK_TOKEN` as well.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use openboard_api::tasks::{
    actions, states, BatchTask, Claim, ClaimReadyRequest, CreateTaskRequest, CreateTasksRequest,
    NeedRef, TaskActionRequest, TaskResponse, TaskView, TasksQuery, MAX_TASKS_PER_BATCH,
};
use openboard_api::{codes as api_codes, Message, MAX_WAIT_SECS};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::cli::{
    self, ReadyArgs, TaskCreateArgs, TaskRunArgs, TasksArgs, TasksCommand, TasksListArgs,
};
use crate::error::{codes, Failure};
use crate::logins;
use crate::{render, shell_quote, signalled, write_stdout, Ctx, Sig};

/// The environment `tasks run` gives the command it runs.
pub const TASK_ENV: &str = "BOARD_TASK";
pub const EPOCH_ENV: &str = "BOARD_TASK_EPOCH";
pub const TOKEN_ENV: &str = "BOARD_TASK_TOKEN";

/// What the input of `tasks create` is, read before the runtime starts.
pub enum CreateInput {
    None,
    Text(String),
    Plan(String),
}

/// Whether `raw` is shaped like a message ID the board issues: `msg_` and
/// 24 lowercase hex digits. A channel could only be spelled so on purpose.
pub fn is_message_id(raw: &str) -> bool {
    raw.strip_prefix("msg_").is_some_and(|hex| {
        hex.len() == 24
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Reads the brief or the plan of `tasks create` from its argument, a file
/// or stdin, before the async runtime starts.
pub fn create_input(args: &TaskCreateArgs) -> Result<CreateInput, Failure> {
    if let Some(path) = &args.file {
        let mut text = String::new();
        let read = if path == "-" {
            std::io::stdin().read_to_string(&mut text)
        } else {
            std::fs::File::open(path).and_then(|mut file| file.read_to_string(&mut text))
        };
        read.map_err(|error| {
            Failure::new(codes::IO, format!("cannot read the plan {path}: {error}"))
                .next("pass --file with a JSON file, or `-` to read it from stdin")
        })?;
        return Ok(CreateInput::Plan(text));
    }
    if is_message_id(&args.target) {
        if args.text.is_some() {
            return Err(Failure::usage(
                "`tasks create <message-id>` makes an existing thread a task and takes no text",
            )
            .next(
                "to start a new task, pass a channel: `openboard tasks create <channel> <text>`",
            ));
        }
        return Ok(CreateInput::None);
    }
    crate::message_text(args.text.as_deref(), "tasks create <channel>").map(CreateInput::Text)
}

// ---------------------------------------------------------------------------
// Kept claims
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Kept {
    task: String,
    epoch: u64,
    token: String,
    until: String,
    lease_secs: u64,
}

impl Ctx<'_> {
    /// The file that keeps this caller's claim on `task`.
    fn claim_file(&self, task: &str) -> Result<PathBuf, Failure> {
        let mut hasher = Sha256::new();
        for part in [
            self.target.url.as_str(),
            self.global.board(),
            self.target.principal.as_deref().unwrap_or("-"),
            self.global.profile().unwrap_or("-"),
            task,
        ] {
            hasher.update(part.as_bytes());
            hasher.update(b"\n");
        }
        let digest: String = hasher
            .finalize()
            .iter()
            .take(16)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(logins::config_dir()?
            .join("claims")
            .join(format!("{digest}.json")))
    }

    fn keep_claim(&self, claim: &Claim) -> Result<(), Failure> {
        let path = self.claim_file(&claim.task.id)?;
        let dir = path.parent().map(PathBuf::from).unwrap_or_default();
        logins::create_private_dir(&dir)?;
        let kept = Kept {
            task: claim.task.id.clone(),
            epoch: claim.epoch,
            token: claim.token.clone(),
            until: claim.until.clone(),
            lease_secs: claim.lease_secs,
        };
        let body = serde_json::to_vec(&kept)
            .map_err(|error| Failure::new(codes::IO, format!("cannot encode a claim: {error}")))?;
        let temp = path.with_extension(format!("tmp{}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        logins::private(&mut options);
        let mut file = options
            .open(&temp)
            .map_err(|error| logins::io_failure("write", &temp, &error))?;
        file.write_all(&body)
            .and_then(|()| file.sync_all())
            .map_err(|error| logins::io_failure("write", &temp, &error))?;
        std::fs::rename(&temp, &path).map_err(|error| logins::io_failure("write", &path, &error))
    }

    fn forget_claim(&self, task: &str) {
        if let Ok(path) = self.claim_file(task) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// The claim this caller holds on `id`: from `tasks run`'s environment
    /// when it names this task, else from the kept file.
    fn held(&self, id: &str) -> Result<(u64, String), Failure> {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        if env(TASK_ENV).as_deref() == Some(id) {
            if let (Some(epoch), Some(token)) = (env(EPOCH_ENV), env(TOKEN_ENV)) {
                if let Ok(epoch) = epoch.trim().parse() {
                    return Ok((epoch, token.trim().to_owned()));
                }
            }
        }
        let path = self.claim_file(id)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(not_held(id, self.global.profile()))
            }
            Err(error) => return Err(logins::io_failure("read", &path, &error)),
        };
        let kept: Kept =
            serde_json::from_str(&text).map_err(|_| not_held(id, self.global.profile()))?;
        Ok((kept.epoch, kept.token))
    }
}

fn not_held(id: &str, profile: Option<&str>) -> Failure {
    let as_flag = profile.map_or_else(String::new, |p| format!(" --as {}", shell_quote(p)));
    Failure::new(
        api_codes::CLAIM_NOT_HELD,
        format!(
            "no claim on task {} is kept here for this board, login and profile",
            render::safe(id)
        ),
    )
    .next(format!(
        "claim it first with `openboard{as_flag} tasks claim {}`, under the same --as profile \
         that will settle it",
        safe_id(id)
    ))
}

/// An ID repeated in a hint, or a placeholder when it does not look like one.
fn safe_id(id: &str) -> &str {
    let plain = !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        id
    } else {
        "<task-id>"
    }
}

fn lease_secs(lease: Option<Duration>) -> Option<u64> {
    lease.map(|lease| lease.as_secs().max(1))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub async fn run(ctx: &Ctx<'_>, args: &TasksArgs, input: CreateInput) -> Result<(), Failure> {
    let id = |raw: &str| crate::message_id(raw).map(str::to_owned);
    match &args.command {
        None => list(ctx, &args.list).await,
        Some(TasksCommand::List(list_args)) => list(ctx, list_args).await,
        Some(TasksCommand::Create(create_args)) => create(ctx, create_args, input).await,
        Some(TasksCommand::Show(a)) => {
            let view = ctx.client.task(&id(&a.id)?).await?;
            ctx.print(&view, || show(&view))
        }
        Some(TasksCommand::Claim(a)) => {
            let claim = match &a.id {
                Some(raw) => {
                    let request = TaskActionRequest {
                        lease_secs: lease_secs(a.lease),
                        ..TaskActionRequest::default()
                    };
                    ctx.client
                        .task_action::<Claim>(&id(raw)?, actions::CLAIM, &request)
                        .await?
                }
                None => claim_ready(ctx, &a.ready, a.lease, None).await?,
            };
            ctx.keep_claim(&claim)?;
            ctx.print(&claim, || claimed(&claim))
        }
        Some(TasksCommand::Renew(a)) => {
            let task = id(&a.id)?;
            let (epoch, token) = ctx.held(&task)?;
            let request = TaskActionRequest {
                epoch: Some(epoch),
                token: Some(token),
                lease_secs: lease_secs(a.lease),
                ..TaskActionRequest::default()
            };
            let claim = settle_call::<Claim>(ctx, &task, actions::RENEW, &request).await?;
            ctx.keep_claim(&claim)?;
            ctx.print(&claim, || {
                format!(
                    "renewed {} until {} (lease {}s)",
                    render::safe(&claim.task.id),
                    render::safe(&claim.until),
                    claim.lease_secs
                )
            })
        }
        Some(TasksCommand::Release(a)) => {
            settle(ctx, &id(&a.id)?, actions::RELEASE, None, None, None).await
        }
        Some(TasksCommand::Complete(a)) => {
            settle(
                ctx,
                &id(&a.id)?,
                actions::COMPLETE,
                a.result.clone(),
                None,
                None,
            )
            .await
        }
        Some(TasksCommand::Fail(a)) => {
            settle(
                ctx,
                &id(&a.id)?,
                actions::FAIL,
                None,
                Some(a.reason.clone()),
                None,
            )
            .await
        }
        Some(TasksCommand::Block(a)) => {
            let task = id(&a.id)?;
            // A worker blocks the task it holds; a creator blocks one nobody
            // holds, without a claim.
            match ctx.held(&task) {
                Ok(_) => {
                    settle(
                        ctx,
                        &task,
                        actions::BLOCK,
                        None,
                        Some(a.reason.clone()),
                        a.kind.clone(),
                    )
                    .await
                }
                Err(_) => {
                    let request = TaskActionRequest {
                        reason: Some(a.reason.clone()),
                        kind: a.kind.clone(),
                        ..TaskActionRequest::default()
                    };
                    change(ctx, &task, actions::BLOCK, &request).await
                }
            }
        }
        Some(TasksCommand::Unblock(a)) => {
            change(
                ctx,
                &id(&a.id)?,
                actions::UNBLOCK,
                &TaskActionRequest::default(),
            )
            .await
        }
        Some(TasksCommand::Reopen(a)) => {
            change(
                ctx,
                &id(&a.id)?,
                actions::REOPEN,
                &TaskActionRequest::default(),
            )
            .await
        }
        Some(TasksCommand::Assign(a)) => {
            let request = TaskActionRequest {
                assignee: a.who.clone(),
                pool: a.pool,
                ..TaskActionRequest::default()
            };
            change(ctx, &id(&a.id)?, actions::ASSIGN, &request).await
        }
        Some(TasksCommand::Needs(a)) => {
            let request = TaskActionRequest {
                needs: Some(if a.none { Vec::new() } else { a.needs.clone() }),
                ..TaskActionRequest::default()
            };
            change(ctx, &id(&a.id)?, actions::NEEDS, &request).await
        }
        Some(TasksCommand::Cancel(a)) => {
            let request = TaskActionRequest {
                reason: Some(a.reason.clone()),
                ..TaskActionRequest::default()
            };
            change(ctx, &id(&a.id)?, actions::CANCEL, &request).await
        }
        // Handled by `run_command`, which needs the signals.
        Some(TasksCommand::Run(_)) => Ok(()),
    }
}

async fn list(ctx: &Ctx<'_>, args: &TasksListArgs) -> Result<(), Failure> {
    let query = TasksQuery {
        channel: args.channel.clone(),
        state: Some(args.state.clone()),
        label: args.label.clone(),
        assignee: args.assignee.clone(),
        limit: args.limit,
        cursor: args.after.clone(),
    };
    let list = ctx.client.tasks(&query).await?;
    ctx.print(&list, || {
        let mut out: Vec<String> = list
            .tasks
            .iter()
            .map(|t| task_line(&t.root, t.reply_count))
            .collect();
        if out.is_empty() {
            out.push(format!(
                "no {} tasks{}",
                render::safe(&args.state),
                args.channel
                    .as_deref()
                    .map_or_else(String::new, |c| format!(" in {}", render::safe(c)))
            ));
        }
        if let Some(next) = &list.next_cursor {
            out.push(format!("next cursor: {}", render::safe(next)));
        }
        out.push(format!("cursor: {}", render::safe(&list.cursor)));
        out.join("\n")
    })
}

async fn create(ctx: &Ctx<'_>, args: &TaskCreateArgs, input: CreateInput) -> Result<(), Failure> {
    let mut labels = std::collections::BTreeMap::new();
    for raw in &args.labels {
        let (key, value) = crate::key_value(raw, "--label")?;
        labels.insert(key.to_owned(), value.to_owned());
    }
    match input {
        CreateInput::Plan(text) => {
            let mut plan: Vec<BatchTask> = serde_json::from_str(&text).map_err(|error| {
                Failure::usage(format!(
                    "the plan is not a JSON array of tasks: {} at line {}, column {}",
                    match error.classify() {
                        serde_json::error::Category::Data => "a field is wrong or unknown",
                        _ => "malformed JSON",
                    },
                    error.line(),
                    error.column()
                ))
                .next(
                    "write the plan as [{\"key\": \"fix\", \"text\": \"...\", \"labels\": {...}, \
                     \"needs\": [{\"key\": \"...\"}], \"for\": \"profile\"}]; any other field \
                     is refused",
                )
            })?;
            for entry in &mut plan {
                for (key, value) in &labels {
                    entry
                        .labels
                        .entry(key.clone())
                        .or_insert_with(|| value.clone());
                }
            }
            create_plan(ctx, &args.target, plan, args.key.as_deref()).await
        }
        CreateInput::Text(text) => {
            let request = CreateTaskRequest {
                channel: Some(args.target.clone()),
                text: Some(text),
                labels,
                needs: args.needs.clone(),
                assignee: args.assignee.clone(),
                claim: args.claim,
                lease_secs: lease_secs(args.lease),
                idempotency_key: args.key.clone(),
                origin: args.origin.clone(),
                ..CreateTaskRequest::default()
            };
            created_one(ctx, &request).await
        }
        CreateInput::None => {
            let request = CreateTaskRequest {
                thread_id: Some(args.target.clone()),
                labels,
                needs: args.needs.clone(),
                assignee: args.assignee.clone(),
                claim: args.claim,
                lease_secs: lease_secs(args.lease),
                idempotency_key: args.key.clone(),
                ..CreateTaskRequest::default()
            };
            created_one(ctx, &request).await
        }
    }
}

async fn created_one(ctx: &Ctx<'_>, request: &CreateTaskRequest) -> Result<(), Failure> {
    let response = ctx.client.create_task(request).await?;
    if let Some(claim) = &response.claim {
        ctx.keep_claim(claim)?;
    }
    ctx.print(&response, || {
        let mut out = format!(
            "created task {} in {} ({})",
            render::safe(&response.task.id),
            render::safe(&response.task.channel),
            state_of(&response.task)
        );
        if response.duplicate {
            out.push_str(", created earlier with the same --key");
        }
        if let Some(claim) = &response.claim {
            out.push_str(&format!(
                "\nclaimed it, epoch {}, until {}",
                claim.epoch,
                render::safe(&claim.until)
            ));
        }
        out.push_str(&format!("\ncursor: {}", render::safe(&response.cursor)));
        out
    })
}

/// Orders a plan so every entry comes after the entries it needs by key.
/// A plan that cannot be ordered has a cycle, which the board reports.
fn plan_order(plan: &[BatchTask]) -> Vec<usize> {
    let keys: HashMap<&str, usize> = plan
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| entry.key.as_deref().map(|key| (key, index)))
        .collect();
    let mut order = Vec::with_capacity(plan.len());
    let mut state = vec![0u8; plan.len()]; // 0 new, 1 visiting, 2 done
    fn visit(
        index: usize,
        plan: &[BatchTask],
        keys: &HashMap<&str, usize>,
        state: &mut [u8],
        order: &mut Vec<usize>,
    ) {
        if state[index] != 0 {
            return;
        }
        state[index] = 1;
        for need in &plan[index].needs {
            if let NeedRef::Key { key } = need {
                if let Some(&other) = keys.get(key.as_str()) {
                    if state[other] == 0 {
                        visit(other, plan, keys, state, order);
                    }
                }
            }
        }
        state[index] = 2;
        order.push(index);
    }
    for index in 0..plan.len() {
        visit(index, plan, &keys, &mut state, &mut order);
    }
    order
}

async fn create_plan(
    ctx: &Ctx<'_>,
    channel: &str,
    plan: Vec<BatchTask>,
    key: Option<&str>,
) -> Result<(), Failure> {
    if plan.is_empty() {
        return Err(
            Failure::usage("the plan has no tasks").next("add at least one entry with `text`")
        );
    }
    // One batch when it fits, so the board checks the whole plan at once.
    let order: Vec<usize> = if plan.len() <= MAX_TASKS_PER_BATCH {
        (0..plan.len()).collect()
    } else {
        plan_order(&plan)
    };
    let chunks: Vec<&[usize]> = order.chunks(MAX_TASKS_PER_BATCH).collect();
    let mut created: HashMap<usize, Message> = HashMap::new();
    let mut by_key: HashMap<String, String> = HashMap::new();
    let mut cursor = String::new();
    let mut duplicate = true;
    for (n, chunk) in chunks.iter().enumerate() {
        let tasks: Vec<BatchTask> = chunk
            .iter()
            .map(|index| {
                let mut entry = plan[*index].clone();
                // A key of an entry in an earlier batch becomes its ID.
                entry.needs = entry
                    .needs
                    .into_iter()
                    .map(|need| match need {
                        NeedRef::Key { key } => match by_key.get(&key) {
                            Some(id) => NeedRef::Id(id.clone()),
                            None => NeedRef::Key { key },
                        },
                        other => other,
                    })
                    .collect();
                entry
            })
            .collect();
        let request = CreateTasksRequest {
            channel: channel.to_owned(),
            tasks,
            idempotency_key: key.map(|key| {
                if chunks.len() == 1 {
                    key.to_owned()
                } else {
                    format!("{key}.{}", n + 1)
                }
            }),
        };
        let response = ctx.client.create_tasks(&request).await.map_err(|failure| {
            if created.is_empty() {
                failure
            } else {
                let hint = match key {
                    Some(_) => "run the same command again: the batches already written are not created twice".to_owned(),
                    None => "the tasks already created stay; add --key next time so that a rerun creates only the rest".to_owned(),
                };
                let message = format!(
                    "{} (after {} of {} tasks were created)",
                    failure.message,
                    created.len(),
                    plan.len()
                );
                let mut failure = failure.next(hint);
                failure.message = message;
                failure
            }
        })?;
        duplicate &= response.duplicate;
        cursor = response.cursor.clone();
        for (index, task) in chunk.iter().zip(response.tasks) {
            if let Some(key) = &plan[*index].key {
                by_key.insert(key.clone(), task.id.clone());
            }
            created.insert(*index, task);
        }
    }
    #[derive(Serialize)]
    struct Created<'a> {
        tasks: Vec<&'a Message>,
        cursor: &'a str,
        duplicate: bool,
    }
    let tasks: Vec<&Message> = (0..plan.len()).filter_map(|i| created.get(&i)).collect();
    let out = Created {
        tasks,
        cursor: &cursor,
        duplicate,
    };
    ctx.print(&out, || {
        let mut lines: Vec<String> = out.tasks.iter().map(|task| task_line(task, 0)).collect();
        lines.push(format!(
            "created {} tasks in {}{}",
            out.tasks.len(),
            render::safe(channel),
            if duplicate {
                ", all created earlier with the same --key"
            } else {
                ""
            }
        ));
        lines.push(format!("cursor: {}", render::safe(&cursor)));
        lines.join("\n")
    })
}

/// Claims the oldest ready task, waiting up to `--wait`. `stop` ends the
/// wait early. Fails with `nothing_ready` or `timeout` when none came.
async fn claim_ready(
    ctx: &Ctx<'_>,
    ready: &ReadyArgs,
    lease: Option<Duration>,
    stop: Option<&mut watch::Receiver<Option<Sig>>>,
) -> Result<Claim, Failure> {
    let channel = ready.ready.clone().unwrap_or_default();
    let deadline = ready.wait.map(|wait| Instant::now() + wait);
    let mut stop = stop;
    loop {
        let remaining = deadline.map_or(Duration::ZERO, |d| {
            d.saturating_duration_since(Instant::now())
        });
        let hold = remaining
            .as_secs()
            .saturating_add(u64::from(remaining.subsec_nanos() > 0))
            .min(MAX_WAIT_SECS);
        let request = ClaimReadyRequest {
            channel: channel.clone(),
            labels: ready.labels.clone(),
            from: ready.from.clone(),
            wait: (hold > 0).then_some(hold),
            lease_secs: lease_secs(lease),
        };
        let asked = Instant::now();
        let call = ctx.client.claim_ready(&request);
        let response = match stop.as_deref_mut() {
            Some(signals) => tokio::select! {
                biased;
                sig = signalled(signals) => return Err(crate::interrupted(sig)),
                response = call => response?,
            },
            None => call.await?,
        };
        if let Some(claim) = response.claim {
            return Ok(claim);
        }
        let over = deadline.is_none_or(|deadline| Instant::now() >= deadline);
        if over {
            let what = cli::format_duration(ready.wait.unwrap_or_default());
            let failure = match ready.wait {
                None => Failure::new(
                    "nothing_ready",
                    format!(
                        "no task you can claim is ready in {}",
                        render::safe(&channel)
                    ),
                )
                .next(
                    "add --wait 10m to wait for one; tasks created by others need --from <creator>",
                ),
                Some(_) => Failure::new(
                    codes::TIMEOUT,
                    format!(
                        "no task you can claim became ready in {} within {what}",
                        render::safe(&channel)
                    ),
                )
                .next("wait again, or check the queue with `openboard tasks <channel>`"),
            };
            return Err(failure);
        }
        // A board that is shutting down answers at once. Do not spin on it.
        if asked.elapsed() < Duration::from_millis(200) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// A call made with a claim. A claim the board says has ended is forgotten.
async fn settle_call<T: serde::de::DeserializeOwned>(
    ctx: &Ctx<'_>,
    task: &str,
    action: &str,
    request: &TaskActionRequest,
) -> Result<T, Failure> {
    match ctx.client.task_action::<T>(task, action, request).await {
        Ok(answer) => Ok(answer),
        Err(failure) => {
            if failure.code == api_codes::CLAIM_LOST {
                ctx.forget_claim(task);
            }
            Err(failure)
        }
    }
}

async fn settle(
    ctx: &Ctx<'_>,
    task: &str,
    action: &str,
    result: Option<String>,
    reason: Option<String>,
    kind: Option<String>,
) -> Result<(), Failure> {
    let (epoch, token) = ctx.held(task)?;
    let request = TaskActionRequest {
        epoch: Some(epoch),
        token: Some(token),
        result,
        reason,
        kind,
        ..TaskActionRequest::default()
    };
    let response = settle_call::<TaskResponse>(ctx, task, action, &request).await?;
    ctx.forget_claim(task);
    ctx.print(&response, || changed(&response, action))
}

async fn change(
    ctx: &Ctx<'_>,
    task: &str,
    action: &str,
    request: &TaskActionRequest,
) -> Result<(), Failure> {
    let response = ctx
        .client
        .task_action::<TaskResponse>(task, action, request)
        .await?;
    ctx.print(&response, || changed(&response, action))
}

// ---------------------------------------------------------------------------
// tasks run
// ---------------------------------------------------------------------------

/// `tasks run`. It handles its own signals, because an interrupted run
/// releases its task before it exits.
pub async fn run_command(
    ctx: &Ctx<'_>,
    args: &TaskRunArgs,
    mut signals: watch::Receiver<Option<Sig>>,
) -> Result<(), Failure> {
    let claim = match &args.id {
        Some(raw) => {
            let id = crate::message_id(raw)?;
            let request = TaskActionRequest {
                lease_secs: lease_secs(args.lease),
                ..TaskActionRequest::default()
            };
            tokio::select! {
                biased;
                sig = signalled(&mut signals) => return Err(crate::interrupted(sig)),
                claim = ctx.client.task_action::<Claim>(id, actions::CLAIM, &request) => claim?,
            }
        }
        None => claim_ready(ctx, &args.ready, args.lease, Some(&mut signals)).await?,
    };
    // A ready claim can be granted at any point of its long-poll, but the
    // answer follows the grant at once; a second covers the trip back.
    let started_claim = tokio::time::Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(tokio::time::Instant::now);
    ctx.keep_claim(&claim)?;
    let task = claim.task.id.clone();
    let holder = claim
        .task
        .task
        .as_ref()
        .and_then(|t| t.holder.as_ref())
        .map(|a| a.id.clone())
        .unwrap_or_default();
    if !ctx.json() {
        eprintln!(
            "openboard: claimed task {} (epoch {}, lease {}s); running {}",
            render::safe(&task),
            claim.epoch,
            claim.lease_secs,
            render::safe(&args.command.join(" "))
        );
    }

    let mut command = tokio::process::Command::new(&args.command[0]);
    command
        .args(&args.command[1..])
        .env(TASK_ENV, &task)
        .env(EPOCH_ENV, claim.epoch.to_string())
        .env(TOKEN_ENV, &claim.token)
        .env("BOARD_URL", &ctx.target.url)
        .env("BOARD_NAME", ctx.global.board())
        .kill_on_drop(true);
    // Its own process group, so that stopping it stops what it started.
    #[cfg(unix)]
    command.process_group(0);
    if let Some(principal) = &ctx.target.principal {
        command.env("BOARD_LOGIN", principal);
    }
    if let Some(profile) = ctx.global.profile() {
        command.env("BOARD_AS", profile);
    }
    if ctx.target.is_network() {
        match &ctx.target.fingerprint {
            Some(fingerprint) => command.env("BOARD_FINGERPRINT", fingerprint),
            // One inherited from the environment would pin this board to
            // another board's certificate.
            None => command.env_remove("BOARD_FINGERPRINT"),
        };
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            release(ctx, &task, claim.epoch, &claim.token).await;
            let exit = if error.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            };
            return Err(Failure::new(
                "command_failed",
                format!("cannot run {}: {error}", render::safe(&args.command[0])),
            )
            .next("check the command after `--`; the task was released")
            .exit(exit));
        }
    };
    let child_group = Group(child.id());

    let mut epoch_token = (claim.epoch, claim.token.clone());
    let lease = Duration::from_secs(claim.lease_secs.max(3));
    let every = lease / 3;
    let retry = Duration::from_secs(5).min(every);
    let mut renew_at = tokio::time::Instant::now() + every;
    // The latest the lease can run to: measured from when the claim or the
    // last renewal was asked for, which is no later than the board's own.
    let mut valid_until = started_claim + lease;
    let mut self_settled = false;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            () = tokio::time::sleep_until(renew_at), if !self_settled => {
                let request = TaskActionRequest {
                    epoch: Some(epoch_token.0),
                    token: Some(epoch_token.1.clone()),
                    lease_secs: Some(claim.lease_secs),
                    ..TaskActionRequest::default()
                };
                let asked = tokio::time::Instant::now();
                // A renewal that cannot finish before the lease ends is as
                // good as a failed one.
                let renewal = tokio::time::timeout_at(
                    valid_until,
                    ctx.client.task_action::<Claim>(&task, actions::RENEW, &request),
                )
                .await
                .unwrap_or_else(|_| {
                    Err(Failure::new(codes::REQUEST_TIMEOUT, "the renewal got no answer before the lease ended"))
                });
                match renewal {
                    Ok(renewed) => {
                        epoch_token = (renewed.epoch, renewed.token.clone());
                        let _ = ctx.keep_claim(&renewed);
                        valid_until = asked + Duration::from_secs(renewed.lease_secs.max(1));
                        renew_at = tokio::time::Instant::now() + every;
                    }
                    Err(failure) if lost(&failure) => {
                        if settled_by(ctx, &task, claim.epoch, &holder).await {
                            // The command settled the task itself.
                            self_settled = true;
                            ctx.forget_claim(&task);
                        } else {
                            stop_child(&mut child).await;
                            ctx.forget_claim(&task);
                            return Err(failure.next(
                                "the claim ended while the command ran (the lease ran out, or the \
                                 task was canceled, blocked or claimed again), so the command was \
                                 stopped",
                            ));
                        }
                    }
                    Err(failure) => {
                        let now = tokio::time::Instant::now();
                        if now + retry >= valid_until {
                            // Work past the lease could clash with the next
                            // worker's: stop before it ends.
                            stop_child(&mut child).await;
                            ctx.forget_claim(&task);
                            return Err(Failure::new(
                                "lease_unconfirmed",
                                format!(
                                    "could not renew the lease of task {} before it ended: {}",
                                    render::safe(&task),
                                    render::safe(&failure.message)
                                ),
                            )
                            .next(
                                "the command was stopped; the task becomes ready again when \
                                 the lease ends on the board, unless it was settled meanwhile",
                            ));
                        }
                        eprintln!(
                            "openboard: could not renew the lease of task {} ({}); trying again",
                            render::safe(&task),
                            render::safe(&failure.message)
                        );
                        renew_at = now + retry;
                    }
                }
            }
            sig = signalled(&mut signals) => {
                // The command stops before the claim is given back, so no
                // other worker starts while it still runs.
                stop_child(&mut child).await;
                if !self_settled {
                    release(ctx, &task, epoch_token.0, &epoch_token.1).await;
                }
                return Err(crate::interrupted(sig).next(format!(
                    "the command was stopped and task {} released for another worker",
                    safe_id(&task)
                )));
            }
        }
    };
    // Whatever the command left running in its group stops with it.
    stop_group(&child_group).await;

    let status = status.map_err(|error| {
        Failure::new(
            "command_failed",
            format!("cannot wait for the command: {error}"),
        )
    })?;
    let code = exit_code(&status);
    if self_settled {
        return finish(ctx, &task, code, None).await;
    }
    let request = TaskActionRequest {
        epoch: Some(epoch_token.0),
        token: Some(epoch_token.1.clone()),
        result: if code == 0 { args.result.clone() } else { None },
        reason: (code != 0).then(|| format!("the command exited with code {code}")),
        ..TaskActionRequest::default()
    };
    let action = if code == 0 {
        actions::COMPLETE
    } else {
        actions::FAIL
    };
    match ctx
        .client
        .task_action::<TaskResponse>(&task, action, &request)
        .await
    {
        Ok(response) => {
            ctx.forget_claim(&task);
            finish(ctx, &task, code, Some(response)).await
        }
        Err(failure) if lost(&failure) && settled_by(ctx, &task, claim.epoch, &holder).await => {
            ctx.forget_claim(&task);
            finish(ctx, &task, code, None).await
        }
        Err(failure) => {
            ctx.forget_claim(&task);
            Err(failure)
        }
    }
}

fn lost(failure: &Failure) -> bool {
    failure.code == api_codes::CLAIM_LOST || failure.code == api_codes::CLAIM_NOT_HELD
}

/// Whether the claim with `epoch` ended because its own holder settled the
/// task, as the command does when it blocks or completes the task itself.
async fn settled_by(ctx: &Ctx<'_>, task: &str, epoch: u64, holder: &str) -> bool {
    match ctx.client.task(task).await {
        Ok(view) => view.task.task.is_some_and(|t| {
            t.epoch == epoch && t.state != states::CLAIMED && t.changed_by.id == holder
        }),
        Err(_) => false,
    }
}

async fn release(ctx: &Ctx<'_>, task: &str, epoch: u64, token: &str) {
    let request = TaskActionRequest {
        epoch: Some(epoch),
        token: Some(token.to_owned()),
        ..TaskActionRequest::default()
    };
    if let Err(failure) = ctx
        .client
        .task_action::<TaskResponse>(task, actions::RELEASE, &request)
        .await
    {
        if !lost(&failure) {
            eprintln!(
                "openboard: could not release task {}: {}",
                render::safe(task),
                render::safe(&failure.message)
            );
        }
    }
    ctx.forget_claim(task);
}

/// How long a stopped command has to exit before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// The process group of the command, which is its own process ID.
#[derive(Clone, Copy)]
struct Group(Option<u32>);

/// Sends `signal` to every process in `group`. Whether any was there.
#[cfg(unix)]
fn signal_group(group: u32, signal: &str) -> bool {
    std::process::Command::new("kill")
        .args([signal, "--", &format!("-{group}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Stops whatever is left in `group`: SIGTERM, then SIGKILL after the
/// grace period.
async fn stop_group(group: &Group) {
    #[cfg(unix)]
    if let Some(group) = group.0 {
        if !signal_group(group, "-TERM") {
            return;
        }
        let deadline = tokio::time::Instant::now() + STOP_GRACE;
        while tokio::time::Instant::now() < deadline {
            if !signal_group(group, "-0") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        signal_group(group, "-KILL");
    }
    #[cfg(not(unix))]
    let _ = group;
}

/// Stops the command and everything in its process group, and waits for
/// the command to exit: SIGTERM first, SIGKILL after the grace period.
async fn stop_child(child: &mut tokio::process::Child) {
    let group = Group(child.id());
    #[cfg(unix)]
    if let Some(id) = group.0 {
        signal_group(id, "-TERM");
        if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_ok() {
            stop_group(&group).await;
            return;
        }
        signal_group(id, "-KILL");
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

/// Reports how a run ended and exits with the command's code.
async fn finish(
    ctx: &Ctx<'_>,
    task: &str,
    code: i32,
    response: Option<TaskResponse>,
) -> Result<(), Failure> {
    let response = match response {
        Some(response) => response,
        None => {
            let view = ctx.client.task(task).await?;
            TaskResponse {
                cursor: String::new(),
                task: view.task,
            }
        }
    };
    let state = state_of(&response.task).to_owned();
    if ctx.json() {
        write_stdout(&crate::to_json(&response)?)?;
    } else {
        eprintln!(
            "openboard: the command exited with code {code}; task {} is {state}",
            render::safe(task)
        );
    }
    if code == 0 {
        Ok(())
    } else {
        let mut failure = Failure::new(
            "command_failed",
            format!(
                "the command exited with code {code}; task {} is {state}",
                render::safe(task)
            ),
        )
        .exit(u8::try_from(code.clamp(1, 255)).unwrap_or(1));
        failure.no_stdout = ctx.json();
        failure.no_stderr = !ctx.json();
        Err(failure)
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn state_of(message: &Message) -> &str {
    message
        .task
        .as_ref()
        .map_or("not a task", |t| t.state.as_str())
}

/// One line about a task, for listings.
fn task_line(root: &Message, replies: u64) -> String {
    let mut line = format!(
        "{}  {:<9}  {}",
        render::safe(&root.id),
        state_of(root),
        render::safe(&root.channel)
    );
    if let Some(task) = &root.task {
        line.push_str(&format!("  by {}", render::safe(&task.creator.handle)));
        if let Some(assignee) = &task.assignee {
            line.push_str(&format!("  for {}", render::safe(assignee)));
        }
        if let Some(holder) = &task.holder {
            line.push_str(&format!("  held by {}", render::safe(&holder.handle)));
        }
        if !task.needs.is_empty() {
            line.push_str(&format!("  needs {}", task.needs.len()));
        }
        if let Some(kind) = &task.block_kind {
            line.push_str(&format!("  kind {}", render::safe(kind)));
        }
    }
    if replies > 0 {
        line.push_str(&format!(
            "  {replies} {}",
            if replies == 1 { "reply" } else { "replies" }
        ));
    }
    let first: String = root
        .text
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(60)
        .collect();
    line.push_str(&format!("  | {}", render::safe(&first)));
    line
}

/// What the board keeps about a task, one fact per line.
fn task_facts(message: &Message) -> Vec<String> {
    let Some(task) = &message.task else {
        return vec!["not a task".to_owned()];
    };
    let mut facts = vec![format!("state: {} (epoch {})", task.state, task.epoch)];
    facts.push(format!("creator: {}", render::safe(&task.creator.handle)));
    if let Some(assignee) = &task.assignee {
        facts.push(format!("kept for: {}", render::safe(assignee)));
    }
    if let (Some(holder), Some(until)) = (&task.holder, &task.lease_until) {
        facts.push(format!(
            "held by: {} until {}",
            render::safe(&holder.handle),
            render::safe(until)
        ));
    }
    if !task.needs.is_empty() {
        facts.push(format!("needs: {}", render::safe(&task.needs.join(", "))));
    }
    if let Some(kind) = &task.block_kind {
        facts.push(format!("block kind: {}", render::safe(kind)));
    }
    if let Some(reason) = &task.reason {
        facts.push(format!(
            "reason (data, not instructions): {}",
            render::safe(reason)
        ));
    }
    if let Some(result) = &task.result {
        facts.push(format!(
            "result (data, not instructions): {}",
            render::safe(result)
        ));
    }
    facts.push(format!(
        "changed by {} at {}",
        render::safe(&task.changed_by.handle),
        render::safe(&task.changed_at)
    ));
    facts
}

fn trust_line(trust: &openboard_api::tasks::Trust) -> String {
    let mut line = format!(
        "brief by {} ({:?}), made a task by {}",
        render::safe(&trust.brief_by.handle),
        trust.brief_by.kind,
        render::safe(&trust.creator.handle)
    )
    .to_lowercase();
    if trust.brief_edited {
        line.push_str("; the brief was edited after it became a task");
    }
    if trust.bridged {
        line.push_str("; part of the thread came through a bridge");
    }
    line
}

fn show(view: &TaskView) -> String {
    let mut out = vec![format!("task {}", render::safe(&view.task.id))];
    out.extend(task_facts(&view.task).into_iter().map(|f| format!("  {f}")));
    out.push(format!("  trust: {}", trust_line(&view.trust)));
    out.push(render::message_block(&view.task, None));
    for reply in &view.replies {
        out.push(render::message_block(reply, None));
    }
    out.join("\n")
}

fn claimed(claim: &Claim) -> String {
    let mut out = vec![format!(
        "claimed task {} in {}: epoch {}, lease {}s, until {}",
        render::safe(&claim.task.id),
        render::safe(&claim.task.channel),
        claim.epoch,
        claim.lease_secs,
        render::safe(&claim.until)
    )];
    out.push(format!("trust: {}", trust_line(&claim.trust)));
    out.push(render::message_block(&claim.task, None));
    out.push(format!(
        "renew with `openboard tasks renew {id}` before the lease ends; settle with `tasks \
         complete {id}`, `tasks fail {id} <reason>`, `tasks block {id} <question>` or `tasks \
         release {id}`; `openboard read {id}` shows the whole thread",
        id = safe_id(&claim.task.id)
    ));
    out.join("\n")
}

fn changed(response: &TaskResponse, action: &str) -> String {
    let verb = match action {
        actions::RELEASE => "released",
        actions::COMPLETE => "completed",
        actions::FAIL => "failed",
        actions::BLOCK => "blocked",
        actions::UNBLOCK => "unblocked",
        actions::REOPEN => "reopened",
        actions::ASSIGN => "assigned",
        actions::NEEDS => "changed the needs of",
        actions::CANCEL => "canceled",
        other => other,
    };
    let state = state_of(&response.task);
    let now = if verb == state {
        String::new()
    } else {
        format!("; it is {state} now")
    };
    format!(
        "{verb} task {}{now}\ncursor: {}",
        render::safe(&response.task.id),
        render::safe(&response.cursor)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_ids_are_recognised_by_shape() {
        assert!(is_message_id("msg_0123456789abcdef01234567"));
        for not in [
            "build",
            "msg_",
            "msg_0123456789ABCDEF01234567",
            "msg_0123456789abcdef0123456",
            "msg_0123456789abcdef012345678",
            "msg_0123456789abcdef0123456g",
        ] {
            assert!(!is_message_id(not), "{not}");
        }
    }

    #[test]
    fn a_long_plan_is_ordered_so_needs_come_first() {
        let entry = |key: &str, needs: &[&str]| BatchTask {
            key: Some(key.to_owned()),
            text: key.to_owned(),
            needs: needs
                .iter()
                .map(|k| NeedRef::Key {
                    key: (*k).to_owned(),
                })
                .collect(),
            ..BatchTask::default()
        };
        let plan = vec![
            entry("c", &["b"]),
            entry("b", &["a"]),
            entry("a", &[]),
            entry("d", &[]),
        ];
        let order = plan_order(&plan);
        let keys: Vec<&str> = order
            .iter()
            .map(|i| plan[*i].key.as_deref().unwrap())
            .collect();
        assert_eq!(keys, ["a", "b", "c", "d"]);
    }
}
