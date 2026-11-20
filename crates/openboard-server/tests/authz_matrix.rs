//! Gate T1, the authorization matrix (milestones M4 and M5).
//!
//! Each scenario gives a subject grants (named, through its group, and to
//! everyone), and sometimes a derived token's scope or an expired grant, on
//! a board with nested and prefix-colliding channels. The subject is an
//! agent with a bearer token, or a person with a browser session: the
//! session cookie, and the anti-CSRF token on every write. An independent
//! model of `docs/permissions.md` says what the subject may do in each
//! channel, and every surface is checked against it:
//!
//! - reads: board and channel listings, thread and task listings, threads
//!   and tasks by ID, events (long-poll, paged, and the server-sent event
//!   stream, which must deliver exactly what the long-poll does, while
//!   events from readable and unreadable channels interleave), `explain`
//!   for oneself and for someone else, and the grant listing;
//! - every action of the "What the actions need" table: post a thread,
//!   reply, edit and delete one's own message, delete someone else's,
//!   label, react, create a channel by posting to a new path, create a
//!   task, make a thread a task, claim and settle, a `task.manage` action,
//!   promote an intake request, change a channel's mode, list grants and
//!   explain someone else's access, each where the model allows it and
//!   where it does not;
//! - the exact status and error code of every refusal, per the Visibility
//!   section: `forbidden` for writes in a channel the subject cannot read,
//!   whether it exists or not, and `not_found` for an ID in such a channel,
//!   the same as for an ID that names nothing;
//! - that nothing sent to the subject, no body, header or stream frame,
//!   holds text from a channel it cannot read or that channel's name.
//!
//! Channel names are matched as whole paths: `buildings` or `build/api` is
//! not an occurrence of `build`. A response may repeat a channel name that
//! the subject's own request named (an error or `explain` echoing it back
//! teaches the subject nothing it did not send), and a grant listing may
//! name the patterns of the grants the subject manages, since the spec lets
//! an administrator manage grants in a channel it cannot read. Instead, a
//! refusal about an existing unreadable channel must be the same as the
//! one about an unreadable path that does not exist, the name aside.
//!
//! Where the server disagrees with the spec, the probe is listed in
//! `KNOWN_GAPS` with what the spec says; the test fails if a listed gap is
//! fixed, so the list stays true.
//!
//! Search and attachments do not exist yet; they join this matrix when
//! they do.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use common::Board;
use openboard_api::web::CSRF_HEADER;
use openboard_server::operator::{self, InviteOptions};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde_json::{json, Value};

const CHANNELS: [&str; 7] = [
    "build",
    "build/api",
    "build/api/v2",
    "buildings",
    "docs",
    "hr/payroll",
    "inbox",
];

/// The one intake channel. The others are work channels.
const INTAKE: &str = "inbox";

/// Paths that never exist, to compare refusals against.
const GHOSTS: [&str; 4] = ["build/api/ghost", "hr/ghost", "docs/ghost", "ghost"];

const MISSING_ID: &str = "msg_00000000deadbeef";

/// Probes where the server disagrees with docs/permissions.md: the
/// scenario, the probe, and what the spec says. Each must still fail; the
/// test says so when one starts to pass.
const KNOWN_GAPS: &[Gap] = &[];

/// Probes where the server disagrees with the spec. A probe ending in
/// ` in *` stands for that probe in every channel.
#[derive(Debug)]
struct Gap {
    scenarios: &'static [&'static str],
    probes: &'static [&'static str],
    spec: &'static str,
    actual: &'static str,
}

fn probe_matches(pattern: &str, label: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => label.starts_with(prefix) && !label.contains(" vs "),
        None => pattern == label,
    }
}

fn secret(channel: &str) -> String {
    format!("secret.{}.end", channel.replace('/', "-"))
}

fn task_secret(channel: &str) -> String {
    format!("tasksecret.{}.end", channel.replace('/', "-"))
}

// ---------------------------------------------------------------------------
// The model: the rules of docs/permissions.md, written again from scratch
// ---------------------------------------------------------------------------

/// Every verb, from the design document's section 9, which
/// docs/permissions.md refers to.
const ALL_VERBS: [&str; 17] = [
    "read",
    "post",
    "react",
    "label",
    "attach",
    "create",
    "task.create",
    "task.claim",
    "task.manage",
    "approve",
    "sync.import",
    "sync.export",
    "grant.manage",
    "integrate.manage",
    "audit.read",
    "export",
    "delete",
];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Kind {
    Exact,
    Subtree,
    Board,
}

fn kind(pattern: &str) -> Kind {
    if pattern == "*" {
        Kind::Board
    } else if pattern.ends_with('/') {
        Kind::Subtree
    } else {
        Kind::Exact
    }
}

fn matches(pattern: &str, channel: &str) -> bool {
    match kind(pattern) {
        Kind::Board => true,
        Kind::Exact => pattern == channel,
        Kind::Subtree => {
            let prefix = pattern.trim_end_matches('/');
            channel == prefix || channel.starts_with(pattern)
        }
    }
}

/// Whether pattern `outer` contains pattern `inner`: authority at `outer`
/// reaches `inner`.
fn contains(outer: &str, inner: &str) -> bool {
    match (kind(outer), kind(inner)) {
        (Kind::Board, _) => true,
        (_, Kind::Board) => false,
        (Kind::Exact, Kind::Exact) => outer == inner,
        (Kind::Exact, Kind::Subtree) => false,
        (Kind::Subtree, _) => matches(outer, inner.trim_end_matches('/')),
    }
}

/// Higher is more specific.
fn specificity(pattern: &str) -> (u8, usize) {
    match kind(pattern) {
        Kind::Exact => (2, pattern.len()),
        Kind::Subtree => (1, pattern.len()),
        Kind::Board => (0, 0),
    }
}

/// A verb list with its shorthands expanded.
fn verbs(raw: &str) -> BTreeSet<String> {
    raw.split(',')
        .flat_map(|verb| match verb {
            "none" => vec![],
            "all" => ALL_VERBS.to_vec(),
            "task" => vec!["task.create", "task.claim"],
            "sync" => vec!["sync.import", "sync.export"],
            "manage" => vec![
                "grant.manage",
                "integrate.manage",
                "audit.read",
                "export",
                "delete",
            ],
            verb => vec![verb],
        })
        .map(str::to_owned)
        .collect()
}

/// How close a grantee is to the principal `me` in group `group`: named,
/// its group, everyone; `None` when the grant does not apply to it.
fn closeness(who: &str, me: &str, group: &str) -> Option<u8> {
    if who == me {
        Some(2)
    } else if who == group {
        Some(1)
    } else if who == "everyone" {
        Some(0)
    } else {
        None
    }
}

/// The verbs `me` (in `group`) holds in `channel`: the most specific
/// matching patterns, then the closest grantees, joined.
fn grant_access_of(
    grants: &[(&str, &str, &str)],
    channel: &str,
    me: &str,
    group: &str,
) -> BTreeSet<String> {
    let applying: Vec<_> = grants
        .iter()
        .filter(|(pattern, who, _)| {
            matches(pattern, channel) && closeness(who, me, group).is_some()
        })
        .collect();
    let Some(best) = applying.iter().map(|(p, _, _)| specificity(p)).max() else {
        return BTreeSet::new();
    };
    let at_best: Vec<_> = applying
        .into_iter()
        .filter(|(p, _, _)| specificity(p) == best)
        .collect();
    let closest = at_best
        .iter()
        .filter_map(|(_, w, _)| closeness(w, me, group))
        .max();
    at_best
        .into_iter()
        .filter(|(_, w, _)| closeness(w, me, group) == closest)
        .flat_map(|(_, _, v)| verbs(v))
        .collect()
}

