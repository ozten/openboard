//! Who may change grants, and what changing them leaves behind (Z9), end to
//! end over HTTPS: the notice a removal posts, notices that cannot be
//! edited or deleted, and removals that cannot hand out verbs.
//! `docs/permissions.md`, "Administration: who may change grants", is the
//! specification.

mod common;

use common::{error_of, Board};
use openboard_api::grants::{GrantChanged, NOTICE_ACCESS_GRANTED, NOTICE_LABEL};
use openboard_api::{codes, Thread};
use reqwest::StatusCode;
use serde_json::json;

async fn remove(board: &Board, token: &str, id: &str) -> reqwest::Response {
    board
        .http
        .delete(board.at(&format!("/v1/boards/general/grants/{id}")))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
}

async fn thread(board: &Board, token: &str, id: &str) -> Thread {
    let response = board
        .get(token, &format!("/v1/boards/general/threads/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

/// A board with an administrator holding `all`, and the private channel
/// `hr/plans` that its owner can use and the administrator cannot read.
async fn private_hr_plans() -> (Board, String, String, String) {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let owner = board.enrol("owner").await;
    board.grant("*", "admin", "all");
    let closed = board.grant("hr/plans/", "everyone", "none");
    board.grant("hr/plans/", "owner", "read,post,create,grant.manage");
    board.posted(&owner, "hr/plans", "the plans").await;
    assert_eq!(board.channels_seen(&admin).await, Vec::<String>::new());
    (board, admin, owner, closed)
}

#[tokio::test]
async fn removing_a_grant_that_opens_a_channel_posts_a_notice() {
    let (board, admin, owner, closed) = private_hr_plans().await;

    let removed = remove(&board, &admin, &closed).await;
    assert_eq!(removed.status(), StatusCode::OK);
    let removed: GrantChanged = removed.json().await.unwrap();
    assert_eq!(removed.notices.len(), 1, "hr/plans is now open to admin");
    assert_eq!(board.channels_seen(&admin).await, ["hr/plans"]);

    // The channel's owner reads the notice, written as the administrator.
    let notice = thread(&board, &owner, &removed.notices[0]).await;
    assert_eq!(notice.root.channel, "hr/plans");
    assert_eq!(notice.root.by.handle, "admin");
    assert!(
        notice.root.text.contains("Admin access"),
        "{}",
        notice.root.text
    );
    assert!(notice.root.text.contains("removed"), "{}", notice.root.text);
    assert!(notice.root.text.contains(&closed), "{}", notice.root.text);
    assert!(
        notice.root.text.contains("hr/plans/"),
        "{}",
        notice.root.text
    );
    assert_eq!(
        notice.root.labels.get(NOTICE_LABEL).map(String::as_str),
        Some(NOTICE_ACCESS_GRANTED)
    );
}

#[tokio::test]
async fn removing_a_grant_that_opens_nothing_for_the_remover_posts_no_notice() {
    let (board, admin, owner, closed) = private_hr_plans().await;
    board.posted(&admin, "docs", "docs").await;

    // Not a grant that touches the channel.
    let docs = board.grant("docs/", "admin", "read");
    let removed = remove(&board, &admin, &docs).await;
    assert_eq!(removed.status(), StatusCode::OK);
    let removed: GrantChanged = removed.json().await.unwrap();
    assert!(removed.notices.is_empty());

    // The administrator could already read hr/plans: no notice either.
    assert_eq!(
        board
            .add_grant(&admin, "hr/plans/", "admin", "read")
            .await
            .status(),
        StatusCode::CREATED
    );
    let removed = remove(&board, &admin, &closed).await;
    assert_eq!(removed.status(), StatusCode::OK);
    let removed: GrantChanged = removed.json().await.unwrap();
    assert!(removed.notices.is_empty());

    // The owner sees only the one notice, from the grant above.
    let threads: serde_json::Value = board
        .get(&owner, "/v1/boards/general/threads?channel=hr/plans")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(threads["threads"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_board_notice_cannot_be_edited_or_deleted() {
    let (board, admin, _owner, _closed) = private_hr_plans().await;
    let moderator = board.enrol("moderator").await;
    board.grant("hr/plans/", "moderator", "read,post,delete");

    let given = board
        .add_grant(&admin, "hr/plans/", "admin", "read,post,delete")
        .await;
    assert_eq!(given.status(), StatusCode::CREATED);
    let given: GrantChanged = given.json().await.unwrap();
    let id = &given.notices[0];
    let url = format!("/v1/boards/general/messages/{id}");

    // Its author, who holds post and delete there.
    let edit = board
        .http
        .patch(board.at(&url))
        .bearer_auth(&admin)
        .json(&json!({ "text": "nothing to see" }))
        .send()
        .await
        .unwrap();
    let body = error_of(edit, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    assert!(body.error.message.contains("board notice"), "{body:?}");
    let delete = board
        .http
        .delete(board.at(&url))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let body = error_of(delete, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    assert!(body.error.message.contains("board notice"), "{body:?}");

    // Someone else holding `delete`.
    let delete = board
        .http
        .delete(board.at(&url))
        .bearer_auth(&moderator)
        .send()
        .await
        .unwrap();
    let body = error_of(delete, StatusCode::FORBIDDEN, codes::FORBIDDEN).await;
    assert!(body.error.message.contains("board notice"), "{body:?}");

    // Nothing changed.
    let notice = thread(&board, &moderator, id).await;
    assert!(notice.root.text.contains("Admin access"));
    assert!(notice.root.labels.contains_key(NOTICE_LABEL));
    assert_eq!(notice.root.revision, 1);

    // The label cannot be cleared to get around it.
    let cleared = board
        .post_json(
            &admin,
            &format!("{url}/labels"),
            json!({ "clear": [NOTICE_LABEL] }),
        )
        .send()
        .await
        .unwrap();
    assert!(!cleared.status().is_success(), "{}", cleared.status());
}

#[tokio::test]
async fn removing_cannot_hand_out_verbs_the_remover_lacks() {
    let board = Board::closed().await;
    let lead = board.enrol("lead").await;
    let admin = board.enrol("admin").await;
    board.grant("*", "admin", "all");
    board.grant("*", "agents", "read,post,delete");
    let shadow = board.grant("build/", "agents", "read");
    let own = board.grant("build/", "lead", "read,grant.manage");

    // Lead may not grant post in build/ ...
    error_of(
        board.add_grant(&lead, "build/", "lead", "post").await,
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
    // ... nor remove the grant that keeps post and delete out of build/.
    error_of(
        remove(&board, &lead, &shadow).await,
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;
    // Nor can it drop its own grant on the way: that exposes the same verbs.
    error_of(
        remove(&board, &lead, &own).await,
        StatusCode::FORBIDDEN,
        codes::NO_AUTHORITY,
    )
    .await;

    // An administrator holding every verb can remove it.
    assert_eq!(
        remove(&board, &admin, &shadow).await.status(),
        StatusCode::OK
    );
}
