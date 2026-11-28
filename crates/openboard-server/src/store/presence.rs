//! The last-seen time of each seat on each board: the only presence state
//! that is written to disk. One row per board and actor, replaced on each
//! update, and removed once it is older than the roster lists.

use std::collections::HashMap;

use openboard_api::ActorKind;
use rusqlite::{params, OptionalExtension};

use super::{upsert_actor, Result, Store, Writer};
use crate::identity::parse_actor_kind;

/// From `user_version` 5 to 6: presence.
pub(super) const MIGRATE_V5_TO_V6: &str = "
CREATE TABLE presence (
    board_id     INTEGER NOT NULL REFERENCES boards(id),
    actor_id     TEXT    NOT NULL REFERENCES actors(id),
    last_seen_ms INTEGER NOT NULL,
    PRIMARY KEY (board_id, actor_id)
) WITHOUT ROWID;
";

/// A stored last-seen time, with who the seat is.
#[derive(Debug, Clone)]
pub(crate) struct SeenRow {
    pub actor_id: String,
    pub principal_id: String,
    pub profile: Option<String>,
    pub last_seen_ms: i64,
}

/// What the roster shows about a principal besides its seats.
#[derive(Debug, Clone)]
pub(crate) struct PrincipalCard {
    pub handle: String,
    pub kind: ActorKind,
    /// `Some("revoked")` or `Some("expired")` for an agent that has no
    /// usable token left.
    pub disabled: Option<&'static str>,
}

impl Store {
    /// Records that `by` was seen on the board at `seen_ms`. A time older
    /// than the one stored is ignored.
    pub(crate) fn record_presence(&self, board_id: i64, by: &Writer, seen_ms: i64) -> Result<()> {
        let conn = self.conn();
        upsert_actor(&conn, by)?;
        conn.execute(
            "INSERT INTO presence (board_id, actor_id, last_seen_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT (board_id, actor_id)
             DO UPDATE SET last_seen_ms = max(last_seen_ms, excluded.last_seen_ms)",
            params![board_id, by.actor.id, seen_ms],
        )?;
        Ok(())
    }

    /// The seats seen on a board at or after `since_ms`.
    pub(crate) fn presence_rows(&self, board_id: i64, since_ms: i64) -> Result<Vec<SeenRow>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT p.actor_id, a.principal, a.profile, p.last_seen_ms
             FROM presence p JOIN actors a ON a.id = p.actor_id
             WHERE p.board_id = ?1 AND p.last_seen_ms >= ?2",
        )?;
        let rows = statement
            .query_map(params![board_id, since_ms], |row| {
                Ok(SeenRow {
                    actor_id: row.get(0)?,
                    principal_id: row.get(1)?,
                    profile: row.get(2)?,
                    last_seen_ms: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Handle, kind and token state of each principal in `ids`. A principal
    /// with no row (the local principal of a local board) is left out.
    pub(crate) fn principal_cards(
        &self,
        ids: &[String],
        now_ms: i64,
    ) -> Result<HashMap<String, PrincipalCard>> {
        let conn = self.conn();
        let mut cards = HashMap::new();
        let mut principal = conn.prepare("SELECT handle, kind FROM principals WHERE id = ?1")?;
        // Only an agent's own tokens count: derived tokens come and go
        // with the processes it starts, and a person's browser sessions
        // end as a matter of course.
        let mut tokens = conn.prepare(
            "SELECT count(*),
                    coalesce(sum(revoked_ms IS NULL AND expires_ms > ?2), 0),
                    coalesce(sum(revoked_ms IS NULL), 0)
             FROM credentials WHERE principal_id = ?1 AND parent_id IS NULL",
        )?;
        for id in ids {
            let Some((handle, kind)) = principal
                .query_row(params![id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .optional()?
            else {
                continue;
            };
            let Some(kind) = parse_actor_kind(&kind) else {
                continue;
            };
            let disabled = if kind == ActorKind::Agent {
                let (all, usable, unrevoked): (i64, i64, i64) = tokens
                    .query_row(params![id, now_ms], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?;
                match (all, usable, unrevoked) {
                    (0, _, _) | (_, 1.., _) => None,
                    (_, 0, 0) => Some("revoked"),
                    _ => Some("expired"),
                }
            } else {
                None
            };
            cards.insert(
                id.clone(),
                PrincipalCard {
                    handle,
                    kind,
                    disabled,
                },
            );
        }
        Ok(cards)
    }

    /// Removes last-seen times older than `before_ms`. Returns how many.
    pub(crate) fn prune_presence(&self, before_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM presence WHERE last_seen_ms < ?1",
            params![before_ms],
        )?)
    }
}
