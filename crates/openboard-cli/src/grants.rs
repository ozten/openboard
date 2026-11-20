//! `openboard grant`, `grants`, `ungrant`, `explain` and `channels create`:
//! the grant commands an agent or a person runs against a network board.
//! The operator's own are under `openboard admin`.

use openboard_api::grants::{Grant, NewChannel, NewGrant};

use crate::cli::{ChannelCreateArgs, ExplainArgs, GrantArgs, UngrantArgs};
use crate::error::Failure;
use crate::{render, Ctx};

fn line(grant: &Grant) -> String {
    let mut line = format!(
        "{}  {}  {}  {}",
        render::safe(&grant.id),
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

/// A `not_found` from a grant command is about a name or a grant ID, not
/// a thread.
fn not_found_hint(failure: Failure, hint: &str) -> Failure {
    if failure.code == openboard_api::codes::NOT_FOUND {
        failure.next(hint)
    } else {
        failure
    }
}

pub async fn grant(ctx: &Ctx<'_>, args: &GrantArgs) -> Result<(), Failure> {
    let changed = ctx
        .client
        .add_grant(&NewGrant {
            pattern: args.pattern.clone(),
            grantee: args.who.clone(),
            verbs: args.verbs.clone(),
            expires_in_secs: args.expires.map(|ttl| ttl.as_secs()),
        })
        .await
        .map_err(|failure| {
            not_found_hint(
                failure,
                "name an enrolled agent, a person's email address, agents, people, services or \
                 everyone; a profile is not a grantee (`lead/worker-1` is lead's profile \
                 worker-1: limit it with `openboard profiles limit`)",
            )
        })?;
    ctx.print(&changed, || {
        let mut human = format!(
            "granted {}\nauthorized by your grant {}",
            line(&changed.grant),
            render::safe(&changed.authorized_by)
        );
        if !changed.notices.is_empty() {
            human.push_str(&format!(
                "\nthis gives you read where you could not read before, so the board posted a \
                 notice in {} channel(s): {}",
                changed.notices.len(),
                changed.notices.join(", ")
            ));
        }
        human
    })
}

pub async fn list(ctx: &Ctx<'_>) -> Result<(), Failure> {
    let list = ctx.client.grants().await?;
    ctx.print(&list, || {
        if list.grants.is_empty() {
            return "no grants you manage: you hold no grant with grant.manage on this board"
                .to_owned();
        }
        let rows: Vec<Vec<String>> = list
            .grants
            .iter()
            .map(|grant| {
                vec![
                    grant.id.clone(),
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
        render::table(&["ID", "PATTERN", "WHO", "VERBS", "EXPIRES", "BY"], &rows)
    })
}

pub async fn ungrant(ctx: &Ctx<'_>, args: &UngrantArgs) -> Result<(), Failure> {
    let changed = ctx.client.remove_grant(&args.id).await.map_err(|failure| {
        not_found_hint(
            failure,
            "list the grants you manage with `openboard grants`",
        )
    })?;
    ctx.print(&changed, || format!("removed {}", line(&changed.grant)))
}

pub async fn explain(ctx: &Ctx<'_>, args: &ExplainArgs) -> Result<(), Failure> {
    let explained = ctx
        .client
        .explain(&args.channel, args.for_.as_deref())
        .await?;
    ctx.print(&explained, || {
        let mut human = format!(
            "{} at {}: {}",
            render::safe(&explained.principal),
            render::safe(&explained.channel),
            render::safe(&explained.verbs)
        );
        if explained.narrowed {
            human.push_str(" (narrowed by your profile or token)");
        }
        if explained.deciding.is_empty() {
            human.push_str("\nno grant applies, so no access");
        } else {
            human.push_str("\ndecided by:");
            for grant in &explained.deciding {
                human.push_str(&format!("\n  {}", line(grant)));
            }
        }
        human
    })
}

pub async fn create_channel(ctx: &Ctx<'_>, args: &ChannelCreateArgs) -> Result<(), Failure> {
    let created = ctx
        .client
        .create_channel(&NewChannel {
            path: args.path.clone(),
            private: args.private,
            intake: args.intake,
        })
        .await?;
    ctx.print(&created, || {
        let path = render::safe(&created.path);
        if created.private {
            format!(
                "created private channel {path}: only you can use it; admit others with \
                 `openboard grant {path}/ <who> <verbs>`"
            )
        } else if created.intake {
            format!("created intake channel {path}: tasks cannot be created or claimed in it")
        } else {
            format!("created channel {path}")
        }
    })
}

pub async fn channel_mode(
    ctx: &Ctx<'_>,
    args: &crate::cli::ChannelModeArgs,
) -> Result<(), Failure> {
    let intake = args.mode == crate::cli::ChannelModeArg::Intake;
    let set = ctx
        .client
        .set_channel_mode(&openboard_api::grants::ChannelMode {
            path: args.path.clone(),
            intake,
        })
        .await?;
    ctx.print(&set, || {
        format!(
            "{} is now {} channel",
            render::safe(&set.path),
            if set.intake { "an intake" } else { "a work" }
        )
    })
}

/// `openboard agent token`, `tokens` and `revoke`.
pub async fn agent(ctx: &Ctx<'_>, command: &crate::cli::AgentCommand) -> Result<(), Failure> {
    use crate::cli::AgentCommand;
    use openboard_api::tokens::{NewToken, TokenInfo};

    fn describe(info: &TokenInfo) -> String {
        let credential = &info.credential;
        let mut line = format!(
            "{}  {}  {}  until {}",
            render::safe(&credential.id),
            render::safe(&info.board),
            render::safe(&info.can.join(" ")),
            render::safe(&credential.expires_at)
        );
        if let Some(profile) = &credential.profile {
            line.push_str(&format!("  as {}", render::safe(profile)));
        }
        line
    }

    match command {
        AgentCommand::Token(args) => {
            if !ctx.target.is_network() {
                return Err(Failure::usage(
                    "a local board has no tokens: every agent on it can already do everything",
                )
                .next("derived tokens are for network boards"));
            }
            let board = ctx.global.board().to_owned();
            let minted = ctx
                .client
                .mint_token(&NewToken {
                    board: board.clone(),
                    can: args.can.clone(),
                    ttl_secs: args.ttl.map(|ttl| ttl.as_secs().max(1)),
                    profile: ctx.global.profile.clone().filter(|p| !p.is_empty()),
                })
                .await?;
            #[derive(serde::Serialize)]
            struct Out<'a> {
                #[serde(flatten)]
                minted: &'a openboard_api::tokens::TokenMinted,
                url: &'a str,
                #[serde(skip_serializing_if = "Option::is_none")]
                fingerprint: Option<&'a str>,
            }
            let out = Out {
                minted: &minted,
                url: &ctx.target.url,
                fingerprint: ctx.target.fingerprint.as_deref(),
            };
            ctx.print(&out, || {
                let mut lines = vec![
                    format!("# derived token {}", describe(&minted.info)),
                    format!("export BOARD_URL={}", crate::shell_quote(&ctx.target.url)),
                    match &ctx.target.fingerprint {
                        Some(fingerprint) => format!(
                            "export BOARD_FINGERPRINT={}",
                            crate::shell_quote(fingerprint)
                        ),
                        // A fingerprint left over from another board would
                        // pin this one to the wrong certificate.
                        None => "unset BOARD_FINGERPRINT".to_owned(),
                    },
                    format!("export BOARD_TOKEN={}", crate::shell_quote(&minted.token)),
                    format!("export BOARD_NAME={}", crate::shell_quote(&board)),
                ];
                match &minted.info.credential.profile {
                    Some(profile) => {
                        lines.push(format!("export BOARD_AS={}", crate::shell_quote(profile)))
                    }
                    None => lines.push("unset BOARD_AS".to_owned()),
                }
                lines.push("unset BOARD_LOGIN".to_owned());
                lines.join("\n")
            })
        }
        AgentCommand::Tokens => {
            let list = ctx.client.tokens().await?;
            ctx.print(&list, || {
                if list.tokens.is_empty() {
                    "no live derived tokens".to_owned()
                } else {
                    list.tokens
                        .iter()
                        .map(describe)
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            })
        }
        AgentCommand::Revoke(args) => {
            let revoked = ctx.client.revoke_token(&args.id).await?;
            ctx.print(&revoked, || {
                format!(
                    "revoked {} and every token made from it",
                    render::safe(&revoked.credential.id)
                )
            })
        }
    }
}

/// `openboard profiles`, `profiles limit` and `profiles clear`.
pub async fn profiles(ctx: &Ctx<'_>, args: &crate::cli::ProfilesArgs) -> Result<(), Failure> {
    use crate::cli::ProfilesCommand;
    use openboard_api::grants::SetProfileLimit;

    match &args.command {
        None => {
            let list = ctx.client.profile_limits().await?;
            ctx.print(&list, || {
                if list.profiles.is_empty() {
                    return "no limited profiles on this board".to_owned();
                }
                list.profiles
                    .iter()
                    .map(|limit| {
                        format!(
                            "{}  {}",
                            render::safe(&limit.profile),
                            render::safe(&limit.can.join(" "))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        Some(ProfilesCommand::Limit(limit)) => {
            let set = ctx
                .client
                .set_profile_limit(
                    &limit.profile,
                    &SetProfileLimit {
                        can: limit.can.clone(),
                    },
                )
                .await?;
            ctx.print(&set, || {
                format!(
                    "limited {} on {} to {}",
                    render::safe(&set.profile),
                    render::safe(ctx.global.board()),
                    render::safe(&set.can.join(" "))
                )
            })
        }
        Some(ProfilesCommand::Clear(clear)) => {
            ctx.client.clear_profile_limit(&clear.profile).await?;
            let out = serde_json::json!({ "profile": clear.profile, "limited": false });
            ctx.print(&out, || {
                format!("lifted the limit on {}", render::safe(&clear.profile))
            })
        }
    }
}
