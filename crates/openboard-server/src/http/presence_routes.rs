//! `GET /v1/boards/{board}/presence`: who is around on a board.
//!
//! The roster joins two sources: the in-memory registry, which knows the
//! requests seats have open and when each last made a request since the
//! server started, and the stored last-seen times, which carry "away"
//! across restarts. See `crate::presence`.

use std::collections::HashMap;

use axum::extract::rejection::PathRejection;
use axum::extract::{Extension, Path, State};
use axum::http::HeaderMap;
use axum::Json;
use openboard_api::{
    PresenceList, PresenceListen, PresencePrincipal, PresenceSeat, PresenceState, PresenceWindows,
};

use super::{duration_ms, ApiResult, Caller, Identity, Shared};
use crate::authz::{Action, Principal};
use crate::names::{self, ChannelSel};
use crate::presence::{Listen, State as SeatState, Windows};
use crate::time::{now_ms, rfc3339_from_ms};

const DAY_MS: i64 = 86_400_000;

/// A seat once the registry and the store have been joined.
struct Merged {
    principal_id: String,
    profile: Option<String>,
    last_seen_ms: i64,
    listening: Vec<Listen>,
}

pub(super) async fn list(
    State(state): State<Shared>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    board: Result<Path<String>, PathRejection>,
) -> ApiResult<Json<PresenceList>> {
    let caller = state.caller(&identity, &headers).await?;
    let board = state.board(&caller, board).await?;
    state.require(&caller, Action::Read, &board.name, None)?;

    let now = now_ms();
    let windows = Windows {
        live_ms: duration_ms(state.limits.presence_live),
        recent_ms: duration_ms(state.limits.presence_recent),
        listed_ms: duration_ms(state.limits.presence_listed),
    };
    let board_id = board.id;
    let since = now.saturating_sub(windows.listed_ms);
    let stored = state
        .db(move |state| state.store.presence_rows(board_id, since))
        .await?;

    let mut seats: HashMap<String, Merged> = HashMap::new();
    for row in stored {
        seats.insert(
            row.actor_id,
            Merged {
                principal_id: row.principal_id,
                profile: row.profile,
                last_seen_ms: row.last_seen_ms,
                listening: Vec::new(),
            },
        );
    }
    for seat in state.presence.seats(board_id) {
        let merged = seats.entry(seat.actor_id).or_insert_with(|| Merged {
            principal_id: seat.principal_id,
            profile: seat.profile,
            last_seen_ms: seat.last_request_ms,
            listening: Vec::new(),
        });
        merged.last_seen_ms = merged.last_seen_ms.max(seat.last_request_ms);
        merged.listening = seat.listening;
    }

    let mut ids: Vec<String> = seats
        .values()
        .map(|seat| seat.principal_id.clone())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let cards = state
        .db(move |state| state.store.principal_cards(&ids, now))
        .await?;

    let mut principals: HashMap<String, PresencePrincipal> = HashMap::new();
    for (actor_id, seat) in seats {
        let Some(seat_state) = windows.state(now, seat.last_seen_ms, !seat.listening.is_empty())
        else {
            continue;
        };
        let principal = match cards.get(&seat.principal_id) {
            Some(card) => Principal {
                id: seat.principal_id.clone(),
                kind: card.kind,
                handle: card.handle.clone(),
            },
            None if seat.principal_id == Principal::local().id => Principal::local(),
            // A principal that is gone: nothing to show it as.
            None => continue,
        };
        let last_seen_ms = if seat.listening.is_empty() {
            seat.last_seen_ms
        } else {
            now
        };
        let listening = seat
            .listening
            .into_iter()
            .filter(|listen| visible(&state, &caller, &board.name, listen.channel.as_deref()))
            .map(|listen| PresenceListen {
                channel: listen.channel,
                kind: listen.kind.as_str().to_owned(),
            })
            .collect();
        let entry = principals
            .entry(seat.principal_id.clone())
            .or_insert_with(|| PresencePrincipal {
                id: principal.id.clone(),
                handle: principal.handle.clone(),
                kind: principal.kind,
                state: PresenceState::Away,
                disabled: cards
                    .get(&seat.principal_id)
                    .and_then(|card| card.disabled)
                    .map(str::to_owned),
                last_seen_at: String::new(),
                seats: Vec::new(),
            });
        entry.seats.push(PresenceSeat {
            actor_id,
            handle: principal.actor(seat.profile.as_deref()).handle,
            profile: seat.profile,
            state: api_state(seat_state),
            last_seen_at: rfc3339_from_ms(last_seen_ms),
            listening,
        });
    }

    let mut principals: Vec<PresencePrincipal> = principals.into_values().collect();
    for principal in &mut principals {
        principal
            .seats
            .sort_by(|a, b| (a.state, &a.handle).cmp(&(b.state, &b.handle)));
        principal.state = principal
            .seats
            .iter()
            .map(|seat| seat.state)
            .min()
            .unwrap_or(PresenceState::Away);
        // RFC 3339 in UTC with fixed precision sorts as text.
        principal.last_seen_at = principal
            .seats
            .iter()
            .map(|seat| seat.last_seen_at.clone())
            .max()
            .unwrap_or_default();
    }
    principals.sort_by(|a, b| (a.state, &a.handle).cmp(&(b.state, &b.handle)));

    Ok(Json(PresenceList {
        now: rfc3339_from_ms(now),
        windows: PresenceWindows {
            live_secs: u64::try_from(windows.live_ms / 1000).unwrap_or(0),
            recent_secs: u64::try_from(windows.recent_ms / 1000).unwrap_or(0),
            listed_days: u64::try_from(windows.listed_ms / DAY_MS).unwrap_or(0),
        },
        principals,
    }))
}

/// Whether the caller may see that a seat listens on `channel`. A request
/// on the whole board names no channel and is always shown.
fn visible(state: &Shared, caller: &Caller, board: &str, channel: Option<&str>) -> bool {
    let Some(raw) = channel else {
        return true;
    };
    match names::parse_channel_sel(raw) {
        Ok(ChannelSel::Exact(path) | ChannelSel::Subtree(path)) => {
            state.allows(caller, Action::Read, board, Some(&path))
        }
        Err(_) => false,
    }
}

fn api_state(state: SeatState) -> PresenceState {
    match state {
        SeatState::Live => PresenceState::Live,
        SeatState::Recent => PresenceState::Recent,
        SeatState::Away => PresenceState::Away,
    }
}
