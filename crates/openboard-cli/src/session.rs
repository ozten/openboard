//! Commands about the caller's own identity on a network board: `join`,
//! `whoami`, `logout` and `logins`.
//!
//! Nothing here prints a token. `join` receives one and hands it straight to
//! the login store.

use std::io::{IsTerminal, Read};
use std::time::Duration;

use openboard_api::auth::{Invite, InviteError, WhoAmI};
use serde::Serialize;

use crate::cli::Global;
use crate::client::Client;
use crate::error::{codes, Failure};
use crate::logins::{same_url, Login, Secret, Store};
use crate::target::{self, Origin, Target, Wanted};
use crate::{render, shell_quote, to_json, write_stdout, Ctx};

/// Longest time spent on a best-effort clean-up request.
const TIDY_TIMEOUT: Duration = Duration::from_secs(10);

/// The invite: the argument, or stdin for `-` or a missing argument with
/// piped input, so that it need not appear in the shell history.
pub fn invite_text(arg: Option<&str>) -> Result<String, Failure> {
    let how = "pass the invite as the argument, or pipe it on stdin: `openboard join < \
               invite.txt`";
    match arg {
        Some(text) if text != "-" => Ok(text.to_owned()),
        _ => {
            let mut stdin = std::io::stdin();
            if arg.is_none() && stdin.is_terminal() {
                return Err(Failure::usage("no invite was given").next(how));
            }
            let mut text = String::new();
            stdin
                .read_to_string(&mut text)
                // The error is not quoted: stdin holds a secret.
                .map_err(|_| Failure::new(codes::IO, "cannot read the invite from stdin"))?;
            if text.trim().is_empty() {
                return Err(Failure::usage("no invite was given").next(how));
            }
            Ok(text)
        }
    }
}

/// The flags that make a later command use this login, given everything
/// that is stored and what the environment already says.
fn selecting_flags(global: &Global, store: &Store, login: &Login) -> String {
    let mut flags = String::new();
    let others_elsewhere = store
        .logins
        .iter()
        .any(|other| !same_url(&other.url, &login.url));
    let env_url_differs = global.url().is_some_and(|url| !same_url(&url, &login.url));
    if others_elsewhere || env_url_differs {
        flags.push_str(&format!(" --url {}", shell_quote(&login.url)));
    }
    let namesakes = store
        .logins
        .iter()
        .filter(|other| same_url(&other.url, &login.url))
        .count();
    let env_login_differs = global
        .login()
        .is_some_and(|name| !crate::logins::same_principal(name, &login.principal));
    if namesakes > 1 || env_login_differs {
        flags.push_str(&format!(" --login {}", shell_quote(&login.principal)));
    }
    flags
}

/// Revokes a credential this client will not keep. Failure is ignored: the
/// token is discarded either way, and it expires by itself.
async fn discard(login: &Login) {
    let Ok(target) = Target::for_login(login) else {
        return;
    };
    if let Ok(client) = Client::new(&target, openboard_api::DEFAULT_BOARD, None) {
        let _ = tokio::time::timeout(TIDY_TIMEOUT, client.logout()).await;
    }
}