/// The verbs the agent `subject` holds in `channel`.
fn grant_access(grants: &[(&str, &str, &str)], channel: &str) -> BTreeSet<String> {
    grant_access_of(grants, channel, "subject", "agents")
}

/// A token scope: the most specific matching entries, joined; nothing where
/// none matches.
fn scope_access(scope: &[&str], channel: &str) -> BTreeSet<String> {
    let entries: Vec<(&str, &str)> = scope
        .iter()
        .map(|entry| entry.split_once('=').unwrap())
        .filter(|(pattern, _)| matches(pattern, channel))
        .collect();
    let Some(best) = entries.iter().map(|(p, _)| specificity(p)).max() else {
        return BTreeSet::new();
    };
    entries
        .into_iter()
        .filter(|(p, _)| specificity(p) == best)
        .flat_map(|(_, v)| verbs(v))
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Who {
    Agent,
    Person,
}

impl Who {
    fn group(self) -> &'static str {
        match self {
            Who::Agent => "agents",
            Who::Person => "people",
        }
    }
}

/// Every board grant: the admin's, then the scenario's.
type GrantRow = (&'static str, &'static str, &'static str);

struct Scenario {
    name: &'static str,
    who: Who,
    grants: Vec<GrantRow>,
    /// Made with a one-millisecond lifetime, so expired when checked.
    expired: Vec<GrantRow>,
    scope: Option<Vec<&'static str>>,
}

/// The admin's grants, on the board in every scenario: `all` at `*`, and
/// in each fixed channel by name, so that no `everyone none` in a scenario
/// stops the admin from writing the events the subject is checked on.
const ADMIN_GRANTS: [GrantRow; 8] = [
    ("*", "admin", "all"),
    ("build", "admin", "all"),
    ("build/api", "admin", "all"),
    ("build/api/v2", "admin", "all"),
    ("buildings", "admin", "all"),
    ("docs", "admin", "all"),
    ("hr/payroll", "admin", "all"),
    ("inbox", "admin", "all"),
];

impl Scenario {
    fn access(&self, channel: &str) -> BTreeSet<String> {
        let granted = grant_access_of(&self.grants, channel, "subject", self.who.group());
        match &self.scope {
            None => granted,
            Some(scope) => granted
                .intersection(&scope_access(scope, channel))
                .cloned()
                .collect(),
        }
    }

    /// Whether the subject may do something needing `needs` in `channel`:
    /// every action also needs `read`.
    fn may(&self, channel: &str, needs: &[&str]) -> bool {
        let access = self.access(channel);
        access.contains("read") && needs.iter().all(|verb| access.contains(*verb))
    }

    fn readable(&self, channel: &str) -> bool {
        self.may(channel, &[])
    }

    /// Whether the subject can read the board: whether it may read some
    /// channel, existing or not.
    fn board_readable(&self, existing: &[String]) -> bool {
        let mut paths: Vec<String> = existing.to_vec();
        for (pattern, _, _) in self.grants.iter() {
            let root = pattern.trim_end_matches('/');
            if root != "*" {
                paths.push(root.to_owned());
                paths.push(format!("{root}/zz-child"));
            }
        }
        paths.push("zz-elsewhere".to_owned());
        paths.iter().any(|path| self.readable(path))
    }

    /// The verbs of `bystander`, an agent no grant names, in `channel`.
    fn bystander_access(&self, channel: &str) -> BTreeSet<String> {
        grant_access_of(&self.grants, channel, "bystander", "agents")
    }

    /// The patterns at which the subject holds `grant.manage` through an
    /// active grant naming it or its group. Authority is not shadowed by
    /// narrower grants.
    fn authority(&self) -> Vec<&'static str> {
        self.grants
            .iter()
            .filter(|(_, who, v)| {
                (*who == "subject" || *who == self.who.group()) && verbs(v).contains("grant.manage")
            })
            .map(|(pattern, _, _)| *pattern)
            .collect()
    }

    /// Explaining someone else's access takes `grant.manage` at `*`.
    fn may_explain_others(&self) -> bool {
        self.scope.is_none() && self.authority().contains(&"*")
    }

    /// The grants the subject's grant listing shows: every grant on the
    /// board inside one of its `grant.manage` grants, as
    /// (pattern, grantee, verbs).
    fn listed_grants(&self, handle: &str) -> BTreeSet<(String, String, BTreeSet<String>)> {
        let authority = self.authority();
        let mut all = ADMIN_GRANTS.to_vec();
        all.extend(self.grants.iter().copied());
        all.extend(self.expired.iter().copied());
        all.into_iter()
            .filter(|(pattern, _, _)| authority.iter().any(|held| contains(held, pattern)))
            .map(|(pattern, who, v)| {
                let who = if who == "subject" { handle } else { who };
                (pattern.to_owned(), who.to_owned(), verbs(v))
            })
            .collect()
    }
}

