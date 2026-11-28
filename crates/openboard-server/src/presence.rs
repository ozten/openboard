//! Presence: who is around on each board, kept in memory.
//!
//! A seat is one actor (a principal plus an optional profile) on one board.
//! The registry knows two things about a seat: when it last made a request
//! to the board, and which long-polls and streams it has open there now.
//! Nothing here is history. The only presence state written to disk is one
//! last-seen time per board and seat (see `Store::record_presence`), which
//! lets "away · 3d ago" survive a restart.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// What kind of open request a seat is listening with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListenKind {
    /// `GET /events` held open (`openboard wait`).
    Wait,
    /// `GET /events/stream` (`openboard tail`, the web UI).
    Stream,
    /// `POST /tasks/claim` with a wait (`openboard tasks run --wait`).
    ClaimWait,
}

impl ListenKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Stream => "stream",
            Self::ClaimWait => "claim_wait",
        }
    }
}

/// Who a seat is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeatId {
    pub board_id: i64,
    pub actor_id: String,
    pub principal_id: String,
    pub profile: Option<String>,
}

/// One open request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listen {
    /// The channel or channel selector waited on; `None` for the whole board.
    pub channel: Option<String>,
    pub kind: ListenKind,
}

#[derive(Debug)]
struct Seat {
    principal_id: String,
    profile: Option<String>,
    last_request_ms: i64,
    /// When the last-seen time was last handed out to be written.
    flushed_ms: Option<i64>,
    listening: HashMap<u64, Listen>,
}

/// A seat as [`Presence::seats`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeatView {
    pub actor_id: String,
    pub principal_id: String,
    pub profile: Option<String>,
    pub last_request_ms: i64,
    pub listening: Vec<Listen>,
}

#[derive(Default)]
struct Inner {
    seats: HashMap<(i64, String), Seat>,
    next_listen: u64,
}

/// The registry. One per server.
#[derive(Default)]
pub(crate) struct Presence {
    inner: Mutex<Inner>,
}

impl Presence {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Records a request by `seat` at `now_ms`. Returns true when its
    /// last-seen time should be written now: the first time the seat is
    /// seen since the server started, and then at most once every
    /// `flush_every_ms`.
    pub(crate) fn touch(&self, seat: &SeatId, now_ms: i64, flush_every_ms: i64) -> bool {
        let mut inner = self.lock();
        let entry = entry(&mut inner, seat, now_ms);
        entry.last_request_ms = entry.last_request_ms.max(now_ms);
        let due = entry
            .flushed_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= flush_every_ms);
        if due {
            entry.flushed_ms = Some(now_ms);
        }
        due
    }

    /// Records that `seat` has a request open until the guard is dropped.
    pub(crate) fn listen(
        self: &Arc<Self>,
        seat: &SeatId,
        listen: Listen,
        now_ms: i64,
    ) -> ListenGuard {
        let mut inner = self.lock();
        let id = inner.next_listen;
        inner.next_listen = inner.next_listen.wrapping_add(1);
        let entry = entry(&mut inner, seat, now_ms);
        entry.last_request_ms = entry.last_request_ms.max(now_ms);
        entry.listening.insert(id, listen);
        ListenGuard {
            presence: Arc::clone(self),
            key: (seat.board_id, seat.actor_id.clone()),
            id,
        }
    }

    /// Every seat this server has seen on a board since it started.
    pub(crate) fn seats(&self, board_id: i64) -> Vec<SeatView> {
        let inner = self.lock();
        let mut seats: Vec<SeatView> = inner
            .seats
            .iter()
            .filter(|((board, _), _)| *board == board_id)
            .map(|((_, actor_id), seat)| {
                let mut ids: Vec<&u64> = seat.listening.keys().collect();
                ids.sort_unstable();
                SeatView {
                    actor_id: actor_id.clone(),
                    principal_id: seat.principal_id.clone(),
                    profile: seat.profile.clone(),
                    last_request_ms: seat.last_request_ms,
                    listening: ids
                        .into_iter()
                        .map(|id| seat.listening[id].clone())
                        .collect(),
                }
            })
            .collect();
        seats.sort_by(|a, b| a.actor_id.cmp(&b.actor_id));
        seats
    }

    /// Open requests over all boards, for tests.
    #[cfg(test)]
    pub(crate) fn open_listens(&self) -> usize {
        self.lock()
            .seats
            .values()
            .map(|seat| seat.listening.len())
            .sum()
    }
}

fn entry<'a>(inner: &'a mut Inner, seat: &SeatId, now_ms: i64) -> &'a mut Seat {
    inner
        .seats
        .entry((seat.board_id, seat.actor_id.clone()))
        .or_insert_with(|| Seat {
            principal_id: seat.principal_id.clone(),
            profile: seat.profile.clone(),
            last_request_ms: now_ms,
            flushed_ms: None,
            listening: HashMap::new(),
        })
}

