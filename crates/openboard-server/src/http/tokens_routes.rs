//! `/v1/tokens`: derived tokens (A9, A10). A token minted from the caller's
//! own credential for one board, with a scope its access is intersected with
//! at every request, an expiry no later than its parent's, and its own
//! credential ID to revoke.

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, PathRejection};
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use openboard_api::codes;
use openboard_api::tokens::{
    NewToken, TokenInfo, TokenList, TokenMinted, DEFAULT_TOKEN_TTL_SECS, MAX_SCOPE_ENTRIES,
    MAX_TOKEN_TTL_SECS,
};

use super::{bad_path, read_json, ApiResult, AppError, Identity, Shared};
use crate::authz::Action;
use crate::grants::Ceiling;
use crate::identity::{Credential, Derivation, MAX_TOKEN_DEPTH};
use crate::names;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::invalid(message).next("see `openboard agent token --help`")
}

fn info(credential: &Credential, board: String, scope: &str) -> TokenInfo {
    TokenInfo {
        credential: credential.info(),
        board,
        can: scope.split_whitespace().map(str::to_owned).collect(),
    }
}

/// The credential behind the request: derived tokens exist on network
/// boards only.
fn credential_of(identity: &Identity) -> ApiResult<Credential> {
    identity.credential.clone().ok_or_else(|| {
        AppError::invalid("a local board has no tokens: everyone on it can do everything")
            .next("derived tokens are for network boards")
    })
}

/// Parses and normalizes a scope: each entry `pattern=verbs`, written back
/// in the board's own spelling.
pub(super) fn parse_scope(entries: &[String]) -> ApiResult<Vec<String>> {
    if entries.is_empty() {
        return Err(invalid(
            "a derived token needs a scope: one or more pattern=verbs entries",
        ));
    }
    if entries.len() > MAX_SCOPE_ENTRIES {
        return Err(invalid(format!(
            "a scope has at most {MAX_SCOPE_ENTRIES} entries"
        )));
    }
    entries
        .iter()
        .map(|raw| {
            let (pattern, verbs) = Ceiling::parse_entry(raw, names::validate_channel)
                .map_err(|error| invalid(format!("{raw}: {error}")))?;
            Ok(format!("{pattern}={verbs}"))
        })
        .collect()
}

/// `POST /v1/tokens`.
pub(super) async fn mint(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> ApiResult<(StatusCode, Json<TokenMinted>)> {
    state.require_global(&identity.principal, Action::Session)?;
    let parent = credential_of(&identity)?;
    let request: NewToken = read_json(&headers, body, "token", || {
        AppError::invalid("the request body is too large for a token")
            .status(StatusCode::PAYLOAD_TOO_LARGE)
    })?;
    names::validate_board(&request.board).map_err(invalid)?;
    let scope = parse_scope(&request.can)?;
    if let Some(profile) = &request.profile {
        names::validate_profile(profile).map_err(invalid)?;
        if parent
            .profile
            .as_ref()
            .is_some_and(|bound| bound != profile)
        {
            return Err(AppError::new(
                codes::DERIVED_TOKEN,
                "this token is bound to another profile, and so is every token minted from it",
            ));
        }
    }
    let ttl_secs = match request.ttl_secs {
        None => DEFAULT_TOKEN_TTL_SECS,
        Some(0) => return Err(invalid("ttl_secs must be more than zero")),
        Some(secs) if secs > MAX_TOKEN_TTL_SECS => {
            return Err(invalid(format!(
                "a derived token lasts at most {} days",
                MAX_TOKEN_TTL_SECS / 86_400
            )))
        }
        Some(secs) => secs,
    };
    let client = identity.client;
    let board = request.board.clone();
    let profile = request.profile.clone();
    let stored_scope = scope.clone();
    let minted = state
        .db(move |state| {
            state.store.derive_token(
                &parent,
                &Derivation {
                    board: &board,
                    scope: &stored_scope,
                    ttl_ms: i64::try_from(ttl_secs * 1000).unwrap_or(i64::MAX),
                    profile: profile.as_deref(),
                },
                client,
            )
        })
        .await?;
    let Some((token, credential)) = minted else {
        return Err(AppError::new(
            codes::DERIVED_TOKEN,
            format!("tokens derive at most {MAX_TOKEN_DEPTH} deep"),
        )
        .next("mint from a token higher up the chain"));
    };
    Ok((
        StatusCode::CREATED,
        Json(TokenMinted {
            token,
            info: info(&credential, request.board, &scope.join(" ")),
        }),
    ))
}

/// `GET /v1/tokens`: the live derived tokens the caller may revoke.
pub(super) async fn list(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
) -> ApiResult<Json<TokenList>> {
    state.require_global(&identity.principal, Action::Session)?;
    let caller = credential_of(&identity)?;
    let tokens = state
        .db(move |state| state.store.derived_tokens(&caller))
        .await?;
    Ok(Json(TokenList {
        tokens: tokens
            .into_iter()
            .map(|(credential, board, scope)| info(&credential, board, &scope))
            .collect(),
    }))
}

/// `DELETE /v1/tokens/{id}`: revokes a derived token the caller may revoke,
/// which ends every token derived from it.
pub(super) async fn revoke(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    id: Result<Path<String>, PathRejection>,
) -> ApiResult<Json<TokenInfo>> {
    state.require_global(&identity.principal, Action::Session)?;
    let caller = credential_of(&identity)?;
    let Path(id) = id.map_err(|_| bad_path())?;
    let client = identity.client;
    let revoked = state
        .db(move |state| state.store.revoke_token(&caller, &id, client))
        .await?;
    let (credential, board, scope) = revoked.ok_or_else(|| {
        AppError::new(codes::NOT_FOUND, "no such live derived token of yours")
            .next("list them with `openboard agent tokens`")
    })?;
    let mut credential = credential;
    credential.revoked_ms = Some(crate::time::now_ms());
    Ok(Json(info(&credential, board, &scope)))
}
