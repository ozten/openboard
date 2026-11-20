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
ALTER TABLE credentials ADD COLUMN parent_id TEXT REFERENCES credentials(id);
ALTER TABLE credentials ADD COLUMN board TEXT;
ALTER TABLE credentials ADD COLUMN scope TEXT;
ALTER TABLE credentials ADD COLUMN profile TEXT;
CREATE INDEX credentials_by_parent ON credentials(parent_id) WHERE parent_id IS NOT NULL;
ALTER TABLE channels ADD COLUMN intake INTEGER NOT NULL DEFAULT 0;
CREATE TABLE profile_limits (
    principal_id TEXT NOT NULL REFERENCES principals(id),
    board_id     INTEGER NOT NULL REFERENCES boards(id),
    profile      TEXT NOT NULL,
    scope        TEXT NOT NULL,
    set_ms       INTEGER NOT NULL,
    PRIMARY KEY (principal_id, board_id, profile)
);
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
    /// Whether the board exists.
    pub(crate) fn has_board(&self, board: &str) -> bool {
        self.boards.contains_key(&board_key(board))
    }

    pub(crate) fn grants(&self, board: &str) -> &[Grant] {
        self.boards
            .get(&board_key(board))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

pub(crate) fn snapshot(conn: &Connection) -> rusqlite::Result<Snapshot> {
    let mut boards: HashMap<String, Vec<Grant>> = HashMap::new();
    // Every board, so that one without grants is told apart from one that
    // does not exist.
    let names: Vec<String> = conn
        .prepare("SELECT name FROM boards")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for name in names {
        boards.entry(board_key(&name)).or_default();
    }
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

/// Who a grant is for, as a request names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Who {
    Handle(String),
    Group(Group),
    Everyone,
}

impl Who {
    pub(crate) fn parse(raw: &str) -> Self {
        if raw == "everyone" {
            return Who::Everyone;
        }
        match Group::parse(raw) {
            Some(group) => Who::Group(group),
            None => Who::Handle(raw.to_owned()),
        }
    }
}

/// The caller changing grants, as the store checks it.
#[derive(Debug, Clone)]
pub(crate) struct IssuerSpec {
    pub subject: crate::grants::Subject,
    /// The ceilings that apply on this board.
    pub ceilings: Vec<crate::grants::Ceiling>,
    pub derived: bool,
}

impl IssuerSpec {
    fn check<'a, T>(&self, work: impl FnOnce(&crate::grants::Issuer<'_>) -> T + 'a) -> T {
        let ceilings: Vec<&crate::grants::Ceiling> = self.ceilings.iter().collect();
        work(&crate::grants::Issuer {
            subject: &self.subject,
            ceilings: &ceilings,
            derived: self.derived,
        })
    }
}

/// Why a grant change or a channel creation was refused.
#[derive(Debug)]
pub(crate) enum GrantFailure {
    /// The grant breaks a rule every grant obeys, or the request is not one
    /// a grant change can be made with.
    Invalid(crate::grants::GrantError),
    /// The caller lacks the authority.
    NoAuthority(crate::grants::GrantError),
    NoSuchPrincipal(String),
    NoSuchGrant,
    /// The channel, or one under it, exists, or a grant reaches it.
    ChannelExists,
}

fn refused(error: crate::grants::GrantError) -> super::StoreError {
    use crate::grants::GrantError as E;
    super::StoreError::Grant(match error {
        E::DerivedToken | E::NoAuthority { .. } | E::OutlivesAuthority { .. } => {
            GrantFailure::NoAuthority(error)
        }
        E::NoSuchGrant => GrantFailure::NoSuchGrant,
        E::AlreadyReached => GrantFailure::ChannelExists,
        other => GrantFailure::Invalid(other),
    })
}

/// A grant just added.
#[derive(Debug)]
pub(crate) struct GrantAdded {
    pub grant: StoredGrant,
    pub authorized_by: String,
    /// The notices posted (Z9), and the event position of the last one.
    pub notices: Vec<String>,
    pub seq: Option<i64>,
}

/// A channel just created.
#[derive(Debug)]
pub(crate) struct ChannelMade {
    pub grants: Vec<StoredGrant>,
}

fn board_grants(conn: &Connection, board_id: i64) -> rusqlite::Result<Vec<StoredGrant>> {
    list(conn, Some(board_id))
}

fn channel_paths(conn: &Connection, board_id: i64) -> rusqlite::Result<Vec<String>> {
    conn.prepare("SELECT path FROM channels WHERE board_id = ?1 ORDER BY path")?
        .query_map([board_id], |row| row.get(0))?
        .collect()
}

