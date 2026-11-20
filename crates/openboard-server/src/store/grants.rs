//! Grants in the database: the schema, the access mode of the instance,
//! the grants a new board starts with, and the snapshot requests are
//! authorized against. The rules themselves are in [`crate::grants`].

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};

use crate::grants::{Grant, Grantee, Group, Pattern, Verbs};
use crate::identity;
use crate::names::{self, board_key};
use crate::time::now_ms;

/// From `user_version` 4 to 5: grants. A removed grant keeps its row, with
/// who removed it and when, so the table is also the log of grant changes.
pub(crate) const MIGRATE_V4_TO_V5: &str = "
CREATE TABLE grants (
    id          TEXT PRIMARY KEY,
    board_id    INTEGER NOT NULL REFERENCES boards(id),
    pattern     TEXT NOT NULL,
    grantee     TEXT NOT NULL,
    verbs       TEXT NOT NULL,
    expires_ms  INTEGER,
    created_by  TEXT NOT NULL,
    created_ms  INTEGER NOT NULL,
    removed_by  TEXT,
    removed_ms  INTEGER
);
CREATE INDEX grants_by_board ON grants(board_id) WHERE removed_ms IS NULL;
";

/// The `meta` key of the instance's access mode.
pub(crate) const META_ACCESS: &str = "access";

/// Who `created_by` and `removed_by` name for a change made through
/// `openboard admin` on the data directory, or by a migration.
pub(crate) const OPERATOR: &str = "operator";

/// How open an instance is to the principals enrolled on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Default deny (Z1). Only the operator creates boards, and a new board
    /// admits only its creator.
    Closed,
    /// Any enrolled agent or person may create a board, and every board
    /// starts admitting all agents and people. What a board from before M4
    /// gets on upgrade.
    Open,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Closed => "closed",
            Access::Open => "open",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "closed" => Some(Access::Closed),
            "open" => Some(Access::Open),
            _ => None,
        }
    }
}

/// The verbs all agents and all people hold on every board of an open
/// instance.
pub(crate) const OPEN_VERBS: &str = "read,post,react,label,create,task";

pub(crate) fn access(conn: &Connection) -> rusqlite::Result<Access> {
    Ok(identity::meta_get(conn, META_ACCESS)?
        .as_deref()
        .and_then(Access::parse)
        .unwrap_or(Access::Closed))
}

pub(crate) fn set_access(conn: &Connection, access: Access) -> rusqlite::Result<()> {
    identity::meta_set(conn, META_ACCESS, access.as_str())
}

/// Whether the instance has recorded an access mode yet.
pub(crate) fn access_recorded(conn: &Connection) -> rusqlite::Result<bool> {
    Ok(identity::meta_get(conn, META_ACCESS)?.is_some())
}

/// The migration to version 5: today's access, kept. Every board gets the
/// open grants, and the instance becomes open, so boards created later
/// admit everyone too.
pub(crate) fn migrate_v4_to_v5(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(MIGRATE_V4_TO_V5)?;
    let boards: Vec<i64> = conn
        .prepare("SELECT id FROM boards ORDER BY id")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for board in boards {
        insert_open_grants(conn, board)?;
    }
    set_access(conn, Access::Open)
}

fn insert_open_grants(conn: &Connection, board_id: i64) -> rusqlite::Result<()> {
    let verbs = Verbs::parse(OPEN_VERBS).expect("the open verbs parse");
    for group in [Group::Agents, Group::People] {
        insert(
            conn,
            board_id,
            &Pattern::Board,
            &Grantee::Group(group),
            verbs,
            None,
            OPERATOR,
        )?;
    }
    Ok(())
}

/// The grants a board starts with when it is created now: its creator, if
/// one is named, gets `all` at `*`; on an open instance all agents and
/// people get the open verbs.
pub(crate) fn board_created(
    conn: &Connection,
    board_id: i64,
    creator: Option<(&str, &str)>,
) -> rusqlite::Result<()> {
    if let Some((principal_id, handle)) = creator {
        insert(
            conn,
            board_id,
            &Pattern::Board,
            &Grantee::Principal(principal_id.to_owned()),
            Verbs::all(),
            None,
            handle,
        )?;
    }
    if access(conn)? == Access::Open {
        insert_open_grants(conn, board_id)?;
    }
    Ok(())
}

/// Writes a grant and returns its ID.
pub(crate) fn insert(
    conn: &Connection,
    board_id: i64,
    pattern: &Pattern,
    grantee: &Grantee,
    verbs: Verbs,
    expires_ms: Option<i64>,
    created_by: &str,
) -> rusqlite::Result<String> {
    let hex: String = conn.query_row("SELECT lower(hex(randomblob(8)))", [], |row| row.get(0))?;
    let id = format!("grt_{hex}");
    conn.execute(
        "INSERT INTO grants (id, board_id, pattern, grantee, verbs, expires_ms, created_by, created_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            board_id,
            pattern.to_string(),
            grantee_text(grantee),
            verbs.to_string(),
            expires_ms,
            created_by,
            now_ms()
        ],
    )?;
    Ok(id)
}

