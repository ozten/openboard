//! Grants on a network board (milestone M4): default deny, subtree access,
//! delegation, private channels, admin visibility and `explain`, end to
//! end over HTTPS. `docs/permissions.md` is the specification; the
//! evaluator's own tests are in `src/grants.rs`.

mod common;

use std::time::Duration;

use common::{error_of, Board};
use openboard_api::grants::{ChannelCreated, Explained, GrantChanged, GrantList};
use openboard_api::{codes, Thread};
use openboard_server::operator;
use reqwest::StatusCode;
use serde_json::{json, Value};

#[tokio::test]
async fn a_closed_board_admits_nobody_until_the_operator_grants() {
    let board = Board::closed().await;
    let agent = board.enrol("agent-a").await;
    error_of(
        board.post(&agent, "notes", "hello").await,
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    let read = board
        .get(&agent, "/v1/boards/general/threads?channel=notes")
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::FORBIDDEN);
    // A board the agent cannot create, either.
    let other = board
        .post_json(
            &agent,
            "/v1/boards/other/messages",
            json!({ "channel": "notes", "text": "hi" }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::FORBIDDEN);

    board.grant("*", "agent-a", "read,post,create");
    board.posted(&agent, "notes", "hello").await;
    let other = board
        .post_json(
            &agent,
            "/v1/boards/other/messages",
            json!({ "channel": "notes", "text": "hi" }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        other.status(),
        StatusCode::FORBIDDEN,
        "still closed to new boards"
    );
}

#[tokio::test]
async fn a_worker_narrowed_to_a_subtree_sees_and_writes_nothing_else() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let worker = board.enrol("worker").await;
    board.grant("*", "admin", "all");
    board.grant("build/", "worker", "read,post,react,label,task.claim");
    board.posted(&admin, "build/api", "build work").await;
    board.posted(&admin, "docs", "docs work").await;
    board.posted(&admin, "buildings", "not under build/").await;

    assert_eq!(board.channels_seen(&worker).await, ["build/api"]);
    assert_eq!(
        board.channels_seen(&admin).await,
        ["build/api", "buildings", "docs"]
    );
    board.posted(&worker, "build/api", "on it").await;
    error_of(
        board.post(&worker, "docs", "sneaky").await,
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    // It can claim work but not create it.
    let created = board
        .post_json(
            &worker,
            "/v1/boards/general/tasks",
            json!({ "channel": "build/api", "text": "a task" }),
        )
        .send()
        .await
        .unwrap();
    error_of(created, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
}

#[tokio::test]
async fn granting_needs_authority_that_holds_every_verb_given() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let lead = board.enrol("lead").await;
    let worker = board.enrol("worker").await;
    board.grant("*", "admin", "all");

    // Nobody but the admin may grant.
    error_of(
        board.add_grant(&lead, "build/", "worker", "read").await,
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
    // The admin delegates build/ to the lead.
    let added = board
        .add_grant(&admin, "build/", "lead", "read,post,task,grant.manage")
        .await;
    assert_eq!(added.status(), StatusCode::CREATED);
    let added: GrantChanged = added.json().await.unwrap();
    assert_eq!(added.grant.grantee, "lead");
    assert!(added.notices.is_empty());

    // Within build/ and its own verbs: yes.
    let ok = board
        .add_grant(&lead, "build/api", "worker", "read,post,task.claim")
        .await;
    assert_eq!(ok.status(), StatusCode::CREATED);
    // A verb it does not hold, or outside build/: no.
    error_of(
        board
            .add_grant(&lead, "build/", "worker", "read,delete")
            .await,
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
    error_of(
        board.add_grant(&lead, "docs/", "worker", "read").await,
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
    // A grant to everyone can only close.
    error_of(
        board.add_grant(&admin, "*", "everyone", "read").await,
        StatusCode::BAD_REQUEST,
        codes::INVALID_GRANT,
    )
    .await;
    error_of(
        board
            .add_grant(&admin, "*", "nobody-by-this-name", "read")
            .await,
        StatusCode::NOT_FOUND,
        codes::NOT_FOUND,
    )
    .await;

    // Each sees the grants it manages.
    let lists = |token: String| {
        let request = board.get(&token, "/v1/boards/general/grants");
        async move {
            let list: GrantList = request.send().await.unwrap().json().await.unwrap();
            list.grants
                .into_iter()
                .map(|grant| format!("{} {} {}", grant.pattern, grant.grantee, grant.verbs))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        lists(lead.clone()).await,
        [
            "build/ lead read,post,task.create,task.claim,grant.manage",
            "build/api worker read,post,task.claim"
        ]
    );
    assert_eq!(lists(admin.clone()).await.len(), 3);
    assert!(lists(worker.clone()).await.is_empty());

    // The lead removes its own grant to the worker; the worker cannot.
    let worker_grant = added_id(&board, &lead, "build/api").await;
    let removed = board
        .http
        .delete(board.at(&format!("/v1/boards/general/grants/{worker_grant}")))
        .bearer_auth(&worker)
        .send()
        .await
        .unwrap();
    error_of(removed, StatusCode::FORBIDDEN, codes::NO_AUTHORITY).await;
    let removed = board
        .http
        .delete(board.at(&format!("/v1/boards/general/grants/{worker_grant}")))
        .bearer_auth(&lead)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::OK);
}

async fn added_id(board: &Board, token: &str, pattern: &str) -> String {
    let list: GrantList = board
        .get(token, "/v1/boards/general/grants")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    list.grants
        .into_iter()
        .find(|grant| grant.pattern == pattern)
        .unwrap()
        .id
}

#[tokio::test]
async fn a_private_channel_admits_its_creator_and_whom_it_admits() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let ada = board.enrol("ada").await;
    let bob = board.enrol("bob").await;
    board.grant("*", "admin", "all");
    board.grant("*", "agents", "read,post,create");

    let made = board
        .post_json(
            &ada,
            "/v1/boards/general/channels",
            json!({ "path": "plans", "private": true }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(made.status(), StatusCode::CREATED);
    let made: ChannelCreated = made.json().await.unwrap();
    assert_eq!(made.grants.len(), 2);
    board.posted(&ada, "plans/q3", "secret plan").await;
    assert_eq!(board.channels_seen(&bob).await, Vec::<String>::new());
    error_of(
        board.post(&bob, "plans", "let me in").await,
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;

    // Not where it exists or a grant reaches.
    let again = board
        .post_json(
            &bob,
            "/v1/boards/general/channels",
            json!({ "path": "plans", "private": true }),
        )
        .send()
        .await
        .unwrap();
    error_of(again, StatusCode::CONFLICT, codes::CHANNEL_EXISTS).await;

    // The owner admits bob.
    assert_eq!(
        board
            .add_grant(&ada, "plans/", "bob", "read,post")
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(board.channels_seen(&bob).await, ["plans", "plans/q3"]);

    // The admin can let themselves in, and it shows in the channel (Z9).
    assert_eq!(board.channels_seen(&admin).await, Vec::<String>::new());
    let me = board.add_grant(&admin, "plans/", "admin", "read").await;
    assert_eq!(me.status(), StatusCode::CREATED);
    let me: GrantChanged = me.json().await.unwrap();
    assert_eq!(me.notices.len(), 2, "plans and plans/q3 both exist");
    let thread: Thread = board
        .get(
            &bob,
            &format!("/v1/boards/general/threads/{}", me.notices[0]),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        thread.root.text.contains("Admin access"),
        "{}",
        thread.root.text
    );
    assert_eq!(
        thread
            .root
            .labels
            .get(openboard_api::grants::NOTICE_LABEL)
            .map(String::as_str),
        Some(openboard_api::grants::NOTICE_ACCESS_GRANTED)
    );
    assert_eq!(thread.root.by.handle, "admin");
}

#[tokio::test]
async fn explain_names_the_deciding_grant_and_guards_others_access() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let worker = board.enrol("worker").await;
    board.grant("*", "admin", "all");
    let narrow = board.grant("build/", "worker", "read,post");

    let explained: Explained = board
        .get(&worker, "/v1/boards/general/explain?channel=build/api")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(explained.verbs, "read,post");
    assert_eq!(explained.principal, "worker");
    assert_eq!(explained.deciding[0].id, narrow);

    let nothing: Explained = board
        .get(&worker, "/v1/boards/general/explain?channel=docs")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(nothing.verbs, "none");
    assert!(nothing.deciding.is_empty());

    let refused = board
        .get(&worker, "/v1/boards/general/explain?channel=docs&for=admin")
        .send()
        .await
        .unwrap();
    error_of(refused, StatusCode::FORBIDDEN, codes::NO_AUTHORITY).await;
    let about: Explained = board
        .get(
            &admin,
            "/v1/boards/general/explain?channel=build/x&for=worker",
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(about.principal, "worker");
    assert_eq!(about.verbs, "read,post");
}

#[tokio::test]
async fn task_manage_lets_an_admin_change_tasks_it_did_not_create() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let lead = board.enrol("lead").await;
    let other = board.enrol("other").await;
    board.grant("*", "admin", "all");
    board.grant("*", "agents", "read,post,create,task");
    let created: Value = board
        .post_json(
            &lead,
            "/v1/boards/general/tasks",
            json!({ "channel": "build", "text": "do it" }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["task"]["id"].as_str().unwrap().to_owned();
    let cancel = |token: &str| {
        board.post_json(
            token,
            &format!("/v1/boards/general/tasks/{id}/cancel"),
            json!({ "reason": "not needed" }),
        )
    };
    error_of(
        cancel(&other).send().await.unwrap(),
        StatusCode::FORBIDDEN,
        codes::NOT_TASK_CREATOR,
    )
    .await;
    assert_eq!(
        cancel(&admin).send().await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn an_open_board_admits_everyone_and_its_creators_own_their_boards() {
    let board = Board::start(true).await;
    let ada = board.enrol("ada").await;
    let bob = board.enrol("bob").await;
    board.posted(&ada, "notes", "general is open").await;
    let created = board
        .post_json(
            &ada,
            "/v1/boards/ada-board/messages",
            json!({ "channel": "notes", "text": "mine" }),
        )
        .send()
        .await
        .unwrap();
    assert!(created.status().is_success());
    // Bob can read it, and Ada holds grant.manage there.
    let list: GrantList = board
        .get(&ada, "/v1/boards/ada-board/grants")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list
        .grants
        .iter()
        .any(|grant| grant.grantee == "ada" && grant.verbs == "all"));
    let read = board
        .get(&bob, "/v1/boards/ada-board/threads?channel=notes")
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::OK);
    error_of(
        board
            .post_json(
                &bob,
                "/v1/boards/ada-board/grants",
                json!({ "pattern": "*", "grantee": "bob", "verbs": "all" }),
            )
            .send()
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
}

// ---------------------------------------------------------------------------
// Derived tokens (A9, A10)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_derived_token_only_narrows_and_dies_with_its_parent() {
    let board = Board::closed().await;
    let lead = board.enrol("lead").await;
    board.grant("*", "lead", "all");
    board.posted(&lead, "docs", "not for the worker").await;
    board.posted(&lead, "build/api", "work here").await;

    let worker = board
        .minted(&lead, &["build/=read,post,react,label,task.claim"])
        .await;
    assert_eq!(worker.info.can, ["build/=read,post,react,label,task.claim"]);
    assert!(worker.info.credential.parent.is_some());
    let token = worker.token.clone();

    board.posted(&token, "build/api", "within scope").await;
    error_of(
        board.post(&token, "docs", "outside").await,
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    assert_eq!(board.channels_seen(&token).await, ["build/api"]);
    // It holds what the lead holds there, no more: no task.create.
    let created = board
        .post_json(
            &token,
            "/v1/boards/general/tasks",
            json!({ "channel": "build/api", "text": "a task" }),
        )
        .send()
        .await
        .unwrap();
    error_of(created, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    // A derived token cannot change grants, even with grant.manage in scope.
    let wide = board.minted(&lead, &["*=all"]).await;
    error_of(
        board.add_grant(&wide.token, "build/", "lead", "read").await,
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
    // Nor create a board or a private channel.
    let other = board
        .post_json(
            &wide.token,
            "/v1/boards/general/channels",
            json!({ "path": "secret", "private": true }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::FORBIDDEN);
    // A token for general is nothing on another board.
    operator::grant(&board.data_dir(), "other", "*", "lead", "all", None).unwrap();
    let elsewhere = board
        .get(&wide.token, "/v1/boards/other/threads?channel=notes")
        .send()
        .await
        .unwrap();
    assert_eq!(elsewhere.status(), StatusCode::FORBIDDEN);

    // Minting from a derived token: the child is still within the parent.
    let grandchild = board.minted(&token, &["*=read,post"]).await;
    error_of(
        board.post(&grandchild.token, "docs", "still outside").await,
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    board
        .posted(&grandchild.token, "build/api", "inside both")
        .await;

    // The lead lists and revokes; everything below goes with it.
    let listed: openboard_api::tokens::TokenList = board
        .get(&lead, "/v1/tokens")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed.tokens.len(), 3);
    let mine: openboard_api::tokens::TokenList = board
        .get(&token, "/v1/tokens")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        mine.tokens.len(),
        1,
        "a derived token sees only its own children"
    );
    let revoked = board
        .http
        .delete(board.at(&format!("/v1/tokens/{}", worker.info.credential.id)))
        .bearer_auth(&lead)
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::OK);
    for dead in [&token, &grandchild.token] {
        let response = board.get(dead, "/v1/whoami").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    board.posted(&lead, "docs", "the parent lives on").await;
    board
        .posted(&wide.token, "docs", "and its other child")
        .await;
}

#[tokio::test]
async fn a_derived_token_cannot_outlive_its_parent_and_binds_its_profile() {
    let board = Board::closed().await;
    let lead = board.enrol("lead").await;
    board.grant("*", "lead", "all");
    let short = board
        .mint(
            &lead,
            json!({ "board": "general", "can": ["*=all"], "ttl_secs": 60 }),
        )
        .await;
    let short: openboard_api::tokens::TokenMinted = short.json().await.unwrap();
    let child = board
        .mint(
            &short.token,
            json!({ "board": "general", "can": ["*=all"], "ttl_secs": 86_400, "profile": "fx" }),
        )
        .await;
    assert_eq!(child.status(), StatusCode::CREATED);
    let child: openboard_api::tokens::TokenMinted = child.json().await.unwrap();
    assert!(child.info.credential.expires_at <= short.info.credential.expires_at);
    assert_eq!(child.info.credential.profile.as_deref(), Some("fx"));

    let posted = board.posted(&child.token, "notes", "as fx").await;
    assert_eq!(posted.message.by.profile.as_deref(), Some("fx"));
    let other = board
        .post_json(
            &child.token,
            "/v1/boards/general/messages",
            json!({ "channel": "notes", "text": "as ops" }),
        )
        .header(openboard_api::PROFILE_HEADER, "ops")
        .send()
        .await
        .unwrap();
    error_of(other, StatusCode::BAD_REQUEST, codes::INVALID_PROFILE).await;

    for bad in [
        json!({ "board": "general", "can": [] }),
        json!({ "board": "general", "can": ["build/"] }),
        json!({ "board": "general", "can": ["build/=fly"] }),
        json!({ "board": "general", "can": ["*=read"], "ttl_secs": 0 }),
    ] {
        let response = board.mint(&lead, bad.clone()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

// ---------------------------------------------------------------------------
// Profile limits (A8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_limited_profile_narrows_only_on_its_board_and_only_when_declared() {
    let board = Board::closed().await;
    let ada = board.enrol("ada").await;
    board.grant("*", "ada", "all");
    operator::grant(&board.data_dir(), "other", "*", "ada", "all", None).unwrap();
    board.posted(&ada, "build/api", "work").await;
    board.posted(&ada, "docs", "words").await;

    let set = board
        .http
        .put(board.at("/v1/boards/general/profiles/fx"))
        .bearer_auth(&ada)
        .json(&json!({ "can": ["build/=read,post"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(set.status(), StatusCode::OK);
    let as_fx = |channel: &str| {
        board
            .post_json(
                &ada,
                "/v1/boards/general/messages",
                json!({ "channel": channel, "text": "as fx" }),
            )
            .header(openboard_api::PROFILE_HEADER, "fx")
    };
    assert!(as_fx("build/api")
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    error_of(
        as_fx("docs").send().await.unwrap(),
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    // Undeclared, or declared on another board: Ada's own access.
    board.posted(&ada, "docs", "as ada").await;
    let elsewhere = board
        .post_json(
            &ada,
            "/v1/boards/other/messages",
            json!({ "channel": "docs", "text": "fx elsewhere" }),
        )
        .header(openboard_api::PROFILE_HEADER, "fx")
        .send()
        .await
        .unwrap();
    assert!(elsewhere.status().is_success());
    let explained: Explained = board
        .get(&ada, "/v1/boards/general/explain?channel=build/api")
        .header(openboard_api::PROFILE_HEADER, "fx")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(explained.verbs, "read,post");
    assert!(explained.narrowed);

    // A token bound to fx carries the limit; it cannot lift it.
    let bound = board
        .mint(
            &ada,
            json!({ "board": "general", "can": ["*=all"], "profile": "fx" }),
        )
        .await;
    let bound: openboard_api::tokens::TokenMinted = bound.json().await.unwrap();
    error_of(
        board.post(&bound.token, "docs", "via token").await,
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    let lift = board
        .http
        .delete(board.at("/v1/boards/general/profiles/fx"))
        .bearer_auth(&bound.token)
        .send()
        .await
        .unwrap();
    error_of(lift, StatusCode::FORBIDDEN, codes::DERIVED_TOKEN).await;

    let lift = board
        .http
        .delete(board.at("/v1/boards/general/profiles/fx"))
        .bearer_auth(&ada)
        .send()
        .await
        .unwrap();
    assert_eq!(lift.status(), StatusCode::NO_CONTENT);
    board.posted(&bound.token, "docs", "lifted").await;
}

// ---------------------------------------------------------------------------
// Delivery-time checks (Z14) and claims across a subtree
// ---------------------------------------------------------------------------

#[tokio::test]
async fn removing_a_grant_ends_open_waits_and_streams() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let worker = board.enrol("worker").await;
    board.grant("*", "admin", "all");
    let grant = board.grant("build/", "worker", "read,post");
    board.posted(&admin, "build/api", "work").await;

    let poll = board
        .get(
            &worker,
            "/v1/boards/general/events?channel=build/api&wait=30",
        )
        .send();
    let stream = board
        .get(&worker, "/v1/boards/general/events/stream?channel=build/")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    let data_dir = board.data_dir();
    let revoke = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        tokio::task::spawn_blocking(move || operator::ungrant(&data_dir, &grant))
            .await
            .unwrap()
            .unwrap();
    };
    let (response, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(poll, revoke) })
            .await
            .expect("the long-poll ends promptly");
    error_of(response.unwrap(), StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    let sent = tokio::time::timeout(Duration::from_secs(5), stream.text())
        .await
        .expect("the stream ends promptly")
        .unwrap();
    assert!(!sent.contains("data:"), "{sent}");
}

#[tokio::test]
async fn a_narrowed_grant_filters_what_an_open_wait_delivers() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let worker = board.enrol("worker").await;
    board.grant("*", "admin", "all");
    board.grant("*", "worker", "read");
    board.posted(&admin, "docs", "open for now").await;
    let start: Value = board
        .get(&worker, "/v1/boards/general/events")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cursor = start["cursor"].as_str().unwrap().to_owned();
    let poll = board
        .get(
            &worker,
            &format!("/v1/boards/general/events?after={cursor}&wait=10"),
        )
        .send();
    let narrow = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let response = board.add_grant(&admin, "docs", "worker", "none").await;
        assert_eq!(response.status(), StatusCode::CREATED);
        // Past the next recheck.
        tokio::time::sleep(Duration::from_millis(300)).await;
        board.posted(&admin, "docs", "closed now").await;
        board.posted(&admin, "build", "still open").await;
    };
    let (response, ()) = tokio::join!(poll, narrow);
    let events: Value = response.unwrap().json().await.unwrap();
    let channels: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["channel"].as_str().unwrap())
        .collect();
    assert!(!channels.contains(&"docs"), "{events}");
}

#[tokio::test]
async fn a_claim_across_a_subtree_takes_only_what_the_caller_may_claim() {
    let board = Board::closed().await;
    let lead = board.enrol("lead").await;
    let worker = board.enrol("worker").await;
    board.grant("*", "lead", "all");
    board.grant("build/", "worker", "read,post");
    board.grant("build/api", "worker", "read,post,task.claim");
    for channel in ["build/web", "build/api"] {
        let created = board
            .post_json(
                &lead,
                "/v1/boards/general/tasks",
                json!({ "channel": channel, "text": format!("work in {channel}") }),
            )
            .send()
            .await
            .unwrap();
        assert!(created.status().is_success(), "{}", created.status());
    }
    let claimed: Value = board
        .post_json(
            &worker,
            "/v1/boards/general/tasks/claim",
            json!({ "channel": "build/", "from": ["lead"] }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        claimed["claim"]["task"]["channel"], "build/api",
        "{claimed}"
    );
    let again: Value = board
        .post_json(
            &worker,
            "/v1/boards/general/tasks/claim",
            json!({ "channel": "build/", "from": ["lead"] }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(again["claim"].is_null(), "{again}");
    // Where it holds no task.claim at all, the claim is refused outright.
    let refused = board
        .post_json(
            &worker,
            "/v1/boards/general/tasks/claim",
            json!({ "channel": "build/web" }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Intake channels (S4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn requests_arrive_in_intake_and_only_task_creators_promote_them() {
    let board = Board::closed().await;
    let lead = board.enrol("lead").await;
    let worker = board.enrol("worker").await;
    let customer = board.enrol("customer").await;
    board.grant("*", "lead", "all");
    board.grant("inbox", "customer", "read,post");
    board.grant("inbox", "worker", "read");
    board.grant("build/", "worker", "read,post,react,label,task.claim");

    let made = board
        .post_json(
            &lead,
            "/v1/boards/general/channels",
            json!({ "path": "inbox", "intake": true }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(made.status(), StatusCode::CREATED);
    let request = board
        .posted(&customer, "inbox", "please add CSV export")
        .await;
    let request_id = request.message.id.clone();

    // No tasks in intake: not created there, not made from its threads.
    let create = |token: &str, body: Value| {
        board
            .post_json(token, "/v1/boards/general/tasks", body)
            .send()
    };
    error_of(
        create(&lead, json!({ "channel": "inbox", "text": "a task" }))
            .await
            .unwrap(),
        StatusCode::CONFLICT,
        codes::INTAKE_CHANNEL,
    )
    .await;
    error_of(
        create(&lead, json!({ "thread_id": request_id }))
            .await
            .unwrap(),
        StatusCode::CONFLICT,
        codes::INTAKE_CHANNEL,
    )
    .await;

    // The worker cannot promote it: it lacks task.create in build/.
    error_of(
        create(
            &worker,
            json!({ "channel": "build/api", "text": "CSV export", "origin": request_id }),
        )
        .await
        .unwrap(),
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    board.posted(&lead, "build/api", "the work channel").await;
    let promoted: Value = create(
        &lead,
        json!({ "channel": "build/api", "text": "CSV export", "origin": request_id }),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(
        promoted["task"]["labels"]["board.origin"], request_id,
        "{promoted}"
    );

    // The worker claims, replies and completes it with post and task.claim.
    let claimed: Value = board
        .post_json(
            &worker,
            "/v1/boards/general/tasks/claim",
            json!({ "channel": "build/", "from": ["lead"] }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let claim = &claimed["claim"];
    let id = claim["task"]["id"].as_str().unwrap().to_owned();
    let reply = board
        .post_json(
            &worker,
            "/v1/boards/general/messages",
            json!({ "thread_id": id, "text": "halfway" }),
        )
        .send()
        .await
        .unwrap();
    assert!(reply.status().is_success(), "{}", reply.status());
    let done = board
        .post_json(
            &worker,
            &format!("/v1/boards/general/tasks/{id}/complete"),
            json!({ "epoch": claim["epoch"], "token": claim["token"], "result": "shipped" }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        done.status(),
        StatusCode::OK,
        "{}",
        done.text().await.unwrap()
    );

    // An origin in a work channel is refused; changing a mode takes
    // grant.manage there.
    let wrong = create(
        &lead,
        json!({ "channel": "build/api", "text": "again", "origin": id }),
    )
    .await
    .unwrap();
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    let mode = |token: &str| {
        board
            .post_json(
                token,
                "/v1/boards/general/channels/mode",
                json!({ "path": "inbox", "intake": false }),
            )
            .send()
    };
    error_of(
        mode(&worker).await.unwrap(),
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
    assert_eq!(mode(&lead).await.unwrap().status(), StatusCode::OK);
    let channels: Value = board
        .get(&lead, "/v1/boards/general/channels")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        channels["channels"]
            .as_array()
            .unwrap()
            .iter()
            .all(|channel| channel.get("intake").is_none()),
        "{channels}"
    );
}

#[tokio::test]
async fn deleting_someone_elses_message_takes_delete() {
    let board = Board::closed().await;
    let ada = board.enrol("ada").await;
    let bob = board.enrol("bob").await;
    let moderator = board.enrol("moderator").await;
    board.grant("*", "agents", "read,post,create");
    board.grant("*", "moderator", "read,post,delete");
    let posted = board.posted(&ada, "notes", "spam").await;
    let delete = |token: &str| {
        board
            .http
            .delete(board.at(&format!(
                "/v1/boards/general/messages/{}",
                posted.message.id
            )))
            .bearer_auth(token)
            .send()
    };
    error_of(
        delete(&bob).await.unwrap(),
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    let deleted: Value = delete(&moderator).await.unwrap().json().await.unwrap();
    assert!(deleted["deleted_at"].is_string(), "{deleted}");
}
