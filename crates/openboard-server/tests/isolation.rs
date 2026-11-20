//! Gate T5, isolation (milestone M4): forged and cross-board IDs, cursor
//! reuse, path traversal, injection-shaped input and cross-origin requests.
//! Every reference that crosses a board, or names something the caller
//! cannot read, gets the answer for something that does not exist (Z15).
//!
//! The board makes no outbound requests and takes no uploads yet, so SSRF
//! and malicious uploads have nothing to test; they join this suite with
//! the features that bring them.

mod common;

use common::{error_of, Board};
use openboard_api::codes;
use reqwest::StatusCode;
use serde_json::{json, Value};

/// A board with `general` and `other`, an admin of both, and a thread and
/// a task on `other`.
async fn two_boards() -> (Board, String, String, String) {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    board.grant("*", "admin", "all");
    openboard_server::operator::grant(&board.data_dir(), "other", "*", "admin", "all", None)
        .unwrap();
    board.posted(&admin, "notes", "on general").await;
    let elsewhere: Value = board
        .post_json(
            &admin,
            "/v1/boards/other/messages",
            json!({ "channel": "notes", "text": "on other" }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let task: Value = board
        .post_json(
            &admin,
            "/v1/boards/other/tasks",
            json!({ "channel": "notes", "text": "a task on other" }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        board,
        admin,
        elsewhere["message"]["id"].as_str().unwrap().to_owned(),
        task["task"]["id"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn an_id_from_another_board_is_not_found_like_a_forged_one() {
    let (board, admin, other_message, other_task) = two_boards().await;
    for id in [
        other_message.as_str(),
        other_task.as_str(),
        "msg_00000000deadbeef00000000",
    ] {
        for path in [
            format!("/v1/boards/general/threads/{id}"),
            format!("/v1/boards/general/tasks/{id}"),
        ] {
            let response = board.get(&admin, &path).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
        let reply = board
            .post_json(
                &admin,
                "/v1/boards/general/messages",
                json!({ "thread_id": id, "text": "across boards" }),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::NOT_FOUND, "reply to {id}");
        let edit = board
            .http
            .patch(board.at(&format!("/v1/boards/general/messages/{id}")))
            .bearer_auth(&admin)
            .json(&json!({ "text": "edited" }))
            .send()
            .await
            .unwrap();
        assert_eq!(edit.status(), StatusCode::NOT_FOUND, "edit {id}");
        let events = board
            .get(&admin, &format!("/v1/boards/general/events?thread={id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(events.status(), StatusCode::NOT_FOUND, "events of {id}");
    }
}

#[tokio::test]
async fn a_task_cannot_need_a_task_on_another_board() {
    let (board, admin, _, other_task) = two_boards().await;
    for need in [other_task.as_str(), "msg_00000000deadbeef00000000"] {
        let created = board
            .post_json(
                &admin,
                "/v1/boards/general/tasks",
                json!({ "channel": "notes", "text": "needs", "needs": [need] }),
            )
            .send()
            .await
            .unwrap();
        error_of(created, StatusCode::BAD_REQUEST, codes::INVALID_NEEDS).await;
    }
}

#[tokio::test]
async fn a_cursor_from_another_board_is_refused() {
    let (board, admin, _, _) = two_boards().await;
    let other: Value = board
        .get(&admin, "/v1/boards/other/events")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cursor = other["cursor"].as_str().unwrap();
    for path in [
        format!("/v1/boards/general/events?after={cursor}"),
        format!("/v1/boards/general/threads?channel=notes&cursor={cursor}"),
    ] {
        let response = board.get(&admin, &path).send().await.unwrap();
        error_of(response, StatusCode::BAD_REQUEST, codes::INVALID_CURSOR).await;
    }
}

#[tokio::test]
async fn traversal_shaped_names_are_refused_and_reach_nothing() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    board.grant("*", "admin", "all");
    board.grant("hr/", "everyone", "none");
    board.posted(&admin, "notes", "fine").await;
    for channel in [
        "../hr",
        "notes/../hr",
        "notes//x",
        "/notes",
        "notes/",
        "%2e%2e",
        "NOTES",
        "notes\u{0}hr",
        "hr\\payroll",
    ] {
        let response = board.post(&admin, channel, "where does this go").await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "posting to {channel:?}"
        );
    }
    for board_name in ["..", "general%2f..", "general%00", "a%2fb"] {
        let response = board
            .get(&admin, &format!("/v1/boards/{board_name}/channels"))
            .send()
            .await
            .unwrap();
        assert!(
            matches!(
                response.status(),
                StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND
            ),
            "{board_name}: {}",
            response.status()
        );
    }
    for pattern in ["../", "hr/../*", "**", "hr/*", ""] {
        let response = board.add_grant(&admin, pattern, "admin", "read").await;
        error_of(response, StatusCode::BAD_REQUEST, codes::INVALID_GRANT).await;
    }
}

#[tokio::test]
async fn injection_shaped_input_is_data() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    board.grant("*", "admin", "all");
    let nasty = "x'); DROP TABLE messages; --";
    let posted = board.posted(&admin, "notes", nasty).await;
    assert_eq!(posted.message.text, nasty);

    let refused = board.post(&admin, "notes' OR '1'='1", "hi").await;
    error_of(refused, StatusCode::BAD_REQUEST, codes::INVALID_CHANNEL).await;
    let filtered = board
        .get(
            &admin,
            "/v1/boards/general/threads?channel=notes&label=topic%3D'%20OR%201%3D1%20--",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(filtered.status(), StatusCode::OK);
    let filtered: Value = filtered.json().await.unwrap();
    assert!(
        filtered["threads"].as_array().unwrap().is_empty(),
        "{filtered}"
    );
    let grantee = board
        .add_grant(&admin, "notes", "x' OR 1=1 --", "read")
        .await;
    assert_eq!(grantee.status(), StatusCode::BAD_REQUEST);
    let explained = board
        .get(
            &admin,
            "/v1/boards/general/explain?channel=notes&for=admin'%20OR%20'1'%3D'1",
        )
        .send()
        .await
        .unwrap();
    assert!(
        explained.status().is_client_error(),
        "{}",
        explained.status()
    );
    let profile = board
        .post_json(
            &admin,
            "/v1/boards/general/messages",
            json!({ "channel": "notes", "text": "hi" }),
        )
        .header(openboard_api::PROFILE_HEADER, "fx'--")
        .send()
        .await
        .unwrap();
    error_of(profile, StatusCode::BAD_REQUEST, codes::INVALID_PROFILE).await;

    // Everything is still there.
    let threads: Value = board
        .get(&admin, "/v1/boards/general/threads?channel=notes")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(threads["threads"][0]["root"]["text"], nasty);
}

#[tokio::test]
async fn other_origins_get_no_cross_origin_access() {
    let board = Board::closed().await;
    let admin = board.enrol("admin").await;
    board.grant("*", "admin", "all");
    let read = board
        .get(&admin, "/v1/boards/general/channels")
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert!(read.headers().get("access-control-allow-origin").is_none());
    let preflight = board
        .http
        .request(
            reqwest::Method::OPTIONS,
            board.at("/v1/boards/general/messages"),
        )
        .header("Origin", "https://evil.example")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();
    assert!(preflight
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    assert!(!preflight.status().is_success(), "{}", preflight.status());
    // A form post, which a page on another origin can send without a
    // preflight, is refused for not being JSON.
    let form = board
        .http
        .post(board.at("/v1/boards/general/messages"))
        .bearer_auth(&admin)
        .header("Origin", "https://evil.example")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("channel=notes&text=hi")
        .send()
        .await
        .unwrap();
    assert!(form.status().is_client_error(), "{}", form.status());
}