/// Marks a grant removed. Returns false when there is no such grant on the
/// board, or it was removed already.
pub(crate) fn remove(
    conn: &Connection,
    board_id: i64,
    id: &str,
    removed_by: &str,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE grants SET removed_by = ?1, removed_ms = ?2
         WHERE id = ?3 AND board_id = ?4 AND removed_ms IS NULL",
        params![removed_by, now_ms(), id, board_id],
    )? == 1)
}

fn grantee_text(grantee: &Grantee) -> String {
    match grantee {
        Grantee::Principal(id) => format!("principal:{id}"),
        Grantee::Group(group) => format!("group:{}", group.name()),
        Grantee::Everyone => "everyone".to_owned(),
    }
}

fn grantee_from(text: &str) -> Option<Grantee> {
    if text == "everyone" {
        return Some(Grantee::Everyone);
    }
    if let Some(id) = text.strip_prefix("principal:") {
        return Some(Grantee::Principal(id.to_owned()));
    }
    text.strip_prefix("group:")
        .and_then(Group::parse)
        .map(Grantee::Group)
}

/// A grant as stored, with who made it.
#[derive(Debug, Clone)]
pub(crate) struct StoredGrant {
    pub grant: Grant,
    pub board: String,
    /// The handle of the grantee principal, for a grant to one.
    pub grantee_handle: Option<String>,
    pub created_by: String,
    pub created_ms: i64,
}

/// The grants that are not removed, of one board or of every board,
/// oldest first. Expired grants are included: [`Grant::is_active`] tells.
pub(crate) fn list(conn: &Connection, board_id: Option<i64>) -> rusqlite::Result<Vec<StoredGrant>> {
    let mut statement = conn.prepare(
        "SELECT g.id, b.name, g.pattern, g.grantee, g.verbs, g.expires_ms, g.created_by,
                g.created_ms, p.handle
         FROM grants g
         JOIN boards b ON b.id = g.board_id
         LEFT JOIN principals p ON g.grantee = 'principal:' || p.id
         WHERE g.removed_ms IS NULL AND (?1 IS NULL OR g.board_id = ?1)
         ORDER BY g.created_ms, g.rowid",
    )?;
    let rows = statement.query_map([board_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, Option<String>>(8)?,
        ))
    })?;
    let mut grants = Vec::new();
    for row in rows {
        let (id, board, pattern, grantee, verbs, expires_ms, created_by, created_ms, handle) = row?;
        // A row that does not parse is skipped rather than trusted: a grant
        // that cannot be read gives nothing.
        let (Ok(pattern), Some(grantee), Ok(verbs)) = (
            Pattern::parse(&pattern, names::validate_channel),
            grantee_from(&grantee),
            Verbs::parse(&verbs),
        ) else {
            continue;
        };
        grants.push(StoredGrant {
            grant: Grant {
                id,
                pattern,
                grantee,
                verbs,
                expires_at: expires_ms,
            },
            board,
            grantee_handle: handle,
            created_by,
            created_ms,
        });
    }
    Ok(grants)
}

/// Every board's grants and the instance's access mode, as requests are
/// authorized against them. Cheap to clone.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub access: Access,
    /// By board key.
    pub boards: Arc<HashMap<String, Vec<Grant>>>,
}

impl Snapshot {
    pub(crate) fn grants(&self, board: &str) -> &[Grant] {
        self.boards
            .get(&board_key(board))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

pub(crate) fn snapshot(conn: &Connection) -> rusqlite::Result<Snapshot> {
    let mut boards: HashMap<String, Vec<Grant>> = HashMap::new();
    for stored in list(conn, None)? {
        boards
            .entry(board_key(&stored.board))
            .or_default()
            .push(stored.grant);
    }
    Ok(Snapshot {
        access: access(conn)?,
        boards: Arc::new(boards),
    })
}

/// The principal with this handle, compared as handles are.
pub(crate) fn principal_by_handle(
    conn: &Connection,
    handle: &str,
) -> rusqlite::Result<Option<(String, openboard_api::ActorKind, String)>> {
    conn.query_row(
        "SELECT id, kind, handle FROM principals WHERE handle_key = ?1",
        [names::handle_key(handle)],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )
    .optional()
    .map(|found| {
        found.and_then(|(id, kind, handle)| {
            identity::parse_actor_kind(&kind).map(|kind| (id, kind, handle))
        })
    })
}

/// Whether any active grant on any board gives `grant.manage`.
pub(crate) fn anyone_manages(conn: &Connection) -> rusqlite::Result<bool> {
    let now = now_ms();
    Ok(list(conn, None)?.iter().any(|stored| {
        stored.grant.is_active(now)
            && stored.grant.grantee != Grantee::Everyone
            && stored
                .grant
                .verbs
                .contains(crate::grants::Verb::GrantManage)
    }))
}
