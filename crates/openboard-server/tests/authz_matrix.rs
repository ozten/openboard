//! Gate T1, the authorization matrix (milestone M4).
//!
//! Each scenario gives a subject grants (named, through its group, and to
//! everyone), and sometimes a derived token's scope or an expired grant, on
//! a board with nested and prefix-colliding channels. An independent model
//! of `docs/permissions.md` says which channels the subject can read and
//! post in. Every read surface is checked against it: board and channel
//! listings, counts, thread and task listings, threads and tasks by ID,
//! events, `explain` and errors. Every response body is kept, and none may
//! contain text from a channel the subject cannot read.
//!
//! Search and attachments do not exist yet; they join this matrix when
//! they do.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use common::Board;
use openboard_server::operator;
use reqwest::{RequestBuilder, StatusCode};
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

fn secret(channel: &str) -> String {
    format!("secret.{}.end", channel.replace('/', "-"))
}

fn task_secret(channel: &str) -> String {
    format!("tasksecret.{}.end", channel.replace('/', "-"))
}

// ---------------------------------------------------------------------------
// The model: the rules of docs/permissions.md, written again from scratch
// ---------------------------------------------------------------------------

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

/// Higher is more specific.
fn specificity(pattern: &str) -> (u8, usize) {
    match kind(pattern) {
        Kind::Exact => (2, pattern.len()),
        Kind::Subtree => (1, pattern.len()),
        Kind::Board => (0, 0),
    }
}

fn verbs(raw: &str) -> BTreeSet<String> {
    match raw {
        "none" => BTreeSet::new(),
        raw => raw.split(',').map(str::to_owned).collect(),
    }
}