pub async fn join(global: &Global, text: &str) -> Result<(), Failure> {
    let invite = Invite::decode(text).map_err(|error| {
        let next = match error {
            InviteError::WrongPrefix | InviteError::Malformed => {
                "copy the whole invite string, which starts with `obi1_`; if it still fails, ask \
                 the board's operator for a new one (`openboard invite <name>` on the board's \
                 host)"
            }
            InviteError::Invalid => {
                "ask the board's operator for a new invite (`openboard invite <name>` on the \
                 board's host)"
            }
        };
        Failure::new(openboard_api::codes::INVITE_INVALID, error.to_string()).next(next)
    })?;
    let url = invite.url.trim_end_matches('/').to_owned();

    // The code works once. Be sure the login can be stored before using it.
    Store::prepare()?;

    let target = Target::for_join(&url, &invite.fingerprint);
    let client = Client::new(&target, openboard_api::DEFAULT_BOARD, None)?;
    let joined = client.join(&invite.code).await?;
    let login = Login {
        url: url.clone(),
        fingerprint: invite.fingerprint.clone(),
        principal: joined.credential.principal.clone(),
        credential_id: joined.credential.id.clone(),
        token: Secret::new(joined.token),
        expires_at: joined.credential.expires_at.clone(),
    };

    let stored = Store::update(|store| {
        let replaced = store.put(login.clone());
        (replaced, selecting_flags(global, store, &login))
    });
    let ((replaced, flags), path) = match stored {
        Ok(stored) => stored,
        Err(mut failure) => {
            // The token cannot be kept, so it should not stay valid either.
            discard(&login).await;
            failure.message = format!(
                "joined {url} as {}, but the login could not be stored: {}",
                login.principal, failure.message
            );
            return Err(failure.next(
                "the invite is used up and its credential was given back; fix the login file's \
                 directory, then ask the board's operator for a new invite",
            ));
        }
    };
    // A login this one replaced is of no use to anyone now.
    let replaced = match replaced {
        Some(old)
            if old.credential_id != login.credential_id && old.fingerprint == login.fingerprint =>
        {
            discard(&old).await;
            Some(old.credential_id)
        }
        Some(old) => Some(old.credential_id),
        None => None,
    };

    let next = format!("openboard{flags} whoami");
    if global.json {
        #[derive(Serialize)]
        struct Joined<'a> {
            url: &'a str,
            fingerprint: &'a str,
            principal: &'a str,
            credential_id: &'a str,
            expires_at: &'a str,
            stored_in: String,
            /// The credential whose stored login this one replaced.
            #[serde(skip_serializing_if = "Option::is_none")]
            replaced: Option<&'a str>,
            next: &'a str,
        }
        write_stdout(&to_json(&Joined {
            url: &login.url,
            fingerprint: &login.fingerprint,
            principal: &login.principal,
            credential_id: &login.credential_id,
            expires_at: &login.expires_at,
            stored_in: path.display().to_string(),
            replaced: replaced.as_deref(),
            next: &next,
        })?)
    } else {
        let mut out = format!(
            "joined {} as {}\ncredential: {}\nexpires: {}\nlogin stored in {}",
            render::safe(&login.url),
            render::safe(&login.principal),
            render::safe(&login.credential_id),
            render::safe(&login.expires_at),
            path.display()
        );
        if let Some(old) = &replaced {
            out.push_str(&format!(
                "\nreplaced the earlier login (credential {})",
                render::safe(old)
            ));
        }
        out.push_str(&format!("\nnext: {next}"));
        write_stdout(&out)
    }
}

pub async fn whoami(ctx: &Ctx<'_>) -> Result<(), Failure> {
    let me = ctx.client.whoami().await?;
    let login = match &ctx.target.origin {
        Origin::Local => "none",
        Origin::Env => "env",
        Origin::Stored(_) => "stored",
    };
    #[derive(Serialize)]
    struct Me<'a> {
        #[serde(flatten)]
        whoami: &'a WhoAmI,
        url: &'a str,
        /// Where the token came from: `stored`, `env` or `none`.
        login: &'a str,
    }
    ctx.print(
        &Me {
            whoami: &me,
            url: &ctx.target.url,
            login,
        },
        || {
            let kind = serde_json::to_value(me.actor.kind)
                .ok()
                .and_then(|kind| kind.as_str().map(str::to_owned))
                .unwrap_or_default();
            let mut out = format!(
                "{} ({kind}) on {}, a {} board",
                render::safe(&me.actor.handle),
                render::safe(&ctx.target.url),
                render::safe(&me.mode)
            );
            match &me.credential {
                Some(credential) => out.push_str(&format!(
                    "\ncredential: {} (expires {})",
                    render::safe(&credential.id),
                    render::safe(&credential.expires_at)
                )),
                None => out.push_str("\nno credential: a local board has no authentication"),
            }
            match &ctx.target.origin {
                Origin::Local => {}
                Origin::Env => out.push_str("\nlogin: BOARD_TOKEN in the environment"),
                Origin::Stored(path) => {
                    out.push_str(&format!("\nlogin: stored in {}", path.display()));
                }
            }
            out
        },
    )
}

#[derive(Serialize)]
struct LoggedOut<'a> {
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    principal: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential_id: Option<&'a str>,
    /// The board revoked the credential in answer to this command.
    revoked: bool,
    /// The stored login was removed.
    removed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stored_in: Option<String>,
}

fn remove_stored(target: &Target) -> Result<bool, Failure> {
    let (Origin::Stored(_), Some(principal)) = (&target.origin, &target.principal) else {
        return Ok(false);
    };
    let (removed, _) = Store::update(|store| store.take(&target.url, principal).is_some())?;
    Ok(removed)
}

fn report_logout(
    json: bool,
    target: &Target,
    revoked: bool,
    removed: bool,
    note: &str,
) -> Result<(), Failure> {
    let stored_in = match &target.origin {
        Origin::Stored(path) => Some(path.display().to_string()),
        _ => None,
    };
    if json {
        return write_stdout(&to_json(&LoggedOut {
            url: &target.url,
            principal: target.principal.as_deref(),
            credential_id: target.credential_id.as_deref(),
            revoked,
            removed,
            stored_in,
        })?);
    }
    let who = match &target.principal {
        Some(principal) => format!("{} at {}", render::safe(principal), target.url),
        None => target.url.clone(),
    };
    let mut out = format!("{note}: {who}");
    if let (true, Some(path)) = (removed, stored_in) {
        out.push_str(&format!("\nremoved the login from {path}"));
    }
    if target.origin == Origin::Env {
        out.push_str("\nunset BOARD_TOKEN: the board no longer accepts it");
    }
    write_stdout(&out)
}