fn scenarios() -> Vec<Scenario> {
    let s = |name, grants: Vec<GrantRow>| Scenario {
        name,
        who: Who::Agent,
        grants,
        expired: Vec::new(),
        scope: None,
    };
    let p = |name, grants: Vec<GrantRow>| Scenario {
        name,
        who: Who::Person,
        grants,
        expired: Vec::new(),
        scope: None,
    };
    vec![
        s("no grants at all", vec![]),
        s(
            "the agents group reads everything",
            vec![("*", "agents", "read")],
        ),
        s(
            "a subtree includes its root and not a prefix collision",
            vec![("build/", "subject", "read,post")],
        ),
        s(
            "an exact pattern is that one channel",
            vec![("build/api", "subject", "read")],
        ),
        s(
            "everyone none closes a subtree to the broader group grant",
            vec![("*", "agents", "read,post"), ("hr/", "everyone", "none")],
        ),
        s(
            "a named none closes one channel",
            vec![("*", "subject", "read,post"), ("docs", "subject", "none")],
        ),
        s(
            "the most specific set replaces the broader one whole",
            vec![("*", "agents", "read"), ("build/", "subject", "post")],
        ),
        s(
            "a named grant beats a group grant at the same pattern",
            vec![
                ("build/", "agents", "read,post"),
                ("build/", "subject", "react"),
            ],
        ),
        s(
            "a group the subject is not in does not apply",
            vec![
                ("build/", "agents", "read"),
                ("build/", "people", "read,post"),
            ],
        ),
        s(
            "tied grants at the same pattern and closeness join",
            vec![("docs", "subject", "read"), ("docs", "subject", "post")],
        ),
        s(
            "a deeper subtree is more specific than a shallower one",
            vec![
                ("build/", "subject", "read,post"),
                ("build/api/", "subject", "none"),
            ],
        ),
        s(
            "an exact pattern beats a subtree of the same channel",
            vec![
                ("build/api/", "subject", "read"),
                ("build/api", "subject", "none"),
            ],
        ),
        Scenario {
            name: "an expired grant is as if absent",
            who: Who::Agent,
            grants: vec![("*", "agents", "read")],
            expired: vec![("*", "subject", "none"), ("docs", "subject", "read,post")],
            scope: None,
        },
        Scenario {
            name: "a token scope narrows to a subtree",
            who: Who::Agent,
            grants: vec![("*", "subject", "read,post")],
            expired: Vec::new(),
            scope: Some(vec!["build/=read"]),
        },
        Scenario {
            name: "a token scope never widens the grants",
            who: Who::Agent,
            grants: vec![("build/", "subject", "read")],
            expired: Vec::new(),
            scope: Some(vec!["*=read,post", "docs=read,post"]),
        },
        Scenario {
            name: "a token scope's most specific entry decides",
            who: Who::Agent,
            grants: vec![("*", "subject", "read,post")],
            expired: Vec::new(),
            scope: Some(vec!["*=read,post", "build/=read", "build/api/v2=none"]),
        },
        // The verbs beyond read and post.
        s(
            "a worker claims, replies and settles but cannot create tasks",
            vec![
                ("*", "subject", "read"),
                ("build/", "subject", "read,post,react,label,task.claim"),
            ],
        ),
        s(
            "task gives create and claim, never in intake",
            vec![
                ("*", "agents", "read,post,task"),
                ("hr/", "everyone", "none"),
            ],
        ),
        s(
            "manage in a subtree gives delete and grant.manage there",
            vec![
                ("*", "subject", "read,post"),
                ("build/", "subject", "read,manage"),
            ],
        ),
        s(
            "all at the board administers a subtree it cannot read",
            vec![("*", "subject", "all"), ("hr/", "everyone", "none")],
        ),
        s(
            "task.create without post makes threads tasks but posts no new one",
            vec![("*", "subject", "read,task.create,task.claim")],
        ),
        s(
            "task.manage alone manages others' tasks",
            vec![("*", "subject", "read,task.manage")],
        ),
        s(
            "create without post cannot open a channel",
            vec![("*", "subject", "read,create")],
        ),
        s(
            "create and post open channels in one subtree",
            vec![
                ("*", "subject", "read"),
                ("build/", "subject", "read,post,create"),
            ],
        ),
        s(
            "delete without post deletes others' messages",
            vec![("*", "agents", "read"), ("docs", "subject", "read,delete")],
        ),
        s(
            "react and label are separate verbs",
            vec![
                ("*", "subject", "read,react"),
                ("docs", "subject", "read,label"),
            ],
        ),
        s(
            "every verb needs read too",
            vec![
                (
                    "*",
                    "subject",
                    "post,react,label,create,task,task.manage,manage",
                ),
                ("docs", "subject", "all"),
            ],
        ),
        s(
            "grant.manage through the agents group",
            vec![
                ("*", "agents", "read,post"),
                ("docs", "agents", "read,grant.manage"),
            ],
        ),
        Scenario {
            name: "a token scope takes away verbs the grants give",
            who: Who::Agent,
            grants: vec![("*", "subject", "read,post,react,label,task,create")],
            expired: Vec::new(),
            scope: Some(vec!["*=read,react", "build/=read,post,task.claim"]),
        },
        // People, signed in with a browser session.
        p("a person with no grants", vec![]),
        p(
            "a person granted by email",
            vec![("build/", "subject", "read,post,react,task")],
        ),
        p(
            "a person through the people group, a subtree closed",
            vec![
                ("*", "people", "read,post,label,create"),
                ("hr/", "everyone", "none"),
            ],
        ),
        p(
            "a grant to agents does not reach a person",
            vec![("*", "agents", "all"), ("docs", "people", "read")],
        ),
        p(
            "a person named beats everyone none",
            vec![
                ("*", "people", "read,post"),
                ("hr/", "everyone", "none"),
                ("hr/payroll", "subject", "read,post,task"),
            ],
        ),
        p(
            "a person administers the board with docs closed",
            vec![("*", "subject", "all"), ("docs", "everyone", "none")],
        ),
        p(
            "people none adds nothing to a tied people grant",
            vec![
                ("build/", "people", "none"),
                ("build/", "people", "read,react"),
                ("*", "people", "read"),
                ("docs", "people", "none"),
            ],
        ),
        Scenario {
            name: "an expired grant to a person is as if absent",
            who: Who::Person,
            grants: vec![("*", "people", "read,label")],
            expired: vec![("docs", "subject", "read,post"), ("*", "people", "none")],
            scope: None,
        },
    ]
}

// ---------------------------------------------------------------------------
// The board
// ---------------------------------------------------------------------------

struct Seeded {
    board: Board,
    admin: String,
    /// Channel → the ID of the thread posted there.
    threads: BTreeMap<&'static str, String>,
    /// Channel → the ID of the task created there.
    tasks: BTreeMap<&'static str, String>,
    /// Every channel on the board, the fixed ones and those created since.
    existing: Vec<String>,
}

async fn admin_write(board: &Board, admin: &str, method: Method, path: &str, body: Value) -> Value {
    let response = board
        .http
        .request(method, board.at(path))
        .bearer_auth(admin)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    assert!(status.is_success(), "admin {path}: {status} {text}");
    serde_json::from_str(&text).unwrap()
}

async fn set_intake(board: &Board, admin: &str, intake: bool) {
    admin_write(
        board,
        admin,
        Method::POST,
        "/v1/boards/general/channels/mode",
        json!({ "path": INTAKE, "intake": intake }),
    )
    .await;
}

async fn seeded() -> Seeded {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    for (pattern, who, verbs) in ADMIN_GRANTS {
        board.grant(pattern, who, verbs);
    }
    board.enrol("bystander").await;
    let mut threads = BTreeMap::new();
    let mut tasks = BTreeMap::new();
    for channel in CHANNELS {
        let posted = board.posted(&admin, channel, &secret(channel)).await;
        threads.insert(channel, posted.message.id);
        let created = admin_write(
            &board,
            &admin,
            Method::POST,
            "/v1/boards/general/tasks",
            json!({ "channel": channel, "text": task_secret(channel) }),
        )
        .await;
        tasks.insert(channel, created["task"]["id"].as_str().unwrap().to_owned());
    }
    set_intake(&board, &admin, true).await;
    Seeded {
        board,
        admin,
        threads,
        tasks,
        existing: CHANNELS.iter().map(|c| (*c).to_owned()).collect(),
    }
}

/// What the admin makes in each channel for one scenario's subject to act
/// on, so that no probe changes what another scenario sees.
struct Targets {
    /// A reply by the admin: labelled, reacted to, then deleted.
    reply: String,
    /// A thread by the admin, to make a task of.
    thread: String,
    /// A task by the admin, to claim and settle.
    claim: String,
    /// A task by the admin, to block (a `task.manage` action).
    manage: String,
}

async fn targets(seeded: &Seeded) -> BTreeMap<&'static str, Targets> {
    let (board, admin) = (&seeded.board, seeded.admin.as_str());
    // Tasks cannot be created in an intake channel, so it is a work
    // channel while they are made.
    set_intake(board, admin, false).await;
    let mut made = BTreeMap::new();
    for channel in CHANNELS {
        let post = |body: Value| {
            admin_write(
                board,
                admin,
                Method::POST,
                "/v1/boards/general/messages",
                body,
            )
        };
        let task = |text: String| {
            admin_write(
                board,
                admin,
                Method::POST,
                "/v1/boards/general/tasks",
                json!({ "channel": channel, "text": text }),
            )
        };
        let id = |value: &Value, at: &str| value[at]["id"].as_str().unwrap().to_owned();
        let reply = post(json!({
            "thread_id": seeded.threads[channel],
            "text": format!("{} victim", secret(channel)),
        }))
        .await;
        let thread = post(json!({ "channel": channel, "text": secret(channel) })).await;
        let claim = task(task_secret(channel)).await;
        let manage = task(task_secret(channel)).await;
        made.insert(
            channel,
            Targets {
                reply: id(&reply, "message"),
                thread: id(&thread, "message"),
                claim: id(&claim, "task"),
                manage: id(&manage, "task"),
            },
        );
    }
    set_intake(board, admin, true).await;
    made
}

// ---------------------------------------------------------------------------
// The subject and what it is sent
// ---------------------------------------------------------------------------

enum Credential {
    Bearer(String),
    /// A browser session: the `Cookie` header, and the anti-CSRF token sent
    /// on every write.
    Browser {
        cookie: String,
        csrf: String,
    },
}