/// The verbs `subject` (an agent) holds in `channel`: the most specific
/// matching patterns, then the closest grantees, joined.
fn grant_access(grants: &[(&str, &str, &str)], channel: &str) -> BTreeSet<String> {
    let closeness = |who: &str| match who {
        "subject" => Some(2),
        "agents" => Some(1),
        "everyone" => Some(0),
        _ => None,
    };
    let applying: Vec<_> = grants
        .iter()
        .filter(|(pattern, who, _)| matches(pattern, channel) && closeness(who).is_some())
        .collect();
    let Some(best) = applying.iter().map(|(p, _, _)| specificity(p)).max() else {
        return BTreeSet::new();
    };
    let at_best: Vec<_> = applying
        .into_iter()
        .filter(|(p, _, _)| specificity(p) == best)
        .collect();
    let closest = at_best.iter().filter_map(|(_, w, _)| closeness(w)).max();
    at_best
        .into_iter()
        .filter(|(_, w, _)| closeness(w) == closest)
        .flat_map(|(_, _, v)| verbs(v))
        .collect()
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

struct Scenario {
    name: &'static str,
    grants: Vec<(&'static str, &'static str, &'static str)>,
    /// Made with a one-millisecond lifetime, so expired when checked.
    expired: Vec<(&'static str, &'static str, &'static str)>,
    scope: Option<Vec<&'static str>>,
}

impl Scenario {
    fn access(&self, channel: &str) -> BTreeSet<String> {
        let granted = grant_access(&self.grants, channel);
        match &self.scope {
            None => granted,
            Some(scope) => granted
                .intersection(&scope_access(scope, channel))
                .cloned()
                .collect(),
        }
    }

    fn readable(&self) -> BTreeSet<&'static str> {
        CHANNELS
            .into_iter()
            .filter(|channel| self.access(channel).contains("read"))
            .collect()
    }
}

fn scenarios() -> Vec<Scenario> {
    let s = |name, grants: Vec<(&'static str, &'static str, &'static str)>| Scenario {
        name,
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
            grants: vec![("*", "agents", "read")],
            expired: vec![("*", "subject", "none"), ("docs", "subject", "read,post")],
            scope: None,
        },
        Scenario {
            name: "a token scope narrows to a subtree",
            grants: vec![("*", "subject", "read,post")],
            expired: Vec::new(),
            scope: Some(vec!["build/=read"]),
        },
        Scenario {
            name: "a token scope never widens the grants",
            grants: vec![("build/", "subject", "read")],
            expired: Vec::new(),
            scope: Some(vec!["*=read,post", "docs=read,post"]),
        },
        Scenario {
            name: "a token scope's most specific entry decides",
            grants: vec![("*", "subject", "read,post")],
            expired: Vec::new(),
            scope: Some(vec!["*=read,post", "build/=read", "build/api/v2=none"]),
        },
    ]
}

// ---------------------------------------------------------------------------
// The board and the probes
// ---------------------------------------------------------------------------

struct Seeded {
    board: Board,
    admin: String,
    /// Channel → the ID of the thread posted there.
    threads: BTreeMap<&'static str, String>,
    /// Channel → the ID of the task created there.
    tasks: BTreeMap<&'static str, String>,
}

async fn seeded() -> Seeded {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    board.grant("*", "admin", "all");
    let mut threads = BTreeMap::new();
    let mut tasks = BTreeMap::new();
    for channel in CHANNELS {
        let posted = board.posted(&admin, channel, &secret(channel)).await;
        threads.insert(channel, posted.message.id);
        let created: Value = board
            .post_json(
                &admin,
                "/v1/boards/general/tasks",
                json!({ "channel": channel, "text": task_secret(channel) }),
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        tasks.insert(channel, created["task"]["id"].as_str().unwrap().to_owned());
    }
    Seeded {
        board,
        admin,
        threads,
        tasks,
    }
}

/// Every response body the subject received, to search for leaks.
#[derive(Default)]
struct Seen {
    bodies: Vec<(String, String)>,
}

impl Seen {
    async fn send(&mut self, what: String, request: RequestBuilder) -> (StatusCode, Value) {
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        self.bodies.push((what, text));
        (status, value)
    }
}

fn channels_in(items: &Value, at: &[&str]) -> BTreeSet<String> {
    items
        .as_array()
        .map(|items| {
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
        })
        .unwrap_or_default()
}

async fn check(seeded: &Seeded, scenario: &Scenario, subject_name: &str) {
    let board = &seeded.board;
    let name = scenario.name;
    let subject = board.enrol(subject_name).await;
    let rename = |who: &str| if who == "subject" { subject_name } else { who }.to_owned();
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
    let token = match &scenario.scope {
        None => subject,
        Some(scope) => {
            let minted: Value = board
                .mint(&subject, json!({ "board": "general", "can": scope }))
                .await
                .json()
                .await
                .unwrap();
            minted["token"].as_str().unwrap().to_owned()
        }
    };
    let readable = scenario.readable();
    let readable_owned: BTreeSet<String> = readable.iter().map(|c| (*c).to_owned()).collect();
    let mut seen = Seen::default();

    // Boards, and the board's newest activity.
    let (status, boards) = seen
        .send("boards".into(), board.get(&token, "/v1/boards"))
        .await;
    assert_eq!(status, StatusCode::OK, "{name}");
    let listed = boards["boards"].as_array().unwrap();
    assert_eq!(listed.is_empty(), readable.is_empty(), "{name}: {boards}");

    // Channels and their counts.
    let (status, channels) = seen
        .send(
            "channels".into(),
            board.get(&token, "/v1/boards/general/channels"),
        )
        .await;
    if readable.is_empty() {
        assert_eq!(status, StatusCode::FORBIDDEN, "{name}: channels");
    } else {
        assert_eq!(status, StatusCode::OK, "{name}");
        assert_eq!(
            channels_in(&channels["channels"], &["path"]),
            readable_owned,
            "{name}: channel listing"
        );
    }

    // Thread listings, by channel and by subtree.
    for channel in CHANNELS {
        let (status, threads) = seen
            .send(
                format!("threads {channel}"),
                board.get(
                    &token,
                    &format!("/v1/boards/general/threads?channel={channel}"),
                ),
            )
            .await;
        if readable.contains(channel) {
            assert_eq!(status, StatusCode::OK, "{name}: threads in {channel}");
            assert_eq!(
                channels_in(&threads["threads"], &["root", "channel"]),
                BTreeSet::from([channel.to_owned()]),
                "{name}"
            );
        } else {
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{name}: threads in {channel}"
            );
        }
    }
    let (status, subtree) = seen
        .send(
            "threads build/".into(),
            board.get(&token, "/v1/boards/general/threads?channel=build/"),
        )
        .await;
    if status == StatusCode::OK {
        let got = channels_in(&subtree["threads"], &["root", "channel"]);
        let want: BTreeSet<String> = readable_owned
            .iter()
            .filter(|c| matches("build/", c))
            .cloned()
            .collect();
        assert_eq!(got, want, "{name}: subtree listing");
    } else {
        assert!(readable.is_empty(), "{name}: subtree refused: {status}");
    }

    // Threads and tasks by ID: unreadable answers exactly as missing does.
    let (missing_thread, _) = seen
        .send(
            "missing thread".into(),
            board.get(&token, "/v1/boards/general/threads/msg_00000000deadbeef"),
        )
        .await;
    let (missing_task, _) = seen
        .send(
            "missing task".into(),
            board.get(&token, "/v1/boards/general/tasks/msg_00000000deadbeef"),
        )
        .await;
    for channel in CHANNELS {
        for (what, id, missing) in [
            ("threads", &seeded.threads[channel], missing_thread),
            ("tasks", &seeded.tasks[channel], missing_task),
        ] {
            let (status, _) = seen
                .send(
                    format!("{what}/{id} in {channel}"),
                    board.get(&token, &format!("/v1/boards/general/{what}/{id}")),
                )
                .await;
            if readable.contains(channel) {
                assert_eq!(status, StatusCode::OK, "{name}: {what} in {channel}");
            } else {
                assert_eq!(status, missing, "{name}: {what} in {channel} vs missing");
            }
        }
    }

    // Tasks listed across the board.
    let (status, tasks) = seen
        .send(
            "tasks".into(),
            board.get(&token, "/v1/boards/general/tasks?state=all"),
        )
        .await;
    if status == StatusCode::OK {
        assert_eq!(
            channels_in(&tasks["tasks"], &["root", "channel"]),
            readable_owned,
            "{name}: task listing"
        );
    } else {
        assert!(
            readable.is_empty(),
            "{name}: task listing refused: {status}"
        );
    }

    // Events from the start.
    let (status, events) = seen
        .send(
            "events".into(),
            board.get(&token, "/v1/boards/general/events?after=start&limit=1000"),
        )
        .await;
    if status == StatusCode::OK {
        assert_eq!(
            channels_in(&events["events"], &["channel"]),
            readable_owned,
            "{name}: events"
        );
    } else {
        assert!(readable.is_empty(), "{name}: events refused: {status}");
    }

    // `explain` agrees with the model wherever it is asked.
    for channel in CHANNELS {
        let (status, explained) = seen
            .send(
                format!("explain {channel}"),
                board.get(
                    &token,
                    &format!("/v1/boards/general/explain?channel={channel}"),
                ),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{name}: explain {channel}");
        let got = verbs(explained["verbs"].as_str().unwrap());
        assert_eq!(got, scenario.access(channel), "{name}: explain {channel}");
    }

    // Writing: a reply needs read and post. Done last, as it adds events.
    for channel in CHANNELS {
        let (status, _) = seen
            .send(
                format!("reply in {channel}"),
                board.post_json(
                    &token,
                    "/v1/boards/general/messages",
                    json!({ "thread_id": seeded.threads[channel], "text": "a reply" }),
                ),
            )
            .await;
        let access = scenario.access(channel);
        let may = access.contains("read") && access.contains("post");
        assert_eq!(
            status.is_success(),
            may,
            "{name}: reply in {channel}: {status}"
        );
    }

    // Zero unauthorized content anywhere.
    for channel in CHANNELS.into_iter().filter(|c| !readable.contains(c)) {
        for (what, body) in &seen.bodies {
            for needle in [secret(channel), task_secret(channel)] {
                assert!(
                    !body.contains(&needle),
                    "{name}: {what} leaked {needle}: {body}"
                );
            }
        }
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
    assert_eq!(all["channels"].as_array().unwrap().len(), CHANNELS.len());
}

#[tokio::test]
async fn the_authorization_matrix_leaks_nothing() {
    let seeded = seeded().await;
    for (index, scenario) in scenarios().iter().enumerate() {
        // A subject of its own per scenario, so the grants do not mix.
        check(&seeded, scenario, &format!("subject-{index}")).await;
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
}
