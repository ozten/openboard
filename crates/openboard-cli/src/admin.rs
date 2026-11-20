//! Operator commands: `invite` and `admin ...`.
//!
//! They act on a board's data directory on the board's own host, through
//! the server library, and work whether or not the board is running. There
//! is no network API for any of them.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use openboard_api::auth::{CredentialInfo, DEFAULT_TOKEN_TTL_SECS};
use openboard_api::ActorKind;
use openboard_server::operator::{
    self, Backup, InviteOptions, Invited, OperatorError, PersonInvite, PrincipalInfo,
    UpgradeAction, UpgradeCheck,
};
use openboard_server::Mode;
use serde::Serialize;
use serde_json::json;

use crate::cli::{AdminCommand, Global, InviteArgs};
use crate::error::{codes, Failure};
use crate::{cli, render, shell_quote, write_stdout};

/// Every credential ID starts with this, and no agent name can: names have
/// no underscore. That is how `admin revoke` tells the two apart.
const CREDENTIAL_ID_PREFIX: &str = "crd_";

/// The current time as the server writes timestamps: RFC 3339 in UTC with
/// milliseconds, a fixed width, so two of them compare as text.
pub fn now_rfc3339() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_epoch.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days, after Howard Hinnant's public domain algorithm.
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since_epoch.subsec_millis()
    )
}

fn failure_of(error: OperatorError, data_dir: &Path) -> Failure {
    let message = error.to_string();
    let dir = shell_quote(&data_dir.display().to_string());
    let start = format!(
        "start the board with `openboard serve --network --data-dir {dir}` first; that creates \
         its certificate"
    );
    match error {
        OperatorError::NoDatabase(_) => Failure::new(codes::NOT_NETWORK, message).next(format!(
            "pass --data-dir with the directory given to `openboard serve`; if the board has \
             never run, {start}"
        )),
        OperatorError::NotNetwork => Failure::new(codes::NOT_NETWORK, message).next(format!(
            "{start} (a local board becomes a network board for good)"
        )),
        // `invite` replaces this hint when the name was an email address.
        OperatorError::InvalidName(_) => Failure::usage(message).next(
            "choose a name such as `agent-a`: a letter, then letters, digits or hyphens; to \
             invite a person, pass their email address",
        ),
        OperatorError::InvalidUrl(_) => Failure::usage(message)
            .next("pass --url as https://<host>:<port> with nothing after the port"),
        OperatorError::NoSuchCredential(_) => {
            Failure::new(openboard_api::codes::NOT_FOUND, message).next(format!(
                "list the credential IDs with `openboard admin credentials --data-dir {dir}`"
            ))
        }
        OperatorError::NoSuchPrincipal(_) => Failure::new(openboard_api::codes::NOT_FOUND, message)
            .next(format!(
                "list the agents with `openboard admin agents --data-dir {dir}` and the people \
                 with `openboard admin people --data-dir {dir}`; to revoke one credential, pass \
                 its ID, which starts with `{CREDENTIAL_ID_PREFIX}`"
            )),
        OperatorError::NoSuchGrant(_) => Failure::new(openboard_api::codes::NOT_FOUND, message)
            .next(format!(
                "list the grants and their IDs with `openboard admin grants --data-dir {dir}`"
            )),
        OperatorError::InvalidGrant(_) => Failure::usage(message)
            .next("see `openboard admin grant --help` for patterns, names and verbs"),
        OperatorError::NotMigrated => Failure::new(codes::OPERATOR_FAILED, message).next(format!(
            "run `openboard admin upgrade-check --data-dir {dir}`, then start the server once"
        )),
        OperatorError::Storage(_) => Failure::new(codes::OPERATOR_FAILED, message)
            .next("check that you can read and write the data directory, then retry"),
    }
}

/// Runs an operator function, which blocks on disk I/O, off the runtime.
async fn operate<T: Send + 'static>(
    data_dir: &Path,
    work: impl FnOnce(&Path) -> Result<T, OperatorError> + Send + 'static,
) -> Result<T, Failure> {
    let dir = data_dir.to_path_buf();
    tokio::task::spawn_blocking(move || work(&dir))
        .await
        .map_err(|error| Failure::new(codes::OPERATOR_FAILED, error.to_string()))?
        .map_err(|error| failure_of(error, data_dir))
}

