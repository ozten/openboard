//! The grant routes: list, add and remove grants, `explain`, and channel
//! creation. See `openboard_api::grants` for the contract and
//! `docs/permissions.md` for the rules.
//!
//! Every change is checked by the store against the grants as they are in
//! the change's own transaction, not against the snapshot the request was
//! authenticated with, so two changes cannot authorize each other.

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, PathRejection, QueryRejection};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use openboard_api::codes;
use openboard_api::grants::{
    ChannelCreated, Explained, Grant as GrantView, GrantChanged, GrantList, NewChannel, NewGrant,
};
use serde::Deserialize;

use super::{
    bad_path, bad_query, invalid_channel, read_json, ApiResult, AppError, Caller, Identity, Shared,
};
use crate::authz::Action;
use crate::grants::{self, Grant, GrantError, Grantee, Pattern, Verbs};
use crate::names;
use crate::store::grants::{GrantFailure, IssuerSpec, StoredGrant, Who};
use crate::store::BoardRef;
use crate::time::{now_ms, rfc3339_from_ms};
use crate::Mode;

/// The error for a refused grant change or channel creation.
pub(super) fn grant_error(error: GrantFailure) -> AppError {
    match error {
        GrantFailure::Invalid(error) => AppError::new(codes::INVALID_GRANT, error.to_string())
            .next("see `openboard grant --help` for patterns, names and verbs"),
        GrantFailure::NoAuthority(error) => AppError::new(codes::NO_AUTHORITY, error.to_string())
            .next(
                "ask someone with grant.manage there to make the change; `openboard explain` \
                 shows what you hold",
            ),
        GrantFailure::NoSuchPrincipal(name) => AppError::new(
            codes::NOT_FOUND,
            format!("no agent or person is named {name}"),
        )
        .next("check the name; agents and people are enrolled by the board's operator"),
        GrantFailure::NoSuchGrant => AppError::new(codes::NOT_FOUND, "no such grant on this board")
            .next("list the grants you manage with `openboard grants`"),
        GrantFailure::ChannelExists => AppError::new(
            codes::CHANNEL_EXISTS,
            "the channel or a channel under it exists, or a grant already reaches it",
        )
        .next("choose another path; a private channel must start empty and ungranted"),
    }
}

fn no_grants_here() -> AppError {
    AppError::new(
        codes::INVALID_GRANT,
        "a local board has no grants: everyone on it can do everything",
    )
    .next("grants apply to network boards; start one with `openboard serve --network`")
}

pub(super) fn view(stored: &StoredGrant, now: i64) -> GrantView {
    let grant = &stored.grant;
    GrantView {
        id: grant.id.clone(),
        pattern: grant.pattern.to_string(),
        grantee: match &grant.grantee {
            Grantee::Principal(id) => stored.grantee_handle.clone().unwrap_or_else(|| id.clone()),
            Grantee::Group(group) => group.name().to_owned(),
            Grantee::Everyone => "everyone".to_owned(),
        },
        verbs: grant.verbs.to_string(),
        expires_at: grant.expires_at.map(rfc3339_from_ms),
        expired: !grant.is_active(now),
        created_by: stored.created_by.clone(),
        created_at: rfc3339_from_ms(stored.created_ms),
    }
}

/// The caller as the store checks a grant change on `board`.
fn issuer(caller: &Caller, board: &str) -> ApiResult<IssuerSpec> {
    let authority = caller
        .authority
        .as_ref()
        .ok_or_else(|| AppError::internal(&"a network caller without authority"))?;
    Ok(IssuerSpec {
        subject: authority.subject.clone(),
        ceilings: authority.ceilings_for(board),
        derived: authority.derived,
    })
}

/// A grant change through a derived token is refused before anything else
/// is looked at (A10).
fn changing_issuer(caller: &Caller, board: &str) -> ApiResult<IssuerSpec> {
    let issuer = issuer(caller, board)?;
    if issuer.derived {
        return Err(
            AppError::new(codes::DERIVED_TOKEN, "a derived token cannot change grants")
                .next("change grants with your own login, not an agent token"),
        );
    }
    Ok(issuer)
}

fn parse_pattern(raw: &str) -> ApiResult<Pattern> {
    Pattern::parse(raw, names::validate_channel)
        .map_err(|error| grant_error(GrantFailure::Invalid(error)))
}

fn parse_verbs(raw: &str) -> ApiResult<Verbs> {
    Verbs::parse(raw).map_err(|error| grant_error(GrantFailure::Invalid(error)))
}

/// `GET /v1/boards/{board}/grants`: the grants the caller may manage.
pub(super) async fn list_grants(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
) -> ApiResult<Json<GrantList>> {
    let caller = state.caller(&identity, &headers).await?;
    if state.mode == Mode::Local {
        return Err(no_grants_here());
    }
    let board = state.board(&caller, board).await?;
    let issuer = issuer(&caller, &board.name)?;
    let listed = board.clone();
    let grants = state
        .db(move |state| state.store.manageable_grants(&listed, &issuer))
        .await?;
    // A caller who manages nothing here and can read nothing on the board
    // is refused as for a board that does not exist. One who manages grants
    // in channels it cannot read still sees those.
    if grants.is_empty() {
        state.require(&caller, Action::Read, &board.name, None)?;
    }
    let now = now_ms();
    Ok(Json(GrantList {
        grants: grants.iter().map(|stored| view(stored, now)).collect(),
    }))
}