/// Invites a person and signs them in, as the sign-in page does.
async fn sign_in(board: &Board, email: &str) -> Credential {
    let invite =
        operator::create_person_invite(&board.data_dir(), email, &InviteOptions::default())
            .unwrap();
    let response = board
        .http
        .post(board.at("/v1/web/login"))
        .json(&json!({ "code": invite.code }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let session: Value = response.json().await.unwrap();
    Credential::Browser {
        cookie,
        csrf: session["csrf_token"].as_str().unwrap().to_owned(),
    }
}

#[derive(Clone, Copy, Debug)]
enum Expect {
    /// Any 2xx.
    Ok,
    /// This status, with one of these error codes.
    Denied(u16, &'static [&'static str]),
}

const NOT_FOUND: Expect = Expect::Denied(404, &["not_found"]);
const FORBIDDEN: Expect = Expect::Denied(403, &["forbidden"]);
const INTAKE_REFUSED: Expect = Expect::Denied(409, &["intake_channel"]);

struct Reply {
    status: StatusCode,
    code: String,
    message: String,
    value: Value,
    text: String,
}

impl Reply {
    fn ok(&self) -> bool {
        self.status.is_success()
    }
}

/// One request and what came back, kept to search for leaks.
struct Exchange {
    label: String,
    /// The path and body the subject sent.
    sent: String,
    /// Every header and the body it received.
    received: String,
}

/// A disagreement with the model: scenario, probe, detail.
type Mismatch = (String, String, String);

struct Run<'a> {
    name: &'static str,
    board: &'a Board,
    credential: Credential,
    seen: Vec<Exchange>,
    /// Channel names a response may contain without leaking them.
    allowed_names: BTreeSet<String>,
    mismatches: Vec<Mismatch>,
    /// Probe kind → how often the model expected it allowed and denied.
    tally: BTreeMap<String, (usize, usize)>,
    /// How many checks were made.
    probes: usize,
}

impl Run<'_> {
    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let write = method != Method::GET && method != Method::HEAD;
        let request = self.board.http.request(method, self.board.at(path));
        match &self.credential {
            Credential::Bearer(token) => request.bearer_auth(token),
            Credential::Browser { cookie, csrf } => {
                let request = request.header("cookie", cookie);
                if write {
                    request.header(CSRF_HEADER, csrf)
                } else {
                    request
                }
            }
        }
    }

    async fn record(&mut self, label: &str, sent: String, response: Response) -> Reply {
        let status = response.status();
        let mut received = String::new();
        for (name, value) in response.headers() {
            received.push_str(&format!("{name}: {}\n", value.to_str().unwrap_or("")));
        }
        let text = response.text().await.unwrap();
        received.push_str(&text);
        self.seen.push(Exchange {
            label: label.to_owned(),
            sent,
            received,
        });
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Reply {
            status,
            code: value["error"]["code"].as_str().unwrap_or("").to_owned(),
            message: value["error"]["message"].as_str().unwrap_or("").to_owned(),
            value,
            text,
        }
    }

    async fn call(
        &mut self,
        label: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Reply {
        let mut request = self.request(method, path);
        let mut sent = path.to_owned();
        if let Some(body) = &body {
            sent.push('\n');
            sent.push_str(&body.to_string());
            request = request.json(body);
        }
        let response = request.send().await.unwrap();
        self.record(label, sent, response).await
    }

    async fn get(&mut self, label: &str, path: &str) -> Reply {
        self.call(label, Method::GET, path, None).await
    }

    async fn post(&mut self, label: &str, path: &str, body: Value) -> Reply {
        self.call(label, Method::POST, path, Some(body)).await
    }

    fn fail(&mut self, label: &str, detail: String) {
        self.mismatches
            .push((self.name.to_owned(), label.to_owned(), detail));
    }

    fn check(&mut self, label: &str, ok: bool, detail: impl FnOnce() -> String) {
        self.probes += 1;
        if !ok {
            self.fail(label, detail());
        }
    }

    /// Checks a reply against what the model expects. True when it agrees.
    fn want(&mut self, label: &str, reply: &Reply, expect: Expect) -> bool {
        self.probes += 1;
        let kind = label.split(" in ").next().unwrap().to_owned();
        let counts = self.tally.entry(kind).or_default();
        match expect {
            Expect::Ok => counts.0 += 1,
            Expect::Denied(..) => counts.1 += 1,
        }
        let agrees = match expect {
            Expect::Ok => reply.ok(),
            Expect::Denied(status, codes) => {
                reply.status.as_u16() == status && codes.contains(&reply.code.as_str())
            }
        };
        if !agrees {
            let shown: String = reply.text.chars().take(300).collect();
            self.fail(
                label,
                format!("expected {expect:?}, got {} {shown}", reply.status),
            );
        }
        agrees
    }

    /// Every item of a paged listing.
    async fn all_pages(&mut self, label: &str, path: &str, key: &str) -> (Reply, Vec<Value>) {
        let mut items = Vec::new();
        let mut first = None;
        let mut next: Option<String> = None;
        loop {
            let url = match &next {
                None => path.to_owned(),
                Some(cursor) => format!("{path}&cursor={cursor}"),
            };
            let reply = self.get(label, &url).await;
            if !reply.ok() {
                return (reply, items);
            }
            items.extend(reply.value[key].as_array().cloned().unwrap_or_default());
            next = reply.value["next_cursor"].as_str().map(str::to_owned);
            if first.is_none() {
                first = Some(reply);
            }
            if next.is_none() {
                return (first.unwrap(), items);
            }
        }
    }

    /// Every event from the start by long-poll, page by page.
    async fn all_events(&mut self) -> (Reply, Vec<Value>) {
        let mut events = Vec::new();
        let mut after = "start".to_owned();
        let mut first = None;
        loop {
            let reply = self
                .get(
                    "events",
                    &format!("/v1/boards/general/events?after={after}&limit=1000"),
                )
                .await;
            if !reply.ok() {
                return (reply, events);
            }
            let page = reply.value["events"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            after = reply.value["cursor"].as_str().unwrap().to_owned();
            let done = page.is_empty();
            events.extend(page);
            if first.is_none() {
                first = Some(reply);
            }
            if done {
                return (first.unwrap(), events);
            }
        }
    }
}

fn channels_of(items: &[Value], at: &[&str]) -> BTreeSet<String> {
    items
        .iter()
        .map(|item| {
            let mut value = item;
            for key in at {
                value = &value[*key];
            }
            value.as_str().unwrap().to_owned()
        })
        .collect()
}

/// Whether `haystack` names the channel `name` as a whole path: not as part
/// of a longer name (`buildings`) or a deeper path (`build/api`).
fn names_channel(haystack: &str, name: &str) -> bool {
    let is_path_char =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '/');
    haystack.match_indices(name).any(|(at, _)| {
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + name.len()..].chars().next();
        !before.is_some_and(is_path_char) && !after.is_some_and(is_path_char)
    })
}

/// The SSE frames in `text`: each event's data.
fn frames(text: &str) -> Vec<Value> {
    text.split("\n\n")
        .filter_map(|block| {
            block
                .lines()
                .find_map(|line| line.strip_prefix("data:"))
                .and_then(|data| serde_json::from_str(data.trim_start()).ok())
        })
        .collect()
}

/// Reads an event stream until `until` is in it, followed by the end of
/// its frame, or ten seconds pass.
async fn read_stream(mut response: Response, until: &str) -> String {
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(at) = text.find(until) {
            if text[at..].contains("\n\n") {
                return text;
            }
        }
        match tokio::time::timeout_at(deadline, response.chunk()).await {
            Ok(Ok(Some(bytes))) => text.push_str(&String::from_utf8_lossy(&bytes)),
            _ => return text,
        }
    }
}

// ---------------------------------------------------------------------------
// The probes
// ---------------------------------------------------------------------------