/// Holds one open request in the registry. Dropping it, however the
/// request ends, takes the request out and counts as the seat's latest
/// request.
pub(crate) struct ListenGuard {
    presence: Arc<Presence>,
    key: (i64, String),
    id: u64,
}

impl Drop for ListenGuard {
    fn drop(&mut self) {
        let now = crate::time::now_ms();
        let mut inner = self.presence.lock();
        if let Some(seat) = inner.seats.get_mut(&self.key) {
            seat.listening.remove(&self.id);
            seat.last_request_ms = seat.last_request_ms.max(now);
        }
    }
}

/// A seat's or principal's state, from the newest of its signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum State {
    // Ordered best first, so that the minimum is the best state.
    Live,
    Recent,
    Away,
}

/// The windows that decide a state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Windows {
    pub live_ms: i64,
    pub recent_ms: i64,
    pub listed_ms: i64,
}

impl Windows {
    /// `None` when the seat is too old to be listed at all.
    pub(crate) fn state(&self, now_ms: i64, last_seen_ms: i64, listening: bool) -> Option<State> {
        let age = now_ms.saturating_sub(last_seen_ms);
        if listening || age < self.live_ms {
            Some(State::Live)
        } else if age < self.recent_ms {
            Some(State::Recent)
        } else if age < self.listed_ms {
            Some(State::Away)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(board_id: i64, actor: &str) -> SeatId {
        SeatId {
            board_id,
            actor_id: actor.to_owned(),
            principal_id: format!("prn_{actor}"),
            profile: None,
        }
    }

    const WINDOWS: Windows = Windows {
        live_ms: 120_000,
        recent_ms: 900_000,
        listed_ms: 30 * 86_400_000,
    };

    #[test]
    fn states_follow_the_windows() {
        let now = 10_000_000_000;
        assert_eq!(WINDOWS.state(now, now, false), Some(State::Live));
        assert_eq!(WINDOWS.state(now, now - 119_999, false), Some(State::Live));
        assert_eq!(
            WINDOWS.state(now, now - 120_000, false),
            Some(State::Recent)
        );
        assert_eq!(
            WINDOWS.state(now, now - 899_999, false),
            Some(State::Recent)
        );
        assert_eq!(WINDOWS.state(now, now - 900_000, false), Some(State::Away));
        assert_eq!(
            WINDOWS.state(now, now - WINDOWS.listed_ms + 1, false),
            Some(State::Away)
        );
        assert_eq!(WINDOWS.state(now, now - WINDOWS.listed_ms, false), None);
        // An open request is live however old the last request was.
        assert_eq!(WINDOWS.state(now, now - 5_000_000, true), Some(State::Live));
    }

    #[test]
    fn the_best_state_is_the_minimum() {
        assert_eq!(
            [State::Away, State::Live, State::Recent].into_iter().min(),
            Some(State::Live)
        );
    }

    #[test]
    fn touch_asks_for_a_write_at_most_once_per_interval() {
        let presence = Presence::default();
        let a = seat(1, "a");
        assert!(
            presence.touch(&a, 1_000, 60_000),
            "the first sighting is written"
        );
        assert!(!presence.touch(&a, 2_000, 60_000));
        assert!(!presence.touch(&a, 60_999, 60_000));
        assert!(presence.touch(&a, 61_000, 60_000));
        assert_eq!(presence.seats(1)[0].last_request_ms, 61_000);
        // A seat is per board.
        assert!(presence.touch(&seat(2, "a"), 61_500, 60_000));
        assert!(presence.seats(3).is_empty());
    }

    #[test]
    fn guards_add_and_remove_listens() {
        let presence = Arc::new(Presence::default());
        let a = seat(1, "a");
        let wait = presence.listen(
            &a,
            Listen {
                channel: Some("ops/x".to_owned()),
                kind: ListenKind::Wait,
            },
            1_000,
        );
        let stream = presence.listen(
            &a,
            Listen {
                channel: None,
                kind: ListenKind::Stream,
            },
            1_000,
        );
        let seats = presence.seats(1);
        assert_eq!(seats.len(), 1);
        assert_eq!(seats[0].listening.len(), 2);
        assert_eq!(seats[0].listening[0].channel.as_deref(), Some("ops/x"));
        drop(wait);
        assert_eq!(presence.seats(1)[0].listening.len(), 1);
        drop(stream);
        assert_eq!(presence.open_listens(), 0);
        // Ending a request counts as the seat's latest request.
        assert!(presence.seats(1)[0].last_request_ms > 1_000);
    }
}