/// `logout`: revoke the credential on the board, then forget it here.
pub async fn logout(ctx: &Ctx<'_>) -> Result<(), Failure> {
    let target = &ctx.target;
    if target.origin == Origin::Local {
        return Err(Failure::usage(format!(
            "{} is a local board: it has no logins, so there is nothing to log out of",
            target.url
        ))
        .next("`openboard logins` lists the network boards you have joined"));
    }
    let (revoked, note) = match ctx.client.logout().await {
        Ok(()) => (true, "logged out, and the board revoked the credential"),
        // The board already refuses this token, which is what logout wants.
        Err(failure) if failure.code == openboard_api::codes::UNAUTHENTICATED => (
            false,
            "the board no longer accepted this login, so it was only forgotten",
        ),
        Err(failure) => {
            let stored = matches!(target.origin, Origin::Stored(_));
            let next = match (&failure.next, stored) {
                (Some(next), true) => Some(format!(
                    "{next}; the stored login was kept. To forget it without contacting the \
                     board, run `{}`",
                    ctx.command("logout --local-only")
                )),
                (None, true) => Some(format!(
                    "the stored login was kept. To forget it without contacting the board, run \
                     `{}`",
                    ctx.command("logout --local-only")
                )),
                (next, false) => next.clone(),
            };
            let mut failure = failure;
            failure.message = format!("could not log out: {}", failure.message);
            failure.next = next;
            return Err(failure);
        }
    };
    let removed = remove_stored(target)?;
    report_logout(ctx.json(), target, revoked, removed, note)
}

/// `logout --local-only`: forget a stored login without contacting the
/// board. `BOARD_TOKEN` is ignored: there is nothing stored behind it.
pub fn forget(global: &Global) -> Result<(), Failure> {
    let wanted = Wanted {
        url: global.url(),
        login: global.login().map(str::to_owned),
        token: None,
        fingerprint: None,
    };
    let target = target::resolve(&wanted, Store::load)?;
    if !remove_stored(&target)? {
        return Err(Failure::usage(match target.origin {
            Origin::Stored(_) => "that login is no longer stored".to_owned(),
            _ => format!("there is no stored login for {} to forget", target.url),
        })
        .next("`openboard logins` lists the stored logins"));
    }
    report_logout(
        global.json,
        &target,
        false,
        true,
        "forgot the login without contacting the board; its token stays valid there until it \
         expires or the operator revokes it",
    )
}

/// `logins`: what is stored, without the tokens.
pub fn logins(json: bool) -> Result<(), Failure> {
    let store = Store::load()?;
    if json {
        #[derive(Serialize)]
        struct Entry<'a> {
            url: &'a str,
            fingerprint: &'a str,
            principal: &'a str,
            credential_id: &'a str,
            expires_at: &'a str,
        }
        #[derive(Serialize)]
        struct Listing<'a> {
            path: String,
            logins: Vec<Entry<'a>>,
        }
        return write_stdout(&to_json(&Listing {
            path: store.path.display().to_string(),
            logins: store
                .logins
                .iter()
                .map(|login| Entry {
                    url: &login.url,
                    fingerprint: &login.fingerprint,
                    principal: &login.principal,
                    credential_id: &login.credential_id,
                    expires_at: &login.expires_at,
                })
                .collect(),
        })?);
    }
    if store.logins.is_empty() {
        return write_stdout(&format!(
            "no stored logins (they are kept in {})\nto join a network board, get an invite \
             from its operator and run `openboard join <invite>`",
            store.path.display()
        ));
    }
    let now = crate::admin::now_rfc3339();
    let rows: Vec<Vec<String>> = store
        .logins
        .iter()
        .map(|login| {
            vec![
                login.principal.clone(),
                login.url.clone(),
                login.credential_id.clone(),
                if login.expires_at <= now {
                    format!("{} (expired)", login.expires_at)
                } else {
                    login.expires_at.clone()
                },
                login.fingerprint.clone(),
            ]
        })
        .collect();
    write_stdout(&format!(
        "{}\nstored in {}",
        render::table(
            &["NAME", "URL", "CREDENTIAL", "EXPIRES", "FINGERPRINT"],
            &rows
        ),
        store.path.display()
    ))
}