fn kind_name(kind: openboard_api::ActorKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|kind| kind.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn or_dash(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| "-".to_owned())
}

/// `invite <name-or-email>`.
pub async fn invite(global: &Global, args: &InviteArgs) -> Result<(), Failure> {
    let options = InviteOptions {
        ttl_secs: args.ttl.as_secs(),
        // Only when asked: the default differs between a token and a session.
        token_ttl_secs: args.token_ttl.map(|ttl| ttl.as_secs()),
        // Only the flag: BOARD_URL says which board a client talks to, not
        // what an invite should carry.
        url: global.url_flag().map(str::to_owned),
    };
    let name = args.name.clone();
    // As the board decides it: an `@` makes the name an email address.
    let email = name.contains('@');
    let invited = operate(&args.data.data_dir, move |dir| {
        operator::invite(dir, &name, &options)
    })
    .await
    .map_err(|failure| {
        if email && failure.message.starts_with("invalid name") {
            failure.next(
                "write the address as name@example.com: letters, digits and single '.', '_', \
                 '+' or '-' between them, around one '@'; to invite an agent instead, pass a \
                 name without '@', such as `agent-a`",
            )
        } else {
            failure
        }
    })?;
    let created = match invited {
        Invited::Person(person) => return person_invited(global.json, args, &person),
        Invited::Agent(created) => created,
    };
    let invite = created.invite.encode();
    let join = format!("openboard join {invite}");
    if global.json {
        return write_stdout(
            &json!({
                "kind": "agent",
                "invite": invite,
                "id": created.id,
                "principal": created.principal,
                "new_principal": created.new_principal,
                "url": created.invite.url,
                "fingerprint": created.invite.fingerprint,
                "expires_at": created.expires_at,
                "token_ttl_secs": options_token_ttl(args),
                "join_command": join,
            })
            .to_string(),
        );
    }
    let who = if created.new_principal {
        format!("{} (a new agent)", created.principal)
    } else {
        format!(
            "{} (one more credential for an agent that already exists)",
            created.principal
        )
    };
    write_stdout(&format!(
        "invite {} for {who}\n\
         board: {}\n\
         certificate fingerprint: {}\n\
         expires: {} (in {}); it works once and is a secret until then\n\
         \n\
         {invite}\n\
         \n\
         on the agent's machine, run:\n  {join}",
        created.id,
        created.invite.url,
        created.invite.fingerprint,
        created.expires_at,
        cli::format_duration(args.ttl),
    ))
}

/// How long an agent's token will last, as `invite --json` reports it.
fn options_token_ttl(args: &InviteArgs) -> u64 {
    args.token_ttl
        .map_or(DEFAULT_TOKEN_TTL_SECS, |ttl| ttl.as_secs())
}

/// What `invite <email>` prints: the sign-in link and what to do with it.
fn person_invited(
    json_output: bool,
    args: &InviteArgs,
    person: &PersonInvite,
) -> Result<(), Failure> {
    if json_output {
        return write_stdout(
            &json!({
                "kind": "person",
                "link": person.link,
                "code": person.code,
                "id": person.id,
                "principal": person.principal,
                "new_principal": person.new_principal,
                "url": person.url,
                "expires_at": person.expires_at,
                "session_ttl_secs": person.session_ttl_secs,
            })
            .to_string(),
        );
    }
    let email = render::safe(&person.principal);
    let who = if person.new_principal {
        format!("{email} (a new person)")
    } else {
        format!("{email} (one more sign-in for a person the board already knows)")
    };
    write_stdout(&format!(
        "sign-in link {} for {who}\n\
         board: {}\n\
         expires: {} (in {}); it works once and is a secret until then\n\
         session: the browser that signs in stays signed in for {}\n\
         \n\
         one-time sign-in link:\n\
         {}\n\
         \n\
         Send this link to {email} yourself, over a channel you trust: no email is sent, and \
         whoever opens it first becomes {email} on the board.\n\
         Opening it and pressing \"Sign in\" signs that browser in. It works once.\n\
         The browser will show a certificate warning, because the board's certificate is \
         self-signed. Before accepting it, they should check that the address is {}.",
        person.id,
        person.url,
        person.expires_at,
        cli::format_duration(args.ttl),
        cli::format_duration(Duration::from_secs(person.session_ttl_secs)),
        person.link,
        person.url,
    ))
}