async fn check(seeded: &mut Seeded, scenario: &Scenario, index: usize) -> Outcome {
    let board = &seeded.board;
    let handle = match scenario.who {
        Who::Agent => format!("subject-{index}"),
        Who::Person => format!("subject-{index}@example.com"),
    };
    let credential = match scenario.who {
        Who::Agent => Credential::Bearer(board.enrol(&handle).await),
        Who::Person => sign_in(board, &handle).await,
    };
    let rename = |who: &str| {
        if who == "subject" {
            handle.as_str()
        } else {
            who
        }
        .to_owned()
    };
    // Removed at the end: group grants would reach the next subject too.
    let mut made = Vec::new();
    for (pattern, who, verbs) in &scenario.grants {
        made.push(board.grant(pattern, &rename(who), verbs));
    }
    for (pattern, who, verbs) in &scenario.expired {
        let expired = operator::grant(
            &board.data_dir(),
            "general",
            pattern,
            &rename(who),
            verbs,
            Some(Duration::from_millis(1)),
        )
        .unwrap();
        made.push(expired.id);
    }
    if !scenario.expired.is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let credential = match (&scenario.scope, credential) {
        (None, credential) => credential,
        (Some(scope), Credential::Bearer(token)) => {
            let minted: Value = board
                .mint(&token, json!({ "board": "general", "can": scope }))
                .await
                .json()
                .await
                .unwrap();
            Credential::Bearer(minted["token"].as_str().unwrap().to_owned())
        }
        (Some(_), Credential::Browser { .. }) => panic!("scopes are for agents' tokens"),
    };
    let targets = targets(seeded).await;
    let board = &seeded.board;
    let mut run = Run {
        name: scenario.name,
        board,
        credential,
        seen: Vec::new(),
        allowed_names: BTreeSet::new(),
        mismatches: Vec::new(),
        tally: BTreeMap::new(),
        probes: 0,
    };
    let readable = |channel: &str| scenario.readable(channel);
    let board_readable = scenario.board_readable(&seeded.existing);
    // An ID in a channel the subject cannot read is `not_found`, as one that
    // names nothing; on a board it cannot read at all, `forbidden`.
    let id_denied = if board_readable { NOT_FOUND } else { FORBIDDEN };
    let readable_existing: BTreeSet<String> = seeded
        .existing
        .iter()
        .filter(|c| readable(c))
        .cloned()
        .collect();

    // Boards, and the board's newest activity.
    let boards = run.get("boards", "/v1/boards").await;
    run.want("boards", &boards, Expect::Ok);
    let listed = boards.value["boards"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    run.check("boards", listed.is_empty() != board_readable, || {
        format!("board listed: {}", boards.text)
    });

    // Channels and their counts.
    let channels = run.get("channels", "/v1/boards/general/channels").await;
    if board_readable {
        if run.want("channels", &channels, Expect::Ok) {
            let got = channels_of(channels.value["channels"].as_array().unwrap(), &["path"]);
            run.check("channel listing", got == readable_existing, || {
                format!("listed {got:?}, model {readable_existing:?}")
            });
            // The name matcher finds what is there: a control for the leak
            // check below.
            for channel in &readable_existing {
                let named = names_channel(&channels.text, channel);
                run.check("channel listing names", named, || {
                    format!("{channel} not found by name in {}", channels.text)
                });
            }
        }
    } else {
        run.want("channels", &channels, FORBIDDEN);
    }

    // A board that does not exist answers as one the subject cannot read.
    let ghost_board = run
        .get("ghost board", "/v1/boards/ghostboard/channels")
        .await;
    run.want(
        "ghost board",
        &ghost_board,
        Expect::Denied(403, &["forbidden"]),
    );
    let ghost_board_id = run
        .get(
            "ghost board thread",
            &format!("/v1/boards/ghostboard/threads/{MISSING_ID}"),
        )
        .await;
    run.want("ghost board thread", &ghost_board_id, FORBIDDEN);
    for (label, method, path, body) in [
        (
            "ghost board task",
            Method::GET,
            format!("/v1/boards/ghostboard/tasks/{MISSING_ID}"),
            None,
        ),
        (
            "ghost board grants",
            Method::GET,
            "/v1/boards/ghostboard/grants".to_owned(),
            None,
        ),
        (
            "ghost board reply",
            Method::POST,
            "/v1/boards/ghostboard/messages".to_owned(),
            Some(json!({ "thread_id": MISSING_ID, "text": "a reply" })),
        ),
    ] {
        let reply = run.call(label, method, &path, body).await;
        run.want(label, &reply, FORBIDDEN);
    }

    // Thread and event listings by channel, existing and not.
    let mut refusals: BTreeMap<String, Vec<(String, Reply)>> = BTreeMap::new();
    for channel in CHANNELS.iter().chain(GHOSTS.iter()) {
        let ghost = GHOSTS.contains(channel);
        for (kind, path) in [
            (
                "threads",
                format!("/v1/boards/general/threads?channel={channel}"),
            ),
            (
                "events",
                format!("/v1/boards/general/events?after=start&channel={channel}&limit=1000"),
            ),
        ] {
            let label = format!("{kind} in {channel}");
            let reply = run.get(&label, &path).await;
            if readable(channel) {
                if run.want(&label, &reply, Expect::Ok) && !ghost {
                    let key = kind;
                    let at: &[&str] = if kind == "threads" {
                        &["root", "channel"]
                    } else {
                        &["channel"]
                    };
                    let got = channels_of(reply.value[key].as_array().unwrap(), at);
                    run.check(
                        &label,
                        got == BTreeSet::from([(*channel).to_owned()]),
                        || format!("listed {got:?}"),
                    );
                }
            } else {
                run.want(&label, &reply, FORBIDDEN);
                refusals
                    .entry(kind.to_owned())
                    .or_default()
                    .push(((*channel).to_owned(), reply));
            }
        }
    }
    let (subtree, items) = run
        .all_pages(
            "threads build/",
            "/v1/boards/general/threads?channel=build/&limit=500",
            "threads",
        )
        .await;
    if board_readable {
        if run.want("threads build/", &subtree, Expect::Ok) {
            let got = channels_of(&items, &["root", "channel"]);
            let want: BTreeSet<String> = readable_existing
                .iter()
                .filter(|c| matches("build/", c))
                .cloned()
                .collect();
            run.check("subtree listing", got == want, || {
                format!("listed {got:?}, model {want:?}")
            });
        }
    } else {
        run.want("threads build/", &subtree, FORBIDDEN);
    }

    // Threads, tasks and events by ID: unreadable answers exactly as
    // missing does.
    for (what, path) in [
        (
            "get thread",
            format!("/v1/boards/general/threads/{MISSING_ID}"),
        ),
        ("get task", format!("/v1/boards/general/tasks/{MISSING_ID}")),
        (
            "events of thread",
            format!("/v1/boards/general/events?thread={MISSING_ID}"),
        ),
    ] {
        let label = format!("{what} missing");
        let reply = run.get(&label, &path).await;
        run.want(&label, &reply, id_denied);
    }
    for channel in CHANNELS {
        for (what, path) in [
            (
                "get thread",
                format!("/v1/boards/general/threads/{}", seeded.threads[channel]),
            ),
            (
                "get task",
                format!("/v1/boards/general/tasks/{}", seeded.tasks[channel]),
            ),
            (
                "events of thread",
                format!(
                    "/v1/boards/general/events?thread={}",
                    seeded.threads[channel]
                ),
            ),
        ] {
            let label = format!("{what} in {channel}");
            let reply = run.get(&label, &path).await;
            let expect = if readable(channel) {
                Expect::Ok
            } else {
                id_denied
            };
            run.want(&label, &reply, expect);
        }
    }

    // Tasks listed across the board.
    let (tasks, items) = run
        .all_pages(
            "tasks",
            "/v1/boards/general/tasks?state=all&limit=500",
            "tasks",
        )
        .await;
    if board_readable {
        if run.want("tasks", &tasks, Expect::Ok) {
            let got = channels_of(&items, &["root", "channel"]);
            let want: BTreeSet<String> = CHANNELS
                .iter()
                .filter(|c| readable(c))
                .map(|c| (*c).to_owned())
                .collect();
            run.check("task listing", got == want, || {
                format!("listed {got:?}, model {want:?}")
            });
        }
    } else {
        run.want("tasks", &tasks, FORBIDDEN);
    }

    // `explain` agrees with the model wherever it is asked, for oneself and,
    // with grant.manage at `*`, for someone else.
    for channel in CHANNELS {
        let label = format!("explain in {channel}");
        let explained = run
            .get(
                &label,
                &format!("/v1/boards/general/explain?channel={channel}"),
            )
            .await;
        if run.want(&label, &explained, Expect::Ok) {
            let got = verbs(explained.value["verbs"].as_str().unwrap());
            let want = scenario.access(channel);
            run.check(&label, got == want, || {
                format!("got {got:?}, model {want:?}")
            });
        }
        let label = format!("explain bystander in {channel}");
        let explained = run
            .get(
                &label,
                &format!("/v1/boards/general/explain?channel={channel}&for=bystander"),
            )
            .await;
        if scenario.may_explain_others() {
            if run.want(&label, &explained, Expect::Ok) {
                let got = verbs(explained.value["verbs"].as_str().unwrap());
                let want = scenario.bystander_access(channel);
                run.check(&label, got == want, || {
                    format!("got {got:?}, model {want:?}")
                });
            }
        } else {
            run.want(
                &label,
                &explained,
                Expect::Denied(403, &["no_authority", "forbidden"]),
            );
        }
    }

    // The grants the subject manages.
    let listing = run.get("grants", "/v1/boards/general/grants").await;
    if !board_readable {
        run.want("grants", &listing, FORBIDDEN);
    } else if scenario.scope.is_none() && run.want("grants", &listing, Expect::Ok) {
        let got: BTreeSet<(String, String, BTreeSet<String>)> = listing.value["grants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|grant| {
                (
                    grant["pattern"].as_str().unwrap().to_owned(),
                    grant["grantee"].as_str().unwrap().to_owned(),
                    verbs(grant["verbs"].as_str().unwrap()),
                )
            })
            .collect();
        let want = scenario.listed_grants(&handle);
        for (pattern, _, _) in &want {
            run.allowed_names
                .insert(pattern.trim_end_matches('/').to_owned());
        }
        run.check("grant listing", got == want, || {
            format!("listed {got:?}, model {want:?}")
        });
    }

    // ---- Writes. Each probe is checked allowed and denied. ----

    // Missing IDs, for every route that takes one.
    let missing_probes: Vec<(&str, Method, String, Option<Value>)> = vec![
        (
            "reply to missing",
            Method::POST,
            "/v1/boards/general/messages".into(),
            Some(json!({ "thread_id": MISSING_ID, "text": "a reply" })),
        ),
        (
            "react to missing",
            Method::PUT,
            format!("/v1/boards/general/messages/{MISSING_ID}/reactions/thumbsup"),
            None,
        ),
        (
            "label missing",
            Method::POST,
            format!("/v1/boards/general/messages/{MISSING_ID}/labels"),
            Some(json!({ "set": { "matrix": "probe" } })),
        ),
        (
            "delete missing",
            Method::DELETE,
            format!("/v1/boards/general/messages/{MISSING_ID}"),
            None,
        ),
        (
            "make missing a task",
            Method::POST,
            "/v1/boards/general/tasks".into(),
            Some(json!({ "thread_id": MISSING_ID })),
        ),
        (
            "claim missing",
            Method::POST,
            format!("/v1/boards/general/tasks/{MISSING_ID}/claim"),
            None,
        ),
        (
            "block missing",
            Method::POST,
            format!("/v1/boards/general/tasks/{MISSING_ID}/block"),
            Some(json!({ "reason": "matrix probe" })),
        ),
    ];
    for (label, method, path, body) in missing_probes {
        let reply = run.call(label, method, &path, body).await;
        run.want(label, &reply, id_denied);
    }

    for channel in CHANNELS {
        let read = readable(channel);
        let intake = channel == INTAKE;
        let target = &targets[channel];

        // A new thread in an existing channel: post.
        let label = format!("thread in {channel}");
        let reply = run
            .post(
                &label,
                "/v1/boards/general/messages",
                json!({ "channel": channel, "text": "a matrix thread" }),
            )
            .await;
        let expect = if scenario.may(channel, &["post"]) {
            Expect::Ok
        } else {
            FORBIDDEN
        };
        run.want(&label, &reply, expect);
        if !read {
            refusals
                .entry("thread".into())
                .or_default()
                .push((channel.to_owned(), reply));
        }

        // A reply: post; then one's own message is edited and deleted.
        let label = format!("reply in {channel}");
        let reply = run
            .post(
                &label,
                "/v1/boards/general/messages",
                json!({ "thread_id": seeded.threads[channel], "text": "a reply" }),
            )
            .await;
        let expect = match (read, scenario.may(channel, &["post"])) {
            (false, _) => id_denied,
            (true, true) => Expect::Ok,
            (true, false) => FORBIDDEN,
        };
        if run.want(&label, &reply, expect) && reply.ok() {
            let own = reply.value["message"]["id"].as_str().unwrap().to_owned();
            let path = format!("/v1/boards/general/messages/{own}");
            let label = format!("edit own in {channel}");
            let edited = run
                .call(
                    &label,
                    Method::PATCH,
                    &path,
                    Some(json!({ "text": "edited" })),
                )
                .await;
            run.want(&label, &edited, Expect::Ok);
            let label = format!("delete own in {channel}");
            let deleted = run.call(&label, Method::DELETE, &path, None).await;
            run.want(&label, &deleted, Expect::Ok);
        }

        // React, label, and delete someone else's message.
        let on_message = |verb: &str| match (read, scenario.may(channel, &[verb])) {
            (false, _) => id_denied,
            (true, true) => Expect::Ok,
            (true, false) => FORBIDDEN,
        };
        let label = format!("react in {channel}");
        let reply = run
            .call(
                &label,
                Method::PUT,
                &format!(
                    "/v1/boards/general/messages/{}/reactions/thumbsup",
                    target.reply
                ),
                None,
            )
            .await;
        run.want(&label, &reply, on_message("react"));
        let label = format!("label in {channel}");
        let reply = run
            .post(
                &label,
                &format!("/v1/boards/general/messages/{}/labels", target.reply),
                json!({ "set": { "matrix": "probe" } }),
            )
            .await;
        run.want(&label, &reply, on_message("label"));
        let label = format!("delete other's in {channel}");
        let reply = run
            .call(
                &label,
                Method::DELETE,
                &format!("/v1/boards/general/messages/{}", target.reply),
                None,
            )
            .await;
        run.want(&label, &reply, on_message("delete"));

        // A new task posts a thread: post and task.create, not in intake.
        let new_task = || {
            if !scenario.may(channel, &["post", "task.create"]) {
                FORBIDDEN
            } else if intake {
                INTAKE_REFUSED
            } else {
                Expect::Ok
            }
        };
        let label = format!("task in {channel}");
        let reply = run
            .post(
                &label,
                "/v1/boards/general/tasks",
                json!({ "channel": channel, "text": "a matrix task" }),
            )
            .await;
        run.want(&label, &reply, new_task());
        if !read {
            refusals
                .entry("task".into())
                .or_default()
                .push((channel.to_owned(), reply));
        }

        // Promoting the intake request into a task here: as a new task,
        // and read on the request.
        let label = format!("promote in {channel}");
        let reply = run
            .post(
                &label,
                "/v1/boards/general/tasks",
                json!({
                    "channel": channel,
                    "text": "a matrix promotion",
                    "origin": seeded.threads[INTAKE],
                }),
            )
            .await;
        let expect = match new_task() {
            Expect::Ok if !readable(INTAKE) => NOT_FOUND,
            other => other,
        };
        if run.want(&label, &reply, expect) && reply.ok() {
            let origin = &reply.value["task"]["labels"]["board.origin"];
            run.check(&label, origin == seeded.threads[INTAKE].as_str(), || {
                format!("the task carries origin {origin}")
            });
        }

        // Making a thread a task: task.create, not in intake.
        let label = format!("make a task in {channel}");
        let reply = run
            .post(
                &label,
                "/v1/boards/general/tasks",
                json!({ "thread_id": target.thread }),
            )
            .await;
        let expect = match (read, scenario.may(channel, &["task.create"]), intake) {
            (false, _, _) => id_denied,
            (true, false, _) => FORBIDDEN,
            (true, true, true) => INTAKE_REFUSED,
            (true, true, false) => Expect::Ok,
        };
        run.want(&label, &reply, expect);

        // Claim and settle: task.claim, not in intake.
        let task_path = format!("/v1/boards/general/tasks/{}", target.claim);
        let label = format!("claim in {channel}");
        let claimed = run
            .post(&label, &format!("{task_path}/claim"), json!({}))
            .await;
        let may_claim = scenario.may(channel, &["task.claim"]);
        let expect = match (read, may_claim, intake) {
            (false, _, _) => id_denied,
            (true, false, _) => FORBIDDEN,
            (true, true, true) => INTAKE_REFUSED,
            (true, true, false) => Expect::Ok,
        };
        run.want(&label, &claimed, expect);
        let label = format!("settle in {channel}");
        if claimed.ok() {
            let proof = json!({
                "epoch": claimed.value["epoch"],
                "token": claimed.value["token"],
            });
            let settled = run
                .post(&label, &format!("{task_path}/complete"), proof)
                .await;
            run.want(&label, &settled, Expect::Ok);
        } else if !(read && may_claim) {
            let settled = run
                .post(
                    &label,
                    &format!("{task_path}/complete"),
                    json!({ "epoch": 1, "token": "not-a-claim" }),
                )
                .await;
            run.want(&label, &settled, if read { FORBIDDEN } else { id_denied });
        }

        // Blocking a task someone else made: task.manage.
        let label = format!("block other's task in {channel}");
        let reply = run
            .post(
                &label,
                &format!("/v1/boards/general/tasks/{}/block", target.manage),
                json!({ "reason": "matrix probe" }),
            )
            .await;
        let expect = match (read, scenario.may(channel, &["task.manage"])) {
            (false, _) => id_denied,
            (true, true) => Expect::Ok,
            (true, false) => Expect::Denied(403, &["forbidden", "not_task_creator"]),
        };
        run.want(&label, &reply, expect);

        // Changing the channel's mode, to what it is: grant.manage there,
        // never through a derived token.
        let label = format!("mode in {channel}");
        let reply = run
            .post(
                &label,
                "/v1/boards/general/channels/mode",
                json!({ "path": channel, "intake": intake }),
            )
            .await;
        let expect = if !read {
            FORBIDDEN
        } else if scenario.scope.is_some() {
            Expect::Denied(403, &["derived_token", "no_authority"])
        } else if scenario.may(channel, &["grant.manage"]) {
            Expect::Ok
        } else {
            Expect::Denied(403, &["no_authority", "forbidden"])
        };
        run.want(&label, &reply, expect);
        if !read {
            refusals
                .entry("mode".into())
                .or_default()
                .push((channel.to_owned(), reply));
        }
    }

    // Writes addressed to paths that do not exist, where the subject cannot
    // read: refused as for an existing channel.
    for ghost in GHOSTS.into_iter().filter(|g| !readable(g)) {
        let label = format!("thread in {ghost}");
        let reply = run
            .post(
                &label,
                "/v1/boards/general/messages",
                json!({ "channel": ghost, "text": "a matrix thread" }),
            )
            .await;
        run.want(&label, &reply, FORBIDDEN);
        refusals
            .entry("thread".into())
            .or_default()
            .push((ghost.to_owned(), reply));
        let label = format!("task in {ghost}");
        let reply = run
            .post(
                &label,
                "/v1/boards/general/tasks",
                json!({ "channel": ghost, "text": "a matrix task" }),
            )
            .await;
        run.want(&label, &reply, FORBIDDEN);
        refusals
            .entry("task".into())
            .or_default()
            .push((ghost.to_owned(), reply));
        let label = format!("mode in {ghost}");
        let reply = run
            .post(
                &label,
                "/v1/boards/general/channels/mode",
                json!({ "path": ghost, "intake": false }),
            )
            .await;
        run.want(&label, &reply, FORBIDDEN);
        refusals
            .entry("mode".into())
            .or_default()
            .push((ghost.to_owned(), reply));
    }

    // A refusal about an existing unreadable channel is the one about a
    // path that does not exist, the name aside.
    for (kind, replies) in &refusals {
        let (existing, ghosts): (Vec<_>, Vec<_>) = replies
            .iter()
            .partition(|(channel, _)| !GHOSTS.contains(&channel.as_str()));
        let Some((ghost, ghost_reply)) = ghosts.first() else {
            continue;
        };
        let shape = |channel: &str, reply: &Reply| {
            (
                reply.status,
                reply.code.clone(),
                reply.message.replace(channel, "<channel>"),
            )
        };
        let want = shape(ghost, ghost_reply);
        for (channel, reply) in existing {
            let label = format!("{kind} in {channel} vs {ghost}");
            let got = shape(channel, reply);
            run.check(&label, got == want, || {
                format!("existing {got:?}, not existing {want:?}")
            });
        }
    }

    // Creating a channel by posting to a new path: create and post.
    for path in [
        format!("build/fresh-{index}"),
        format!("buildings/fresh-{index}"),
        format!("docs/fresh-{index}"),
        format!("hr/fresh-{index}"),
        format!("fresh-{index}"),
    ] {
        let label = format!("create {}", path.replace(&index.to_string(), "N"));
        let reply = run
            .post(
                &label,
                "/v1/boards/general/messages",
                json!({ "channel": path, "text": "a new channel" }),
            )
            .await;
        let expect = if scenario.may(&path, &["post", "create"]) {
            Expect::Ok
        } else {
            FORBIDDEN
        };
        run.want(&label, &reply, expect);
        if reply.ok() {
            seeded.existing.push(path);
        }
    }
    let readable_existing: BTreeSet<String> = seeded
        .existing
        .iter()
        .filter(|c| readable(c))
        .cloned()
        .collect();

    // ---- Events: the stream delivers what the long-poll does. ----

    let stream = run
        .request(Method::GET, "/v1/boards/general/events/stream?after=start")
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    if !board_readable {
        let reply = run.record("stream", "stream".into(), stream).await;
        run.want("stream", &reply, FORBIDDEN);
    } else {
        let status = stream.status();
        run.check("stream", status == StatusCode::OK, || {
            format!("stream opened with {status}")
        });
        // Events from readable and unreadable channels, interleaved, then
        // a last one the subject can read.
        let mut live = Vec::new();
        for _ in 0..2 {
            for channel in CHANNELS {
                let posted = admin_write(
                    board,
                    &seeded.admin,
                    Method::POST,
                    "/v1/boards/general/messages",
                    json!({
                        "thread_id": seeded.threads[channel],
                        "text": format!("{} live", secret(channel)),
                    }),
                )
                .await;
                live.push((
                    channel,
                    posted["message"]["id"].as_str().unwrap().to_owned(),
                ));
            }
        }
        let last_channel = readable_existing.iter().next().unwrap().clone();
        let last = admin_write(
            board,
            &seeded.admin,
            Method::POST,
            "/v1/boards/general/messages",
            json!({ "channel": last_channel, "text": "the last event" }),
        )
        .await;
        let last_id = last["message"]["id"].as_str().unwrap().to_owned();
        let sent = read_stream(stream, &last_id).await;
        run.seen.push(Exchange {
            label: "stream".into(),
            sent: "/v1/boards/general/events/stream?after=start".into(),
            received: sent.clone(),
        });
        let streamed = frames(&sent);
        run.check(
            "stream",
            streamed.iter().any(|e| e["message_id"] == last_id.as_str()),
            || "the stream never delivered the last event".into(),
        );
        for (channel, id) in &live {
            let delivered = sent.contains(id.as_str());
            let label = format!("stream live in {channel}");
            run.check(&label, delivered == readable(channel), || {
                format!(
                    "delivered {delivered}, model readable {}",
                    readable(channel)
                )
            });
        }
        let (events, all) = run.all_events().await;
        if run.want("events", &events, Expect::Ok) {
            let got = channels_of(&all, &["channel"]);
            run.check("events", got == readable_existing, || {
                format!("events in {got:?}, model {readable_existing:?}")
            });
            let polled: Vec<&Value> = all.iter().map(|e| &e["cursor"]).collect();
            let streamed: Vec<&Value> = streamed.iter().map(|e| &e["cursor"]).collect();
            run.check("stream vs long-poll", polled == streamed, || {
                format!(
                    "the stream sent {} events, the long-poll {}",
                    streamed.len(),
                    polled.len()
                )
            });
        }
    }

    // ---- Zero unauthorized content anywhere. ----

    let unreadable: Vec<String> = seeded
        .existing
        .iter()
        .filter(|c| !readable(c))
        .cloned()
        .collect();
    let mut leaks = Vec::new();
    for exchange in &run.seen {
        for channel in &unreadable {
            if CHANNELS.contains(&channel.as_str()) {
                for needle in [secret(channel), task_secret(channel)] {
                    if exchange.received.contains(&needle) {
                        leaks.push((format!("leak in {}", exchange.label), needle));
                    }
                }
            }
            let echoed = names_channel(&exchange.sent, channel);
            if !echoed
                && !run.allowed_names.contains(channel)
                && names_channel(&exchange.received, channel)
            {
                leaks.push((format!("name leak in {}", exchange.label), channel.clone()));
            }
        }
    }
    for (label, what) in leaks {
        run.fail(&label, format!("leaked {what}"));
    }

    for id in made {
        operator::ungrant(&board.data_dir(), &id).unwrap();
    }

    // The admin still sees everything, whatever the subject was given.
    let all: Value = board
        .get(&seeded.admin, "/v1/boards/general/channels")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        all["channels"].as_array().unwrap().len(),
        seeded.existing.len()
    );
    Outcome {
        mismatches: run.mismatches,
        tally: run.tally,
        probes: run.probes,
    }
}

