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
    self, InviteOptions, Invited, OperatorError, PersonInvite, PrincipalInfo,
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
        | AdminCommand::Agents(args)
        | AdminCommand::People(args)
        | AdminCommand::Credentials(args)
        | AdminCommand::Invites(args) => args.data_dir.clone(),
        AdminCommand::Revoke(args) => args.data.data_dir.clone(),
        AdminCommand::SecurityLog(args) => args.data.data_dir.clone(),
        AdminCommand::VerifyLog(args) => args.data_dir.clone(),
    }
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
    // Also the check that the directory holds a board at all.
    let info = operate(&dir, operator::board_info).await?;
    match command {
        AdminCommand::Info(_) => {
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
            human.push_str(&format!("\ndata directory: {}", dir.display()));
            print(
                json!({
                    "mode": info.mode.as_str(),
                    "url": info.url,
                    "fingerprint": info.fingerprint,
                    "data_dir": dir.display().to_string(),
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
        // Handled before this is reached; see `dispatch`.
        AdminCommand::VerifyLog(_) => Ok(()),
    }
}