/// A credential with what an operator wants to know first.
#[derive(Serialize)]
struct CredentialRow {
    #[serde(flatten)]
    info: CredentialInfo,
    /// `token` for an agent's token, `session` for a person's browser.
    #[serde(rename = "type")]
    credential_type: &'static str,
    /// `active`, `revoked` or `expired`.
    status: &'static str,
}

/// Only a person holds sessions; everything else is a token.
fn credential_type(kind: ActorKind) -> &'static str {
    match kind {
        ActorKind::Person => "session",
        _ => "token",
    }
}

/// One line per principal, for `admin agents` and `admin people`.
fn principals_document(key: &str, principals: &[PrincipalInfo]) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = principals
        .iter()
        .map(|principal| {
            json!({
                "id": principal.id,
                "handle": principal.handle,
                "kind": principal.kind,
                "created_at": principal.created_at,
                "active_credentials": principal.active_credentials,
                "pending_invites": principal.pending_invites,
            })
        })
        .collect();
    json!({ key: rows })
}

fn status_of(credential: &CredentialInfo, now: &str) -> &'static str {
    if credential.revoked_at.is_some() {
        "revoked"
    } else if credential.expires_at.as_str() <= now {
        "expired"
    } else {
        "active"
    }
}

fn data_dir_of(command: &AdminCommand) -> PathBuf {
    match command {
        AdminCommand::Info(args)
        | AdminCommand::UpgradeCheck(args)
        | AdminCommand::Agents(args)
        | AdminCommand::People(args)
        | AdminCommand::Credentials(args)
        | AdminCommand::Invites(args) => args.data_dir.clone(),
        AdminCommand::Revoke(args) => args.data.data_dir.clone(),
        AdminCommand::SecurityLog(args) => args.data.data_dir.clone(),
        AdminCommand::Backups(args) => args.data.data_dir.clone(),
        AdminCommand::VerifyLog(args) => args.data_dir.clone(),
        AdminCommand::Grant(args) => args.data.data_dir.clone(),
        AdminCommand::Grants(args) => args.data.data_dir.clone(),
        AdminCommand::Ungrant(args) => args.data.data_dir.clone(),
        AdminCommand::Explain(args) => args.data.data_dir.clone(),
        AdminCommand::Access(args) => args.data.data_dir.clone(),
    }
}

fn grant_json(grant: &operator::GrantInfo) -> serde_json::Value {
    json!({
        "id": grant.id,
        "board": grant.board,
        "pattern": grant.pattern,
        "grantee": grant.grantee,
        "verbs": grant.verbs,
        "expires_at": grant.expires_at,
        "expired": grant.expired,
        "created_by": grant.created_by,
        "created_at": grant.created_at,
    })
}

fn grant_line(grant: &operator::GrantInfo) -> String {
    let mut line = format!(
        "{}  {}  {}  {}  {}",
        render::safe(&grant.id),
        render::safe(&grant.board),
        render::safe(&grant.pattern),
        render::safe(&grant.grantee),
        render::safe(&grant.verbs)
    );
    match (&grant.expires_at, grant.expired) {
        (Some(at), true) => line.push_str(&format!("  expired {}", render::safe(at))),
        (Some(at), false) => line.push_str(&format!("  until {}", render::safe(at))),
        (None, _) => {}
    }
    line
}