struct Outcome {
    mismatches: Vec<Mismatch>,
    tally: BTreeMap<String, (usize, usize)>,
    probes: usize,
}

#[tokio::test]
async fn the_authorization_matrix_leaks_nothing() {
    let started = std::time::Instant::now();
    let mut seeded = seeded().await;
    let mut mismatches = Vec::new();
    let mut tally: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut probes = 0;
    let scenarios = scenarios();
    for (index, scenario) in scenarios.iter().enumerate() {
        // A subject of its own per scenario, so the grants do not mix.
        let outcome = check(&mut seeded, scenario, index).await;
        mismatches.extend(outcome.mismatches);
        probes += outcome.probes;
        for (kind, (allowed, denied)) in outcome.tally {
            let counts = tally.entry(kind).or_default();
            counts.0 += allowed;
            counts.1 += denied;
        }
    }
    println!(
        "{} scenarios, {probes} checks in {:?}; per probe, (expected allowed, expected denied):",
        scenarios.len(),
        started.elapsed()
    );
    for (kind, counts) in &tally {
        println!("  {kind}: {counts:?}");
    }

    let known = |scenario: &str, probe: &str| {
        KNOWN_GAPS.iter().any(|gap| {
            gap.scenarios.contains(&scenario) && gap.probes.iter().any(|p| probe_matches(p, probe))
        })
    };
    let unexpected: Vec<String> = mismatches
        .iter()
        .filter(|(scenario, probe, _)| !known(scenario, probe))
        .map(|(s, p, d)| format!("[{s}] {p}: {d}"))
        .collect();
    assert!(
        unexpected.is_empty(),
        "{} disagreements with the model:\n{}",
        unexpected.len(),
        unexpected.join("\n")
    );
    // Every listed gap must still be seen, in every scenario it names.
    let mut fixed = Vec::new();
    for gap in KNOWN_GAPS {
        for scenario in gap.scenarios {
            for pattern in gap.probes {
                let seen = mismatches
                    .iter()
                    .any(|(s, p, _)| s == scenario && probe_matches(pattern, p));
                if !seen {
                    fixed.push(format!("[{scenario}] {pattern}"));
                }
            }
        }
    }
    assert!(
        fixed.is_empty(),
        "known gaps that no longer fail; remove them from KNOWN_GAPS:\n{}",
        fixed.join("\n")
    );
    println!(
        "{} disagreements, all of them known gaps:",
        mismatches.len()
    );
    for gap in KNOWN_GAPS {
        println!(
            "  {:?}\n    spec: {}\n    actual: {}",
            gap.probes, gap.spec, gap.actual
        );
    }
}