impl super::Store {
    /// Adds a grant on `board` for `by`, checked against the grants as they
    /// are in the same transaction. A grant that gives its own issuer `read`
    /// in an existing channel it could not read posts a notice there (Z9).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_grant(
        &self,
        board: &super::BoardRef,
        by: &super::Writer,
        issuer: &IssuerSpec,
        pattern: Pattern,
        who: &Who,
        verbs: Verbs,
        expires_at: Option<i64>,
    ) -> super::Result<GrantAdded> {
        use crate::grants::{evaluate, Grant, Verb};
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        let grantee = match who {
            Who::Everyone => Grantee::Everyone,
            Who::Group(group) => Grantee::Group(*group),
            Who::Handle(handle) => match principal_by_handle(&tx, handle)? {
                Some((id, _, _)) => Grantee::Principal(id),
                None => {
                    return Err(super::StoreError::Grant(GrantFailure::NoSuchPrincipal(
                        handle.clone(),
                    )))
                }
            },
        };
        let mut new = Grant {
            id: String::new(),
            pattern,
            grantee,
            verbs,
            expires_at,
        };
        let held: Vec<Grant> = board_grants(&tx, board.id)?
            .into_iter()
            .map(|stored| stored.grant)
            .collect();
        let authorized_by = issuer
            .check(|iss| crate::grants::check_add(&held, iss, &new, now).map(|g| g.id.clone()))
            .map_err(refused)?;
        new.id = insert(
            &tx,
            board.id,
            &new.pattern,
            &new.grantee,
            new.verbs,
            new.expires_at,
            &by.actor.handle,
        )?;

        let mut notices = Vec::new();
        let mut seq = None;
        let mut after = held.clone();
        after.push(new.clone());
        for channel in channel_paths(&tx, board.id)? {
            let before = evaluate(&held, &issuer.subject, &channel, now);
            let now_reads = evaluate(&after, &issuer.subject, &channel, now);
            if before.verbs.contains(Verb::Read) || !now_reads.verbs.contains(Verb::Read) {
                continue;
            }
            let text = format!(
                "**Admin access:** {} gave themselves {} at `{}` (grant {}), so they can now \
                 read this channel. The board posts this notice whenever an administrator \
                 grants themselves access to a channel, and it cannot be turned off.",
                by.actor.handle, new.verbs, new.pattern, new.id
            );
            let inserted = super::insert_message(
                &tx,
                board,
                &channel,
                None,
                by,
                &text,
                &super::MentionInput {
                    candidates: Vec::new(),
                    local: false,
                },
            )?;
            tx.execute(
                "INSERT INTO labels (message_id, thread_id, key, value) VALUES (?1, ?1, ?2, ?3)",
                params![
                    inserted.id,
                    openboard_api::grants::NOTICE_LABEL,
                    openboard_api::grants::NOTICE_ACCESS_GRANTED
                ],
            )?;
            seq = Some(inserted.seq);
            notices.push(inserted.id);
        }
        let grant = board_grants(&tx, board.id)?
            .into_iter()
            .find(|stored| stored.grant.id == new.id)
            .ok_or_else(|| super::StoreError::Other("the new grant vanished".to_owned()))?;
        tx.commit()?;
        Ok(GrantAdded {
            grant,
            authorized_by,
            notices,
            seq,
        })
    }

    /// Removes a grant of `board`, checked against the grants as they are.
    /// Returns it, and the caller's grant that authorized the removal.
    pub(crate) fn remove_grant(
        &self,
        board: &super::BoardRef,
        by: &super::Writer,
        issuer: &IssuerSpec,
        id: &str,
    ) -> super::Result<(StoredGrant, String)> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let stored = board_grants(&tx, board.id)?;
        let held: Vec<Grant> = stored.iter().map(|stored| stored.grant.clone()).collect();
        let authorized_by = issuer
            .check(|iss| {
                crate::grants::check_remove(&held, iss, id, now_ms()).map(|g| g.id.clone())
            })
            .map_err(refused)?;
        let target = stored
            .into_iter()
            .find(|stored| stored.grant.id == id)
            .ok_or(super::StoreError::Grant(GrantFailure::NoSuchGrant))?;
        remove(&tx, board.id, id, &by.actor.handle)?;
        tx.commit()?;
        Ok((target, authorized_by))
    }

    /// The grants of `board` that `issuer` may manage.
    pub(crate) fn manageable_grants(
        &self,
        board: &super::BoardRef,
        issuer: &IssuerSpec,
    ) -> super::Result<Vec<StoredGrant>> {
        let stored = board_grants(&self.conn(), board.id)?;
        let held: Vec<Grant> = stored.iter().map(|stored| stored.grant.clone()).collect();
        let now = now_ms();
        Ok(stored
            .into_iter()
            .filter(|stored| {
                issuer.check(|iss| {
                    crate::grants::authority(&held, iss, &stored.grant.pattern, Verbs::NONE, now)
                        .is_ok()
                })
            })
            .collect())
    }

    /// The principal with this handle, as a subject of grants, with its
    /// handle as stored.
    pub(crate) fn principal_named(
        &self,
        handle: &str,
    ) -> super::Result<Option<(crate::grants::Subject, String)>> {
        Ok(principal_by_handle(&self.conn(), handle)?
            .map(|(id, kind, handle)| (crate::grants::Subject { id, kind }, handle)))
    }

    /// The grants of `board`, all of them.
    /// Limits profile `profile` of `principal_id` on `board` to `scope`
    /// (A8), replacing any earlier limit.
    pub(crate) fn set_profile_limit(
        &self,
        board: &super::BoardRef,
        principal_id: &str,
        profile: &str,
        scope: &[String],
    ) -> super::Result<()> {
        self.conn().execute(
            "INSERT INTO profile_limits (principal_id, board_id, profile, scope, set_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (principal_id, board_id, profile)
             DO UPDATE SET scope = excluded.scope, set_ms = excluded.set_ms",
            params![principal_id, board.id, profile, scope.join(" "), now_ms()],
        )?;
        Ok(())
    }

    /// Lifts a profile's limit. Whether there was one.
    pub(crate) fn clear_profile_limit(
        &self,
        board: &super::BoardRef,
        principal_id: &str,
        profile: &str,
    ) -> super::Result<bool> {
        Ok(self.conn().execute(
            "DELETE FROM profile_limits WHERE principal_id = ?1 AND board_id = ?2 AND profile = ?3",
            params![principal_id, board.id, profile],
        )? == 1)
    }

    /// The limited profiles of `principal_id` on `board`: (profile, scope).
    pub(crate) fn profile_limits(
        &self,
        board: &super::BoardRef,
        principal_id: &str,
    ) -> super::Result<Vec<(String, String)>> {
        Ok(self
            .conn()
            .prepare(
                "SELECT profile, scope FROM profile_limits
                  WHERE principal_id = ?1 AND board_id = ?2 ORDER BY profile",
            )?
            .query_map(params![principal_id, board.id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// What a request needs to judge its caller: the grants, and the limits
    /// on the profile it declared, by board name.
    pub(crate) fn authority_inputs(
        &self,
        principal_id: &str,
        profile: Option<&str>,
    ) -> super::Result<(Snapshot, Vec<(String, String)>)> {
        let conn = self.conn();
        let snapshot = snapshot(&conn)?;
        let limits = match profile {
            None => Vec::new(),
            Some(profile) => conn
                .prepare_cached(
                    "SELECT b.name, l.scope FROM profile_limits l JOIN boards b ON b.id = l.board_id
                      WHERE l.principal_id = ?1 AND l.profile = ?2",
                )?
                .query_map(params![principal_id, profile], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<rusqlite::Result<_>>()?,
        };
        Ok((snapshot, limits))
    }

    /// Whether the channel is an intake channel (S4); `None` when it does
    /// not exist.
    pub(crate) fn channel_intake(&self, board_id: i64, path: &str) -> super::Result<Option<bool>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT intake FROM channels WHERE board_id = ?1 AND path = ?2",
                params![board_id, path],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Sets a channel's mode. Whether the channel exists.
    pub(crate) fn set_channel_intake(
        &self,
        board_id: i64,
        path: &str,
        intake: bool,
    ) -> super::Result<bool> {
        Ok(self.conn().execute(
            "UPDATE channels SET intake = ?3 WHERE board_id = ?1 AND path = ?2",
            params![board_id, path, intake],
        )? == 1)
    }

    pub(crate) fn board_grants(&self, board_id: i64) -> super::Result<Vec<StoredGrant>> {
        Ok(board_grants(&self.conn(), board_id)?)
    }

    /// Creates an empty channel. A private one is closed to everyone and
    /// owned by its creator, in the same transaction (Z7). Fails if the
    /// channel or any channel under it exists, and for a private one if a
    /// grant already reaches it. The caller checked `create` for a public
    /// one.
    pub(crate) fn create_channel(
        &self,
        board: &super::BoardRef,
        by: &super::Writer,
        issuer: &IssuerSpec,
        path: &str,
        private: bool,
        intake: bool,
    ) -> super::Result<ChannelMade> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let taken = channel_paths(&tx, board.id)?.iter().any(|existing| {
            existing == path
                || existing
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'))
        });
        if taken {
            return Err(super::StoreError::Grant(GrantFailure::ChannelExists));
        }
        let mut made = Vec::new();
        if private {
            let held: Vec<Grant> = board_grants(&tx, board.id)?
                .into_iter()
                .map(|stored| stored.grant)
                .collect();
            let planned = issuer
                .check(|iss| {
                    crate::grants::private_channel(
                        &held,
                        iss,
                        path,
                        now_ms(),
                        (String::new(), String::new()),
                    )
                })
                .map_err(refused)?;
            let mut ids = Vec::new();
            for grant in &planned {
                ids.push(insert(
                    &tx,
                    board.id,
                    &grant.pattern,
                    &grant.grantee,
                    grant.verbs,
                    grant.expires_at,
                    &by.actor.handle,
                )?);
            }
            made = board_grants(&tx, board.id)?
                .into_iter()
                .filter(|stored| ids.contains(&stored.grant.id))
                .collect();
        }
        let now = crate::time::now_rfc3339();
        tx.execute(
            "INSERT INTO channels (board_id, path, created_at, thread_count, last_activity_at,
                                   intake)
             VALUES (?1, ?2, ?3, 0, ?3, ?4)",
            params![board.id, path, now, intake],
        )?;
        tx.commit()?;
        Ok(ChannelMade { grants: made })
    }
}