/// A byte count for people: `512 B`, `3.4 MiB`.
fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// An age for people: `45s`, `3h`, `12d`.
fn age(secs: u64) -> String {
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 2 * 3600 => format!("{}m", s / 60),
        s if s < 2 * 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Who last started a server on the directory, for people.
fn started_by(check: &UpgradeCheck) -> String {
    match &check.last_started_by {
        Some(by) => {
            let mut text = format!("Openboard {}", render::safe(&by.version));
            if let Some(commit) = &by.commit {
                text.push_str(&format!(" ({})", render::safe(commit)));
            }
            if let Some(at) = &by.at {
                text.push_str(&format!(" at {}", render::safe(at)));
            }
            text
        }
        None => "not recorded (a version older than 0.1.0, or never started)".to_owned(),
    }
}

/// What the next start does, for people.
fn next_start(check: &UpgradeCheck) -> String {
    match check.action {
        UpgradeAction::None => "nothing to migrate: the schema is this version's".to_owned(),
        UpgradeAction::Migrate { from, to } => format!(
            "migrate the database from schema version {from} to {to}, after writing a backup"
        ),
        UpgradeAction::Refuse => format!(
            "refuse to start: the database is newer than this openboard ({}) supports",
            openboard_api::build::DESCRIBE
        ),
    }
}

fn last_started_json(check: &UpgradeCheck) -> serde_json::Value {
    match &check.last_started_by {
        Some(by) => json!({ "version": by.version, "commit": by.commit, "at": by.at }),
        None => serde_json::Value::Null,
    }
}

/// `admin upgrade-check`. Read-only.
async fn upgrade_check(json_output: bool, dir: &Path) -> Result<(), Failure> {
    let check = operate(dir, operator::upgrade_check).await?;
    let backup = check
        .backup_name
        .as_ref()
        .map(|name| check.backup_dir.join(name).display().to_string());
    let (migrates, to) = match check.action {
        UpgradeAction::Migrate { to, .. } => (true, Some(to)),
        _ => (false, None),
    };
    // Moving to schema 5 brings grants (M4). Everyone keeps the access they
    // had, and nobody may change grants until the operator says who may.
    let grants_note = (check.schema_version < 5 && check.supported_schema_version >= 5)
        .then_some(
            "grants arrive with this upgrade: agents and people keep the access they have, and              nobody can change who may do what until you run `openboard admin grant '*' <your              name or email> all --data-dir <dir>` (or start once with `serve --admin <name>`)",
        );
    let failure = match check.action {
        UpgradeAction::Refuse => Some(
            Failure::new(
                codes::SCHEMA_TOO_NEW,
                format!(
                    "the database has schema version {}, newer than this openboard supports ({})",
                    check.schema_version, check.supported_schema_version
                ),
            )
            .next(
                "install the version of Openboard that last started this board, or a newer one \
                 (`last started by` above), or restore a backup taken before the upgrade",
            ),
        ),
        UpgradeAction::Migrate { .. } if check.room_for_backup == Some(false) => Some(
            Failure::new(
                codes::NO_ROOM_FOR_BACKUP,
                format!(
                    "a migration is due, but the file system holding {} has {} free and the \
                     backup needs about {}",
                    dir.display(),
                    bytes(check.free_bytes.unwrap_or(0)),
                    bytes(check.database_bytes)
                ),
            )
            .next(
                "free some space in the data directory's file system: the server refuses to \
                 migrate without its backup",
            ),
        ),
        _ => None,
    };
    if json_output {
        let mut document = json!({
            "data_dir": dir.display().to_string(),
            "schema_version": check.schema_version,
            "supported_schema_version": check.supported_schema_version,
            "openboard": {
                "version": openboard_api::build::VERSION,
                "commit": openboard_api::build::commit(),
            },
            "last_started_by": last_started_json(&check),
            "action": match check.action {
                UpgradeAction::None => "none",
                UpgradeAction::Migrate { .. } => "migrate",
                UpgradeAction::Refuse => "refuse",
            },
            "would_migrate": migrates,
            "migrate_to": to,
            "backup_path": backup,
            "database_bytes": check.database_bytes,
            "free_bytes": check.free_bytes,
            "room_for_backup": check.room_for_backup,
            "notes": grants_note.into_iter().collect::<Vec<_>>(),
        });
        if let Some(failure) = &failure {
            let error: serde_json::Value =
                serde_json::from_str(&failure.to_json()).unwrap_or_default();
            document["error"] = error["error"].clone();
        }
        write_stdout(&document.to_string())?;
    } else {
        let mut human = format!(
            "data directory: {}\n\
             database schema version: {}\n\
             this openboard: {}, schema version {}\n\
             last started by: {}\n\
             on next start: {}",
            dir.display(),
            check.schema_version,
            openboard_api::build::DESCRIBE,
            check.supported_schema_version,
            started_by(&check),
            next_start(&check),
        );
        if let Some(backup) = &backup {
            let room = match check.room_for_backup {
                Some(true) => "yes",
                Some(false) => "NO",
                None => "unknown",
            };
            human.push_str(&format!(
                "\nbackup: {backup}\ndatabase size: {}; free space: {}; room for the backup: \
                 {room}",
                bytes(check.database_bytes),
                check.free_bytes.map_or_else(|| "unknown".to_owned(), bytes),
            ));
        }
        if let Some(note) = grants_note {
            human.push_str(&format!(
                "
note: {note}"
            ));
        }
        write_stdout(&human)?;
    }
    match failure {
        Some(mut failure) => {
            failure.no_stdout = true;
            Err(failure)
        }
        None => Ok(()),
    }
}

/// `admin backups [--prune]`.
async fn backups(json_output: bool, dir: &Path, prune: bool) -> Result<(), Failure> {
    let found: Vec<Backup> = if prune {
        operate(dir, operator::prune_backups).await?
    } else {
        operate(dir, operator::list_backups).await?
    };
    if json_output {
        let rows: Vec<serde_json::Value> = found
            .iter()
            .map(|backup| {
                json!({
                    "path": backup.path.display().to_string(),
                    "file_name": backup.file_name,
                    "size_bytes": backup.size_bytes,
                    "modified_at": backup.modified_at,
                    "age_secs": backup.age_secs,
                    "migrated_to_schema_version": backup.before_schema_version,
                })
            })
            .collect();
        return write_stdout(&json!({ "backups": rows, "pruned": prune }).to_string());
    }
    let dir_flag = shell_quote(&dir.display().to_string());
    if found.is_empty() {
        return write_stdout(&format!("no backups in {}", dir.display()));
    }
    if prune {
        let names: Vec<String> = found.iter().map(|b| render::safe(&b.file_name)).collect();
        return write_stdout(&format!(
            "deleted {} backup(s) from {}: {}",
            found.len(),
            dir.display(),
            names.join(", ")
        ));
    }
    let rows: Vec<Vec<String>> = found
        .iter()
        .map(|backup| {
            vec![
                backup.file_name.clone(),
                bytes(backup.size_bytes),
                backup.age_secs.map_or_else(|| "-".to_owned(), age),
                or_dash(&backup.modified_at),
            ]
        })
        .collect();
    write_stdout(&format!(
        "{}\n\
         each backup keeps the text of messages deleted or edited since it was taken; once the \
         upgrade is confirmed, delete them with: openboard admin backups --prune --data-dir \
         {dir_flag}",
        render::table(&["FILE", "SIZE", "AGE", "WRITTEN"], &rows)
    ))
}

/// What an empty listing means on this data directory.
fn nothing(what: &str, mode: Mode) -> String {
    match mode {
        Mode::Network => format!("no {what}"),
        Mode::Local => format!(
            "no {what}: this is a local board, which has no agents, people, credentials or \
             invites until it is started with `openboard serve --network`"
        ),
    }
}

/// `admin ...`, except `verify-log`.
pub async fn run(json_output: bool, command: &AdminCommand) -> Result<(), Failure> {
    let dir = data_dir_of(command);
    let print = |document: serde_json::Value, human: String| {
        write_stdout(&if json_output {
            document.to_string()
        } else {
            human
        })
    };
    // These read a database of any version, a newer one included.
    match command {
        AdminCommand::UpgradeCheck(_) => return upgrade_check(json_output, &dir).await,
        AdminCommand::Backups(args) => return backups(json_output, &dir, args.prune).await,
        _ => {}
    }
    // Also the check that the directory holds a board at all.
    let info = operate(&dir, operator::board_info).await?;
    match command {
        AdminCommand::UpgradeCheck(_) | AdminCommand::Backups(_) => Ok(()),
        AdminCommand::Info(_) => {
            let check = operate(&dir, operator::upgrade_check).await?;
            let mut human = format!("mode: {}", info.mode);
            match info.mode {
                Mode::Network => {
                    human.push_str(&format!(
                        "\nurl: {}\ncertificate fingerprint: {}",
                        info.url.as_deref().unwrap_or("(not recorded yet)"),
                        info.fingerprint.as_deref().unwrap_or("(not recorded yet)")
                    ));
                }
                Mode::Local => human.push_str(
                    "\nurl: a local board prints its address when it starts\ncertificate \
                     fingerprint: none (plain HTTP on loopback, no authentication)",
                ),
            }
            human.push_str(&format!(
                "\ndata directory: {}\nschema version: {} (this openboard: {})\non next start: \
                 {}\nlast started by: {}",
                dir.display(),
                check.schema_version,
                check.supported_schema_version,
                next_start(&check),
                started_by(&check),
            ));
            print(
                json!({
                    "mode": info.mode.as_str(),
                    "url": info.url,
                    "fingerprint": info.fingerprint,
                    "data_dir": dir.display().to_string(),
                    "schema_version": check.schema_version,
                    "supported_schema_version": check.supported_schema_version,
                    "would_migrate": matches!(check.action, UpgradeAction::Migrate { .. }),
                    "last_started_by": last_started_json(&check),
                }),
                human,
            )
        }
        AdminCommand::Agents(_) => {
            let agents = operate(&dir, operator::list_agents).await?;
            let rows: Vec<Vec<String>> = agents
                .iter()
                .map(|agent| {
                    vec![
                        agent.handle.clone(),
                        kind_name(agent.kind),
                        agent.active_credentials.to_string(),
                        agent.pending_invites.to_string(),
                        agent.created_at.clone(),
                        agent.id.clone(),
                    ]
                })
                .collect();
            let document = principals_document("agents", &agents);
            let human = if rows.is_empty() {
                nothing("agents are enrolled", info.mode)
            } else {
                render::table(
                    &[
                        "NAME",
                        "KIND",
                        "ACTIVE CREDENTIALS",
                        "PENDING INVITES",
                        "CREATED",
                        "ID",
                    ],
                    &rows,
                )
            };
            print(document, human)
        }
        AdminCommand::People(_) => {
            let people = operate(&dir, operator::list_people).await?;
            let rows: Vec<Vec<String>> = people
                .iter()
                .map(|person| {
                    vec![
                        person.handle.clone(),
                        person.active_credentials.to_string(),
                        person.pending_invites.to_string(),
                        person.created_at.clone(),
                        person.id.clone(),
                    ]
                })
                .collect();
            let human = if rows.is_empty() {
                nothing(
                    "people have been invited; invite one with `openboard invite <email>`",
                    info.mode,
                )
            } else {
                render::table(
                    &[
                        "EMAIL",
                        "ACTIVE SESSIONS",
                        "PENDING SIGN-IN LINKS",
                        "CREATED",
                        "ID",
                    ],
                    &rows,
                )
            };
            print(principals_document("people", &people), human)
        }
        AdminCommand::Credentials(_) => {
            let now = now_rfc3339();
            let credentials: Vec<CredentialRow> = operate(&dir, operator::list_credentials)
                .await?
                .into_iter()
                .map(|info| CredentialRow {
                    credential_type: credential_type(info.kind),
                    status: status_of(&info, &now),
                    info,
                })
                .collect();
            let rows: Vec<Vec<String>> = credentials
                .iter()
                .map(|row| {
                    vec![
                        row.info.id.clone(),
                        row.credential_type.to_owned(),
                        row.info.principal.clone(),
                        match &row.info.revoked_at {
                            Some(at) => format!("revoked {at}"),
                            None => row.status.to_owned(),
                        },
                        row.info.created_at.clone(),
                        row.info.expires_at.clone(),
                        or_dash(&row.info.last_used_at),
                    ]
                })
                .collect();
            let human = if rows.is_empty() {
                nothing("credentials exist", info.mode)
            } else {
                render::table(
                    &[
                        "ID",
                        "TYPE",
                        "NAME",
                        "STATUS",
                        "CREATED",
                        "EXPIRES",
                        "LAST USED",
                    ],
                    &rows,
                )
            };
            print(json!({ "credentials": credentials }), human)
        }
        AdminCommand::Invites(_) => {
            let invites = operate(&dir, operator::list_invites).await?;
            let rows: Vec<Vec<String>> = invites
                .iter()
                .map(|invite| {
                    vec![
                        invite.id.clone(),
                        match invite.kind {
                            ActorKind::Person => "sign-in link".to_owned(),
                            _ => "agent invite".to_owned(),
                        },
                        invite.principal.clone(),
                        invite.created_at.clone(),
                        invite.expires_at.clone(),
                    ]
                })
                .collect();
            let document = json!({
                "invites": invites.iter().map(|invite| json!({
                    "id": invite.id,
                    "principal": invite.principal,
                    "kind": invite.kind,
                    "created_at": invite.created_at,
                    "expires_at": invite.expires_at,
                })).collect::<Vec<_>>(),
            });
            let human = if rows.is_empty() {
                nothing("invites are pending", info.mode)
            } else {
                render::table(&["ID", "KIND", "NAME", "CREATED", "EXPIRES"], &rows)
            };
            print(document, human)
        }
        AdminCommand::Revoke(args) => {
            let target = args.target.clone();
            // An email address may start like a credential ID; an ID has no `@`.
            let by_id = target.starts_with(CREDENTIAL_ID_PREFIX) && !target.contains('@');
            let revoked = operate(&dir, move |dir| {
                if by_id {
                    operator::revoke_credential(dir, &target)
                } else {
                    operator::revoke_principal(dir, &target)
                }
            })
            .await?;
            let agent = render::safe(&revoked.principal);
            let mut human = match (
                by_id,
                revoked.credentials.is_empty(),
                revoked.invites.is_empty(),
            ) {
                (true, false, _) => {
                    format!("revoked credential {} of {agent}", revoked.credentials[0])
                }
                (true, true, _) => format!(
                    "credential {} of {agent} was already revoked; nothing changed",
                    render::safe(&args.target)
                ),
                (false, true, true) => format!(
                    "{agent} had no usable credentials and no pending invites; nothing changed"
                ),
                (false, _, _) => {
                    let mut parts = Vec::new();
                    if !revoked.credentials.is_empty() {
                        parts.push(format!(
                            "revoked {} credential(s) of {agent}: {}",
                            revoked.credentials.len(),
                            revoked.credentials.join(", ")
                        ));
                    }
                    if !revoked.invites.is_empty() {
                        parts.push(format!(
                            "withdrew {} pending invite(s) for {agent}: {}",
                            revoked.invites.len(),
                            revoked.invites.join(", ")
                        ));
                    }
                    parts.join("\n")
                }
            };
            // A person's handle is an email address, and no agent's name
            // holds an `@`.
            let person = revoked.principal.contains('@');
            if !revoked.credentials.is_empty() {
                human.push_str(if person {
                    "\na running board refuses the session from its next request, which signs \
                     that browser out, and ends its live updates within a few seconds"
                } else {
                    "\na running board refuses the token from its next request and ends its \
                     open waits within a few seconds"
                });
            }
            print(
                json!({
                    "principal": revoked.principal,
                    "revoked": match (by_id, person) {
                        (true, _) => "credential",
                        (false, true) => "person",
                        (false, false) => "agent",
                    },
                    "credentials": revoked.credentials,
                    "invites": revoked.invites,
                }),
                human,
            )
        }
        AdminCommand::SecurityLog(args) => {
            let limit = args.limit;
            let entries = operate(&dir, move |dir| operator::security_log(dir, limit)).await?;
            let rows: Vec<Vec<String>> = entries
                .iter()
                .map(|entry| {
                    vec![
                        entry.id.to_string(),
                        entry.at.clone(),
                        entry.kind.clone(),
                        or_dash(&entry.principal),
                        or_dash(&entry.credential_id),
                        or_dash(&entry.client),
                        or_dash(&entry.detail),
                    ]
                })
                .collect();
            let document = json!({
                "entries": entries.iter().map(|entry| json!({
                    "id": entry.id,
                    "at": entry.at,
                    "kind": entry.kind,
                    "principal": entry.principal,
                    "credential_id": entry.credential_id,
                    "client": entry.client,
                    "detail": entry.detail,
                })).collect::<Vec<_>>(),
            });
            let human = if rows.is_empty() {
                "the security log is empty".to_owned()
            } else {
                render::table(
                    &["#", "AT", "EVENT", "NAME", "CREDENTIAL", "CLIENT", "DETAIL"],
                    &rows,
                )
            };
            print(document, human)
        }
        AdminCommand::Grant(args) => {
            let (board, pattern, who, verbs, ttl) = (
                args.board.clone(),
                args.pattern.clone(),
                args.who.clone(),
                args.verbs.clone(),
                args.expires,
            );
            let grant = operate(&dir, move |dir| {
                operator::grant(dir, &board, &pattern, &who, &verbs, ttl)
            })
            .await?;
            print(
                grant_json(&grant),
                format!(
                    "granted {}\na running board applies it on its next request",
                    grant_line(&grant)
                ),
            )
        }
        AdminCommand::Grants(args) => {
            let board = args.board.clone();
            let grants = operate(&dir, move |dir| {
                operator::list_grants(dir, board.as_deref())
            })
            .await?;
            let rows: Vec<Vec<String>> = grants
                .iter()
                .map(|grant| {
                    vec![
                        grant.id.clone(),
                        grant.board.clone(),
                        grant.pattern.clone(),
                        grant.grantee.clone(),
                        grant.verbs.clone(),
                        match (&grant.expires_at, grant.expired) {
                            (Some(at), true) => format!("expired {at}"),
                            (Some(at), false) => at.clone(),
                            (None, _) => "-".to_owned(),
                        },
                        grant.created_by.clone(),
                    ]
                })
                .collect();
            let human = if rows.is_empty() {
                "no grants: nobody but the operator can use this board".to_owned()
            } else {
                render::table(
                    &["ID", "BOARD", "PATTERN", "WHO", "VERBS", "EXPIRES", "BY"],
                    &rows,
                )
            };
            print(
                json!({ "grants": grants.iter().map(grant_json).collect::<Vec<_>>() }),
                human,
            )
        }
        AdminCommand::Ungrant(args) => {
            let id = args.id.clone();
            let grant = operate(&dir, move |dir| operator::ungrant(dir, &id)).await?;
            print(
                grant_json(&grant),
                format!(
                    "removed {}\na running board applies it on its next request",
                    grant_line(&grant)
                ),
            )
        }
        AdminCommand::Explain(args) => {
            let (board, who, channel) =
                (args.board.clone(), args.who.clone(), args.channel.clone());
            let explained = operate(&dir, move |dir| {
                operator::explain(dir, &board, &who, &channel)
            })
            .await?;
            let who = render::safe(&args.who);
            let channel = render::safe(&args.channel);
            let mut human = format!("{who} at {channel}: {}", explained.verbs);
            if explained.deciding.is_empty() {
                human.push_str("\nno grant applies, so no access");
            } else {
                human.push_str("\ndecided by:");
                for grant in &explained.deciding {
                    human.push_str(&format!("\n  {}", grant_line(grant)));
                }
            }
            print(
                json!({
                    "verbs": explained.verbs,
                    "deciding": explained.deciding.iter().map(grant_json).collect::<Vec<_>>(),
                }),
                human,
            )
        }
        AdminCommand::Access(args) => {
            let access = match args.mode.as_deref() {
                None => operate(&dir, operator::access).await?,
                Some(mode) => {
                    let access = if mode == "open" {
                        openboard_server::Access::Open
                    } else {
                        openboard_server::Access::Closed
                    };
                    operate(&dir, move |dir| operator::set_access(dir, access)).await?;
                    access
                }
            };
            let human = match access {
                openboard_server::Access::Open => {
                    "access: open (any agent or person may create boards; new boards admit them all)"
                }
                openboard_server::Access::Closed => {
                    "access: closed (only the operator creates boards; new boards admit their creator)"
                }
            };
            print(json!({ "access": access.as_str() }), human.to_owned())
        }
        // Handled before this is reached; see `dispatch`.
        AdminCommand::VerifyLog(_) => Ok(()),
    }
}