#[test]
fn the_model_agrees_with_the_spec_examples() {
    // docs/permissions.md: the most specific pattern decides, then the
    // closest grantee; ties join.
    let grants = [
        ("*", "agents", "read,post"),
        ("hr/", "everyone", "none"),
        ("hr/payroll", "subject", "read"),
    ];
    assert_eq!(grant_access(&grants, "docs"), verbs("read,post"));
    assert_eq!(grant_access(&grants, "hr/policy"), verbs("none"));
    assert_eq!(grant_access(&grants, "hr/payroll"), verbs("read"));
    assert!(!matches("build/", "buildings"));
    assert!(matches("build/", "build"));

    // The spec's worked example.
    let grants = [
        ("*", "agents", "read,post"),
        ("build/", "worker-1", "read,post,task.claim"),
        ("hr/", "everyone", "none"),
        ("hr/", "hr-bot", "read"),
    ];
    let of = |who, channel| grant_access_of(&grants, channel, who, "agents");
    assert_eq!(of("worker-1", "build/api"), verbs("read,post,task.claim"));
    assert_eq!(of("worker-2", "build/api"), verbs("read,post"));
    assert_eq!(of("worker-1", "hr/payroll"), verbs("none"));
    assert_eq!(of("hr-bot", "hr/payroll"), verbs("read"));
    // A group a principal is not in does not apply.
    assert_eq!(
        grant_access_of(&grants, "docs", "ada@example.com", "people"),
        verbs("none")
    );

    // Shorthands.
    assert_eq!(verbs("task"), verbs("task.create,task.claim"));
    assert!(verbs("manage").contains("delete") && !verbs("manage").contains("task.manage"));
    assert_eq!(verbs("all").len(), ALL_VERBS.len());

    // Authority containment.
    assert!(contains("*", "hr/"));
    assert!(contains("build/", "build/api"));
    assert!(contains("build/", "build"));
    assert!(contains("build/", "build/api/"));
    assert!(!contains("build/", "buildings"));
    assert!(!contains("build", "build/api"));
    assert!(!contains("build/", "*"));

    // Whole-path channel names.
    assert!(names_channel("\"channel\":\"build\"", "build"));
    assert!(!names_channel("\"channel\":\"buildings\"", "build"));
    assert!(!names_channel("\"channel\":\"build/api\"", "build"));
    assert!(!names_channel("\"channel\":\"build/api\"", "api"));
    assert!(names_channel("channel=build/api&x", "build/api"));
}