/// `POST /v1/boards/{board}/grants`.
pub(super) async fn add_grant(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<(StatusCode, Json<GrantChanged>)> {
    let caller = state.caller(&identity, &headers).await?;
    if state.mode == Mode::Local {
        return Err(no_grants_here());
    }
    let board = state.board(&caller, board).await?;
    let request: NewGrant = read_json(&headers, body, "grant", || {
        AppError::invalid("the request body is too large for a grant")
            .status(StatusCode::PAYLOAD_TOO_LARGE)
    })?;
    let pattern = parse_pattern(&request.pattern)?;
    let verbs = parse_verbs(&request.verbs)?;
    let who = Who::parse(&request.grantee);
    if let Who::Handle(handle) = &who {
        names::validate_handle(handle)
            .or_else(|_| names::validate_email(handle).map(|_| ()))
            .map_err(|_| {
                grant_error(GrantFailure::Invalid(GrantError::BadPattern(
                    "the grantee is not an agent's name, an email address, a group or everyone"
                        .to_owned(),
                )))
            })?;
    }
    let expires_at = match request.expires_in_secs {
        None => None,
        Some(0) => {
            return Err(grant_error(GrantFailure::Invalid(GrantError::BadPattern(
                "expires_in_secs must be more than zero".to_owned(),
            ))))
        }
        Some(secs) => Some(
            now_ms().saturating_add(i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX)),
        ),
    };
    let issuer = changing_issuer(&caller, &board.name)?;
    let writer = caller.writer();
    let on = board.clone();
    let added = state
        .db(move |state| {
            let added = state
                .store
                .add_grant(&on, &writer, &issuer, pattern, &who, verbs, expires_at)?;
            if let Some(seq) = added.seq {
                state.hub.publish(on.id, seq);
            }
            Ok(added)
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(GrantChanged {
            grant: view(&added.grant, now_ms()),
            authorized_by: added.authorized_by,
            notices: added.notices,
        }),
    ))
}

