//! What a caller learns about boards, channels and messages it cannot read,
//! and the verbs that deleting a message takes. `docs/permissions.md`
//! ("Visibility", "What the actions need") is the specification.

mod common;

use common::{error_of, Board};
use openboard_api::codes;
use openboard_server::operator;
use reqwest::StatusCode;
use serde_json::json;

/// The status and body `token` gets for `path`; an error is its code.
async fn answer(board: &Board, token: &str, path: &str) -> (StatusCode, serde_json::Value) {
    let response = board.get(token, path).send().await.unwrap();
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap();
    match body["error"]["code"].as_str() {
        Some(code) => (status, json!(code)),
        None => (status, body),
    }
}

#[tokio::test]
async fn a_missing_board_answers_like_an_unreadable_one() {
    let board = Board::closed().await;
    let agent = board.enrol("agent-a").await;
    // A second board that exists and that the agent cannot read.
    board.enrol("someone-else").await;
    operator::grant(&board.data_dir(), "vault", "*", "someone-else", "all", None).unwrap();
    // A derived token comes from a principal that holds something.
    let holder = board.enrol("holder").await;
    board.grant("*", "holder", "read,post,create");
    let derived = board.minted(&holder, &["*=read"]).await.token;

    for token in [&agent, &derived] {
        for suffix in [
            "channels",
            "threads?channel=notes",
            "events",
            "explain?channel=notes",
        ] {
            let existing = answer(&board, token, &format!("/v1/boards/vault/{suffix}")).await;
            let missing = answer(&board, token, &format!("/v1/boards/ghost/{suffix}")).await;
            assert_eq!(existing, missing, "{suffix}");
            // `explain` of one's own access needs no verb, so on either board
            // it says the same thing: nothing.
            if suffix.starts_with("explain") {
                assert_eq!(existing.0, StatusCode::OK);
                assert_eq!(existing.1["verbs"], "none");
            } else {
                assert_eq!(existing, (StatusCode::FORBIDDEN, json!(codes::FORBIDDEN)));
            }
        }
    }
}

#[tokio::test]
async fn a_first_post_to_a_new_board_still_creates_it_on_an_open_instance() {
    let board = Board::start(true).await;
    let agent = board.enrol("agent-a").await;
    let made = board
        .post_json(
            &agent,
            "/v1/boards/fresh/messages",
            json!({ "channel": "notes", "text": "first" }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(made.status(), StatusCode::CREATED);
    let listed = board
        .get(&agent, "/v1/boards/fresh/channels")
        .send()
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
}

/// A board where ada posted a message, and others hold different verbs.
async fn moderated() -> (Board, String, String, String, String, String) {
    let board = Board::closed().await;
    let ada = board.enrol("ada").await;
    let poster = board.enrol("poster").await;
    let moderator = board.enrol("moderator").await;
    let reader = board.enrol("reader").await;
    board.grant("*", "ada", "read,post,create");
    board.grant("*", "poster", "read,post");
    board.grant("*", "moderator", "read,delete");
    board.grant("*", "reader", "read");
    let id = board.posted(&ada, "notes", "spam").await.message.id;
    (board, ada, poster, moderator, reader, id)
}

fn delete(board: &Board, token: &str, id: &str) -> reqwest::RequestBuilder {
    board
        .http
        .delete(board.at(&format!("/v1/boards/general/messages/{id}")))
        .bearer_auth(token)
}

#[tokio::test]
async fn delete_without_post_removes_someone_elses_message() {
    let (board, _, _, moderator, _, id) = moderated().await;
    let deleted: serde_json::Value = delete(&board, &moderator, &id)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(deleted["deleted_at"].is_string(), "{deleted}");
}

#[tokio::test]
async fn post_without_delete_cannot_remove_someone_elses_message() {
    let (board, _, poster, _, reader, id) = moderated().await;
    for token in [&poster, &reader] {
        error_of(
            delete(&board, token, &id).send().await.unwrap(),
            StatusCode::FORBIDDEN,
            codes::FORBIDDEN,
        )
        .await;
    }
}

#[tokio::test]
async fn delete_without_post_covers_ones_own_message_and_read_alone_does_not() {
    let board = Board::closed().await;
    let ada = board.enrol("ada").await;
    board.grant("*", "ada", "read,post,create,delete");
    let id = board.posted(&ada, "notes", "mine").await.message.id;
    // The same author, on tokens narrowed below `post`.
    let reads = board.minted(&ada, &["*=read"]).await.token;
    error_of(
        delete(&board, &reads, &id).send().await.unwrap(),
        StatusCode::FORBIDDEN,
        codes::FORBIDDEN,
    )
    .await;
    let moderates = board.minted(&ada, &["*=read,delete"]).await.token;
    let deleted: serde_json::Value = delete(&board, &moderates, &id)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(deleted["deleted_at"].is_string(), "{deleted}");
}

#[tokio::test]
async fn an_author_with_read_and_post_deletes_its_own_message() {
    let (board, ada, _, _, _, id) = moderated().await;
    let deleted: serde_json::Value = delete(&board, &ada, &id)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(deleted["deleted_at"].is_string(), "{deleted}");
}

#[tokio::test]
async fn a_message_in_an_unreadable_channel_is_not_found_to_a_moderator() {
    let (board, ada, _, _, _, _) = moderated().await;
    let hidden = board.enrol("hidden-mod").await;
    board.grant("*", "hidden-mod", "read,delete");
    board.grant("notes/", "hidden-mod", "none");
    let id = board.posted(&ada, "notes", "mine").await.message.id;
    error_of(
        delete(&board, &hidden, &id).send().await.unwrap(),
        StatusCode::NOT_FOUND,
        codes::NOT_FOUND,
    )
    .await;
}

#[tokio::test]
async fn public_creation_does_not_reveal_a_hidden_child_but_private_stays_refused() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    let ada = board.enrol("ada").await;
    board.grant("*", "admin", "all");
    board.grant("*", "ada", "read,post,create");
    let secret = board
        .post_json(
            &admin,
            "/v1/boards/general/channels",
            json!({ "path": "hr/plans", "private": true }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(secret.status(), StatusCode::CREATED);

    let private = board
        .post_json(
            &ada,
            "/v1/boards/general/channels",
            json!({ "path": "hr", "private": true }),
        )
        .send()
        .await
        .unwrap();
    error_of(private, StatusCode::CONFLICT, codes::CHANNEL_EXISTS).await;

    let public = board
        .post_json(&ada, "/v1/boards/general/channels", json!({ "path": "hr" }))
        .send()
        .await
        .unwrap();
    assert_eq!(public.status(), StatusCode::CREATED);
    // The exact path is still taken.
    let again = board
        .post_json(&ada, "/v1/boards/general/channels", json!({ "path": "hr" }))
        .send()
        .await
        .unwrap();
    error_of(again, StatusCode::CONFLICT, codes::CHANNEL_EXISTS).await;
}
