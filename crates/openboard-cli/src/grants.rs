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

pub async fn grant(ctx: &Ctx<'_>, args: &GrantArgs) -> Result<(), Failure> {
    let changed = ctx
        .client
        .add_grant(&NewGrant {
            pattern: args.pattern.clone(),
            grantee: args.who.clone(),
            verbs: args.verbs.clone(),
            expires_in_secs: args.expires.map(|ttl| ttl.as_secs()),
        })
        .await?;
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
    let changed = ctx.client.remove_grant(&args.id).await?;
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
        })
        .await?;
    ctx.print(&created, || {
        let path = render::safe(&created.path);
        if created.private {
            format!(
                "created private channel {path}: only you can use it; admit others with \
                 `openboard grant {path}/ <who> <verbs>`"
            )
        } else {
            format!("created channel {path}")
        }
    })
}