/// `DELETE /v1/boards/{board}/grants/{id}`.
pub(super) async fn remove_grant(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> ApiResult<Json<GrantChanged>> {
    let caller = state.caller(&identity, &headers).await?;
    if state.mode == Mode::Local {
        return Err(no_grants_here());
    }
    let Path((board_name, id)) = path.map_err(|_| bad_path())?;
    let board = state.board_named(&caller, &board_name).await?;
    let issuer = changing_issuer(&caller, &board.name)?;
    let writer = caller.writer();
    let on = board.clone();
    let removed = state
        .db(move |state| {
            let removed = state.store.remove_grant(&on, &writer, &issuer, &id)?;
            if let Some(seq) = removed.seq {
                state.hub.publish(on.id, seq);
            }
            Ok(removed)
        })
        .await?;
    Ok(Json(GrantChanged {
        grant: view(&removed.grant, now_ms()),
        authorized_by: removed.authorized_by,
        notices: removed.notices,
    }))
}

#[derive(Debug, Deserialize)]
pub(super) struct ExplainQuery {
    channel: String,
    #[serde(rename = "for")]
    for_: Option<String>,
}

/// `GET /v1/boards/{board}/explain?channel=<path>[&for=<handle>]`. Anyone may
/// explain their own access. Explaining someone else's needs `grant.manage`
/// at `*` (Z4, Z12).
pub(super) async fn explain(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    query: Result<Query<ExplainQuery>, QueryRejection>,
) -> ApiResult<Json<Explained>> {
    let caller = state.caller(&identity, &headers).await?;
    let Query(query) = query.map_err(bad_query)?;
    names::validate_channel(&query.channel).map_err(invalid_channel)?;
    let (board_name, board) = state.board_or_unreadable(&caller, board).await?;
    let Some(authority) = caller.authority.clone() else {
        return Ok(Json(Explained {
            channel: query.channel,
            principal: caller.actor().handle,
            verbs: Verbs::all().to_string(),
            deciding: Vec::new(),
            narrowed: false,
        }));
    };
    // A board that does not exist and that the caller could not read has no
    // grants, which is what an existing board nobody granted it looks like.
    let stored = match board.as_ref().map(|board| board.id) {
        Some(board_id) => {
            state
                .db(move |state| state.store.board_grants(board_id))
                .await?
        }
        None => Vec::new(),
    };
    let now = now_ms();
    let held: Vec<Grant> = stored.iter().map(|stored| stored.grant.clone()).collect();
    let decided = |access: &grants::Access| -> Vec<GrantView> {
        stored
            .iter()
            .filter(|stored| access.deciding.contains(&stored.grant.id))
            .map(|stored| view(stored, now))
            .collect()
    };
    match &query.for_ {
        None => {
            let access = authority.access(&board_name, &query.channel);
            Ok(Json(Explained {
                deciding: decided(&access),
                channel: query.channel,
                principal: caller.actor().handle,
                verbs: access.verbs.to_string(),
                narrowed: access.narrowed,
            }))
        }
        Some(handle) => {
            let issuer = issuer(&caller, &board_name)?;
            let allowed = {
                let ceilings: Vec<&grants::Ceiling> = issuer.ceilings.iter().collect();
                grants::authority(
                    &held,
                    &grants::Issuer {
                        subject: &issuer.subject,
                        ceilings: &ceilings,
                        derived: issuer.derived,
                    },
                    &Pattern::Board,
                    Verbs::NONE,
                    now,
                )
                .is_ok()
            };
            if !allowed {
                return Err(AppError::new(
                    codes::NO_AUTHORITY,
                    "explaining someone else's access needs grant.manage at *",
                )
                .next("explain your own access by leaving out `for`"));
            }
            let target = handle.clone();
            let found = state
                .db(move |state| state.store.principal_named(&target))
                .await?
                .ok_or_else(|| grant_error(GrantFailure::NoSuchPrincipal(handle.clone())))?;
            let access = grants::evaluate(&held, &found.0, &query.channel, now);
            Ok(Json(Explained {
                deciding: decided(&access),
                channel: query.channel,
                principal: found.1,
                verbs: access.verbs.to_string(),
                narrowed: false,
            }))
        }
    }
}

/// `POST /v1/boards/{board}/channels`.
pub(super) async fn create_channel(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<(StatusCode, Json<ChannelCreated>)> {
    let caller = state.caller(&identity, &headers).await?;
    let board: BoardRef = state.board(&caller, board).await?;
    let request: NewChannel = read_json(&headers, body, "channel", || {
        AppError::invalid("the request body is too large for a channel")
            .status(StatusCode::PAYLOAD_TOO_LARGE)
    })?;
    names::validate_channel(&request.path).map_err(invalid_channel)?;
    if request.private && state.mode == Mode::Local {
        return Err(no_grants_here());
    }
    if !request.private {
        state.require(
            &caller,
            Action::CreateChannel,
            &board.name,
            Some(&request.path),
        )?;
    }
    let issuer = match state.mode {
        Mode::Local => IssuerSpec {
            subject: grants::Subject {
                id: caller.principal.id.clone(),
                kind: caller.principal.kind,
            },
            ceilings: Vec::new(),
            derived: false,
        },
        Mode::Network => issuer(&caller, &board.name)?,
    };
    let writer = caller.writer();
    let (path, private, intake) = (request.path.clone(), request.private, request.intake);
    let made = state
        .db(move |state| {
            state
                .store
                .create_channel(&board, &writer, &issuer, &path, private, intake)
        })
        .await?;
    let now = now_ms();
    Ok((
        StatusCode::CREATED,
        Json(ChannelCreated {
            path: request.path,
            private: request.private,
            intake: request.intake,
            grants: made.grants.iter().map(|stored| view(stored, now)).collect(),
        }),
    ))
}

/// `POST /v1/boards/{board}/channels/mode`: makes a channel a work or an
/// intake channel (S4). It takes `grant.manage` there, as it changes what
/// may be done in it.
pub(super) async fn set_channel_mode(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<Json<openboard_api::grants::ChannelMode>> {
    let caller = state.caller(&identity, &headers).await?;
    let board: BoardRef = state.board(&caller, board).await?;
    let request: openboard_api::grants::ChannelMode =
        read_json(&headers, body, "channel mode", || {
            AppError::invalid("the request body is too large for a channel mode")
                .status(StatusCode::PAYLOAD_TOO_LARGE)
        })?;
    names::validate_channel(&request.path).map_err(invalid_channel)?;
    state.require(&caller, Action::Read, &board.name, Some(&request.path))?;
    if let Some(authority) = &caller.authority {
        if authority.derived {
            return Err(AppError::new(
                codes::DERIVED_TOKEN,
                "a derived token cannot change a channel's mode",
            ));
        }
        if !authority
            .access(&board.name, &request.path)
            .verbs
            .contains(grants::Verb::GrantManage)
        {
            return Err(AppError::new(
                codes::NO_AUTHORITY,
                "changing a channel's mode takes grant.manage there",
            )
            .next("`openboard explain <channel>` shows what you hold"));
        }
    }
    let (board_id, path, intake) = (board.id, request.path.clone(), request.intake);
    let found = state
        .db(move |state| state.store.set_channel_intake(board_id, &path, intake))
        .await?;
    if !found {
        return Err(
            AppError::new(codes::NOT_FOUND, "no such channel on this board")
                .next("create it with `openboard channels create <path> --intake`"),
        );
    }
    Ok(Json(request))
}
