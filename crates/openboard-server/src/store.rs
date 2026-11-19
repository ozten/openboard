//! SQLite storage. Everything here blocks, so callers run it on a blocking
//! thread. Every mutation is one transaction.

use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use openboard_api::{event_types, Actor, ChannelInfo, Event, Message, Thread, ThreadSummary};
use rusqlite::types::Value;
use rusqlite::{
    params, params_from_iter, Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior,
};

use crate::chain::{event_hash, EventFields, GENESIS_HASH, HASH_VERSION};
use crate::identity::{
    self, actor_kind_name, log_kinds, parse_actor_kind, AuthOutcome, Credential, LogEntry,
};
use crate::limits::LogRetention;
use crate::names::{board_key, ChannelSel};
use crate::time::{now_ms, now_rfc3339, rfc3339_from_ms};
use crate::tls::{CERT_FILE_NAME, KEY_FILE_NAME};
use crate::Mode;

/// File name of the board database inside the data directory.
pub const DB_FILE_NAME: &str = "openboard.db";
/// File name of the lock a running server holds on the data directory.
pub const LOCK_FILE_NAME: &str = "openboard.lock";

pub(crate) const SCHEMA_VERSION: i64 = 2;

/// The schema milestone M0 shipped, `user_version` 1. It is never edited:
/// later versions are reached by running the migrations below on top of it,
/// for a new database exactly as for an old one.
const SCHEMA_V1: &str = "
CREATE TABLE boards (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    name_key   TEXT NOT NULL UNIQUE,
    tag        TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    head_seq   INTEGER NOT NULL DEFAULT 0,
    head_hash  TEXT NOT NULL
);
CREATE TABLE actors (
    id        TEXT PRIMARY KEY,
    kind      TEXT NOT NULL,
    principal TEXT NOT NULL,
    profile   TEXT,
    handle    TEXT NOT NULL
);
CREATE TABLE channels (
    board_id         INTEGER NOT NULL REFERENCES boards(id),
    path             TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    thread_count     INTEGER NOT NULL,
    last_activity_at TEXT NOT NULL,
    PRIMARY KEY (board_id, path)
) WITHOUT ROWID;
CREATE TABLE messages (
    id         TEXT PRIMARY KEY,
    board_id   INTEGER NOT NULL REFERENCES boards(id),
    channel    TEXT NOT NULL,
    thread_id  TEXT NOT NULL,
    actor_id   TEXT NOT NULL REFERENCES actors(id),
    text       TEXT NOT NULL,
    created_at TEXT NOT NULL,
    revision   INTEGER NOT NULL DEFAULT 1,
    seq        INTEGER NOT NULL
);
CREATE INDEX messages_by_thread ON messages(thread_id, seq);
CREATE TABLE threads (
    root_id          TEXT PRIMARY KEY REFERENCES messages(id),
    board_id         INTEGER NOT NULL REFERENCES boards(id),
    channel          TEXT NOT NULL,
    reply_count      INTEGER NOT NULL DEFAULT 0,
    last_seq         INTEGER NOT NULL,
    last_activity_at TEXT NOT NULL
);
CREATE UNIQUE INDEX threads_by_activity ON threads(board_id, last_seq);
CREATE TABLE events (
    board_id   INTEGER NOT NULL REFERENCES boards(id),
    seq        INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    channel    TEXT NOT NULL,
    thread_id  TEXT NOT NULL,
    message_id TEXT NOT NULL,
    actor_id   TEXT NOT NULL REFERENCES actors(id),
    at         TEXT NOT NULL,
    prev_hash  TEXT NOT NULL,
    hash       TEXT NOT NULL,
    PRIMARY KEY (board_id, seq)
) WITHOUT ROWID;
CREATE TRIGGER events_are_append_only_update BEFORE UPDATE ON events
BEGIN SELECT RAISE(ABORT, 'the event log is append-only'); END;
CREATE TRIGGER events_are_append_only_delete BEFORE DELETE ON events
BEGIN SELECT RAISE(ABORT, 'the event log is append-only'); END;
CREATE TABLE idempotency (
    board_id    INTEGER NOT NULL REFERENCES boards(id),
    actor_id    TEXT NOT NULL,
    key         TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    message_id  TEXT NOT NULL REFERENCES messages(id),
    PRIMARY KEY (board_id, actor_id, key)
) WITHOUT ROWID;
";

/// From `user_version` 1 to 2: modes, principals, credentials, invites, the
/// security log, and the credential behind each message and event. Events
/// written before this keep hash encoding 1.
const MIGRATE_V1_TO_V2: &str = "
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE principals (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    handle     TEXT NOT NULL,
    handle_key TEXT NOT NULL UNIQUE,
    created_ms INTEGER NOT NULL
);
CREATE TABLE credentials (
    id           TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL REFERENCES principals(id),
    token_hash   TEXT NOT NULL UNIQUE,
    created_ms   INTEGER NOT NULL,
    expires_ms   INTEGER NOT NULL,
    revoked_ms   INTEGER,
    last_used_ms INTEGER
);
CREATE INDEX credentials_by_principal ON credentials(principal_id);
CREATE TABLE invites (
    id           TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL REFERENCES principals(id),
    code_hash    TEXT NOT NULL UNIQUE,
    token_ttl_ms INTEGER NOT NULL,
    created_ms   INTEGER NOT NULL,
    expires_ms   INTEGER NOT NULL,
    used_ms      INTEGER,
    revoked_ms   INTEGER
);
CREATE INDEX invites_by_principal ON invites(principal_id);
CREATE TABLE security_log (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    at            TEXT NOT NULL,
    kind          TEXT NOT NULL,
    principal     TEXT,
    credential_id TEXT,
    client        TEXT,
    detail        TEXT
);
CREATE TRIGGER security_log_is_append_only_update BEFORE UPDATE ON security_log
BEGIN SELECT RAISE(ABORT, 'the security log is append-only'); END;
CREATE TRIGGER security_log_is_append_only_delete BEFORE DELETE ON security_log
BEGIN SELECT RAISE(ABORT, 'the security log is append-only'); END;
ALTER TABLE messages ADD COLUMN credential_id TEXT;
ALTER TABLE events ADD COLUMN credential_id TEXT;
ALTER TABLE events ADD COLUMN hash_version INTEGER NOT NULL DEFAULT 1;
";

const MESSAGE_COLUMNS: &str = "m.id, m.channel, m.thread_id, m.text, m.created_at, m.revision, \
                               a.id, a.kind, a.handle, a.profile, m.credential_id";
/// Number of columns in [`MESSAGE_COLUMNS`].
const MESSAGE_COLUMN_COUNT: usize = 11;

#[derive(Debug)]
pub(crate) enum StoreError {
    /// The idempotency key was already used with a different request.
    IdempotencyConflict,
    /// The thread a reply names does not exist on this board.
    ThreadMissing,
    /// Another server holds the lock on the data directory.
    DataDirInUse,
    Sqlite(rusqlite::Error),
    Other(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::IdempotencyConflict => f.write_str("idempotency conflict"),
            StoreError::ThreadMissing => f.write_str("thread missing"),
            StoreError::DataDirInUse => f.write_str("the data directory is in use"),
            StoreError::Sqlite(error) => write!(f, "sqlite: {error}"),
            StoreError::Other(detail) => f.write_str(detail),
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        StoreError::Sqlite(error)
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// A board as the handlers need it.
#[derive(Debug, Clone)]
pub(crate) struct BoardRef {
    pub id: i64,
    /// The name as first written.
    pub name: String,
    /// Random per-board value embedded in cursors, so that a cursor from
    /// another board is rejected.
    pub tag: String,
}

/// Where a post goes.
#[derive(Debug, Clone)]
pub(crate) enum PostTarget {
    Board(BoardRef),
    /// A board with this name, created with the message if it does not
    /// exist yet.
    NewBoard(String),
}

pub(crate) struct NewPost {
    pub channel: String,
    /// The root to reply under, or `None` to start a thread.
    pub thread_id: Option<String>,
    pub actor: Actor,
    /// The principal behind `actor`.
    pub principal_id: String,
    /// The credential the caller authenticated with. `None` on a local board.
    pub credential_id: Option<String>,
    pub text: String,
    /// Key and request fingerprint.
    pub idempotency: Option<(String, String)>,
}

pub(crate) struct Posted {
    /// The board the message landed on.
    pub board: BoardRef,
    pub message: Message,
    /// Position of the `message.posted` event.
    pub seq: i64,
    pub duplicate: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EventFilter {
    pub channel: Option<ChannelSel>,
    pub thread: Option<String>,
    pub not_from: Option<String>,
    /// Skip events written by the actor with this ID.
    pub not_from_actor: Option<String>,
}

pub(crate) struct EventPage {
    /// Matching events, oldest first, each with its position.
    pub events: Vec<(i64, Event)>,
    /// Every event up to and including this position has been considered.
    pub scanned_to: i64,
    /// True when the page filled, so later matches may already exist.
    pub more: bool,
}

pub(crate) struct ThreadPage {
    /// Summaries, newest activity first, each with its activity position.
    pub threads: Vec<(i64, ThreadSummary)>,
    pub more: bool,
}

/// What a start does to the directory's recorded mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeChange {
    /// The record already says it.
    Keep,
    /// A new directory: its first start decides.
    First,
    /// A local directory becomes a network one, for good.
    Upgrade,
    /// The directory is a network one by its contents, but its record was
    /// missing or said local.
    Repair,
}

/// The mode a start will run in, settled before anything is committed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ModePlan {
    pub mode: Mode,
    change: ModeChange,
    /// Whether a certificate fingerprint was ever recorded, after which
    /// clients may have pinned it.
    pub pinned: bool,
}

pub(crate) struct Store {
    data_dir: PathBuf,
    conn: Mutex<Connection>,
    /// Holds the exclusive lock on the data directory. Closing the file
    /// releases it, so it is declared after the connection and outlives it.
    _lock: File,
}

impl Store {
    /// Opens the database in `data_dir` for a server, creating the
    /// directory and the schema when they do not exist.
    ///
    /// The store is the only writer of the data directory for as long as it
    /// lives: it holds an exclusive lock on [`LOCK_FILE_NAME`] and fails with
    /// [`StoreError::DataDirInUse`] when another store already holds it.
    pub(crate) fn open(data_dir: &Path) -> Result<Self> {
        create_private_dir(data_dir).map_err(|error| {
            StoreError::Other(format!(
                "cannot create data directory {}: {error}",
                data_dir.display()
            ))
        })?;
        // Taken before the database is touched.
        let lock = lock_data_dir(data_dir)?;
        make_private(data_dir).map_err(StoreError::Other)?;
        let mut conn = Connection::open(data_dir.join(DB_FILE_NAME))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // Migrations are forward-only and run here, in one transaction, so
        // a database is either at the old version or fully at the new one.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => {
                // A new database. Its mode is decided by the first start.
                tx.execute_batch(SCHEMA_V1)?;
                tx.execute_batch(MIGRATE_V1_TO_V2)?;
            }
            1 => {
                // Milestone M0 knew only local mode.
                tx.execute_batch(MIGRATE_V1_TO_V2)?;
                identity::meta_set(&tx, identity::META_MODE, Mode::Local.as_str())?;
            }
            SCHEMA_VERSION => {}
            other => {
                return Err(StoreError::Other(format!(
                    "the database has schema version {other}, but this server supports version {SCHEMA_VERSION}"
                )));
            }
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            conn: Mutex::new(conn),
            _lock: lock,
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock rolls back its transaction, so the
        // connection is still usable.
        self.conn
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Settles the mode this start runs in, without writing anything.
    ///
    /// A network directory stays one whatever is asked, and it is recognised
    /// by more than its record: a certificate or key file, or any principal,
    /// credential or invite, makes it one even when the record is missing or
    /// says local, so that removing a row cannot expose a network board
    /// without TLS and authentication. A local directory becomes a network
    /// one when `network` is asked for.
    pub(crate) fn plan_mode(&self, network: bool) -> Result<ModePlan> {
        let conn = self.conn();
        let recorded = match identity::meta_get(&conn, identity::META_MODE)? {
            None => None,
            Some(text) => Some(Mode::parse(&text).ok_or_else(|| {
                StoreError::Other("the database records an unknown mode".to_owned())
            })?),
        };
        let evidence = tls_files_exist(&self.data_dir) || has_identities(&conn)?;
        let (mode, change) = match (recorded, evidence, network) {
            (Some(Mode::Network), _, _) => (Mode::Network, ModeChange::Keep),
            (_, true, _) => (Mode::Network, ModeChange::Repair),
            (Some(Mode::Local), false, true) => (Mode::Network, ModeChange::Upgrade),
            (Some(Mode::Local), false, false) => (Mode::Local, ModeChange::Keep),
            (None, false, true) => (Mode::Network, ModeChange::First),
            (None, false, false) => (Mode::Local, ModeChange::First),
        };
        let pinned = identity::meta_get(&conn, identity::META_FINGERPRINT)?.is_some();
        Ok(ModePlan {
            mode,
            change,
            pinned,
        })
    }

    /// Commits what [`Store::plan_mode`] settled and records that a server
    /// started, and where a network board can be reached, so the operator
    /// functions can build invites. This is the one place a directory's mode
    /// is written, and it runs only once the start can no longer fail for
    /// another reason, such as a port that is taken.
    pub(crate) fn record_start(
        &self,
        plan: &ModePlan,
        endpoint: Option<(&str, &str)>,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if plan.change != ModeChange::Keep {
            identity::meta_set(&tx, identity::META_MODE, plan.mode.as_str())?;
        }
        let change = match plan.change {
            ModeChange::Keep | ModeChange::First => None,
            ModeChange::Upgrade => Some((log_kinds::MODE_UPGRADED, "local to network")),
            ModeChange::Repair => Some((
                log_kinds::MODE_REPAIRED,
                "the mode record was missing or said local",
            )),
        };
        if let Some((kind, detail)) = change {
            identity::log(
                &tx,
                &LogEntry {
                    kind,
                    detail: Some(detail),
                    ..LogEntry::default()
                },
            )?;
        }
        if let Some((url, fingerprint)) = endpoint {
            identity::meta_set(&tx, identity::META_URL, url)?;
            identity::meta_set(&tx, identity::META_FINGERPRINT, fingerprint)?;
        }
        identity::log(
            &tx,
            &LogEntry {
                kind: log_kinds::SERVER_STARTED,
                detail: Some(&format!("mode={}", plan.mode.as_str())),
                ..LogEntry::default()
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Removes security log entries beyond what `retention` keeps. Returns
    /// how many were removed.
    pub(crate) fn prune_log(&self, retention: &LogRetention) -> Result<u64> {
        let max_age_ms = i64::try_from(retention.max_age.as_millis()).unwrap_or(i64::MAX);
        let cutoff = rfc3339_from_ms(now_ms().saturating_sub(max_age_ms).max(0));
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = identity::prune_log(&tx, &cutoff, retention.max_entries)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Resolves a token digest; see [`identity::authenticate`].
    pub(crate) fn authenticate(
        &self,
        token_hash: &str,
        last_used_interval: Duration,
    ) -> Result<AuthOutcome> {
        let interval_ms = i64::try_from(last_used_interval.as_millis()).unwrap_or(i64::MAX);
        Ok(identity::authenticate(
            &self.conn(),
            token_hash,
            now_ms(),
            interval_ms,
        )?)
    }

    /// Whether the credential may still be used, read from the database.
    pub(crate) fn credential_usable(&self, credential_id: &str) -> Result<bool> {
        Ok(identity::credential_usable(
            &self.conn(),
            credential_id,
            now_ms(),
        )?)
    }

    /// Swaps an enrolment code for a token. A success is written to the
    /// security log in the same transaction. A failure returns `None` and
    /// is left to the caller to log, within the log's budget for failures.
    pub(crate) fn join(
        &self,
        code: &str,
        client: Option<IpAddr>,
    ) -> Result<Option<(String, Credential)>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let joined = identity::join(&tx, code, now_ms()).map_err(StoreError::Other)?;
        if let Some((_, credential)) = &joined {
            identity::log(
                &tx,
                &LogEntry {
                    kind: log_kinds::ENROL_SUCCEEDED,
                    principal: Some(&credential.handle),
                    credential_id: Some(&credential.id),
                    client,
                    detail: None,
                },
            )?;
        }
        tx.commit()?;
        Ok(joined)
    }

    /// Revokes the caller's own credential.
    pub(crate) fn logout(&self, credential: &Credential, client: Option<IpAddr>) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        identity::revoke_credential(&tx, &credential.id, now_ms())?;
        identity::log(
            &tx,
            &LogEntry {
                kind: log_kinds::LOGOUT,
                principal: Some(&credential.handle),
                credential_id: Some(&credential.id),
                client,
                detail: None,
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Appends one security log entry.
    pub(crate) fn log(&self, entry: &LogEntry<'_>) -> Result<()> {
        Ok(identity::log(&self.conn(), entry)?)
    }

    pub(crate) fn find_board(&self, name: &str) -> Result<Option<BoardRef>> {
        find_board(&self.conn(), name)
    }

    /// Creates the board unless a board with this name, compared without
    /// regard to case, already exists. Returns the board either way.
    ///
    /// Only server start-up calls this, for the default board. Every other
    /// board is created by [`Store::post`].
    pub(crate) fn ensure_board(&self, name: &str) -> Result<BoardRef> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let board = ensure_board(&tx, name)?;
        tx.commit()?;
        Ok(board)
    }

    pub(crate) fn channel_exists(&self, board_id: i64, path: &str) -> Result<bool> {
        Ok(self.conn().query_row(
            "SELECT EXISTS (SELECT 1 FROM channels WHERE board_id = ?1 AND path = ?2)",
            params![board_id, path],
            |row| row.get(0),
        )?)
    }

    /// The root ID and channel of the thread that holds `message_id`.
    pub(crate) fn thread_of(
        &self,
        board_id: i64,
        message_id: &str,
    ) -> Result<Option<(String, String)>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT thread_id, channel FROM messages WHERE board_id = ?1 AND id = ?2",
                params![board_id, message_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// Stores a message and appends its event in one transaction. A
    /// [`PostTarget::NewBoard`] is created in that same transaction, so a
    /// board never exists without a message unless it is the default one.
    pub(crate) fn post(&self, target: PostTarget, post: NewPost) -> Result<Posted> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let board = match target {
            PostTarget::Board(board) => board,
            PostTarget::NewBoard(name) => ensure_board(&tx, &name)?,
        };
        let board = &board;

        if let Some((key, fingerprint)) = &post.idempotency {
            let earlier: Option<(String, String)> = tx
                .query_row(
                    "SELECT fingerprint, message_id FROM idempotency
                     WHERE board_id = ?1 AND actor_id = ?2 AND key = ?3",
                    params![board.id, post.actor.id, key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((earlier_fingerprint, message_id)) = earlier {
                if &earlier_fingerprint != fingerprint {
                    return Err(StoreError::IdempotencyConflict);
                }
                let (message, seq) = tx.query_row(
                    &format!(
                        "SELECT {MESSAGE_COLUMNS}, m.seq FROM messages m
                         JOIN actors a ON a.id = m.actor_id WHERE m.id = ?1"
                    ),
                    [&message_id],
                    |row| {
                        Ok((
                            message_from_row(row, 0, &board.name)?,
                            row.get(MESSAGE_COLUMN_COUNT)?,
                        ))
                    },
                )?;
                return Ok(Posted {
                    board: board.clone(),
                    message,
                    seq,
                    duplicate: true,
                });
            }
        }

        let (head_seq, head_hash): (i64, String) = tx.query_row(
            "SELECT head_seq, head_hash FROM boards WHERE id = ?1",
            [board.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let seq = head_seq + 1;
        let now = now_rfc3339();
        let id = format!("msg_{}", random_hex(&tx, 12)?);
        let is_root = post.thread_id.is_none();
        let thread_id = post.thread_id.unwrap_or_else(|| id.clone());
        let actor = post.actor;
        let kind = actor_kind_name(actor.kind);

        tx.execute(
            "INSERT OR IGNORE INTO actors (id, kind, principal, profile, handle)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                actor.id,
                kind,
                post.principal_id,
                actor.profile,
                actor.handle
            ],
        )?;
        tx.execute(
            "INSERT INTO messages (id, board_id, channel, thread_id, actor_id, text, created_at, seq, credential_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                board.id,
                post.channel,
                thread_id,
                actor.id,
                post.text,
                now,
                seq,
                post.credential_id
            ],
        )?;
        tx.execute(
            "INSERT INTO channels (board_id, path, created_at, thread_count, last_activity_at)
             VALUES (?1, ?2, ?3, ?4, ?3)
             ON CONFLICT (board_id, path) DO UPDATE SET
                 thread_count = thread_count + excluded.thread_count,
                 last_activity_at = excluded.last_activity_at",
            params![board.id, post.channel, now, i64::from(is_root)],
        )?;
        if is_root {
            tx.execute(
                "INSERT INTO threads (root_id, board_id, channel, last_seq, last_activity_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, board.id, post.channel, seq, now],
            )?;
        } else {
            let updated = tx.execute(
                "UPDATE threads SET reply_count = reply_count + 1, last_seq = ?1, last_activity_at = ?2
                 WHERE root_id = ?3 AND board_id = ?4",
                params![seq, now, thread_id, board.id],
            )?;
            if updated != 1 {
                return Err(StoreError::ThreadMissing);
            }
        }

        let hash = event_hash(
            HASH_VERSION,
            &EventFields {
                prev_hash: &head_hash,
                board: &board.name,
                seq,
                kind: event_types::MESSAGE_POSTED,
                channel: &post.channel,
                thread_id: &thread_id,
                message_id: &id,
                actor_id: &actor.id,
                actor_kind: kind,
                actor_handle: &actor.handle,
                actor_profile: actor.profile.as_deref(),
                at: &now,
                credential_id: post.credential_id.as_deref(),
            },
        )
        .ok_or_else(|| StoreError::Other("the current hash encoding is unknown".to_owned()))?;
        tx.execute(
            "INSERT INTO events (board_id, seq, kind, channel, thread_id, message_id, actor_id, at, prev_hash, hash, credential_id, hash_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                board.id,
                seq,
                event_types::MESSAGE_POSTED,
                post.channel,
                thread_id,
                id,
                actor.id,
                now,
                head_hash,
                hash,
                post.credential_id,
                HASH_VERSION
            ],
        )?;
        tx.execute(
            "UPDATE boards SET head_seq = ?1, head_hash = ?2 WHERE id = ?3",
            params![seq, hash, board.id],
        )?;
        if let Some((key, fingerprint)) = &post.idempotency {
            tx.execute(
                "INSERT INTO idempotency (board_id, actor_id, key, fingerprint, message_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![board.id, actor.id, key, fingerprint, id],
            )?;
        }
        tx.commit()?;

        Ok(Posted {
            board: board.clone(),
            message: Message {
                id,
                board: board.name.clone(),
                channel: post.channel,
                thread_id,
                by: actor,
                text: post.text,
                created_at: now,
                revision: 1,
                credential_id: post.credential_id,
            },
            seq,
            duplicate: false,
        })
    }

    pub(crate) fn channels(&self, board_id: i64) -> Result<Vec<ChannelInfo>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT path, thread_count, last_activity_at FROM channels
             WHERE board_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map([board_id], |row| {
            Ok(ChannelInfo {
                path: row.get(0)?,
                thread_count: row.get::<_, i64>(1)?.max(0) as u64,
                last_activity_at: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Thread summaries, newest activity first, strictly older than
    /// `before` when it is given.
    pub(crate) fn threads(
        &self,
        board: &BoardRef,
        channel: &ChannelSel,
        before: Option<i64>,
        limit: u32,
    ) -> Result<ThreadPage> {
        let mut values = vec![Value::Integer(board.id)];
        let mut sql = format!(
            "SELECT t.last_seq, t.reply_count, t.last_activity_at, {MESSAGE_COLUMNS}
             FROM threads t
             JOIN messages m ON m.id = t.root_id
             JOIN actors a ON a.id = m.actor_id
             WHERE t.board_id = ? AND {}",
            channel_clause("t.channel", channel, &mut values)
        );
        if let Some(before) = before {
            sql.push_str(" AND t.last_seq < ?");
            values.push(Value::Integer(before));
        }
        sql.push_str(" ORDER BY t.last_seq DESC LIMIT ?");
        values.push(Value::Integer(i64::from(limit) + 1));

        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                ThreadSummary {
                    reply_count: row.get::<_, i64>(1)?.max(0) as u64,
                    last_activity_at: row.get(2)?,
                    root: message_from_row(row, 3, &board.name)?,
                },
            ))
        })?;
        let mut threads = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let more = threads.len() > limit as usize;
        threads.truncate(limit as usize);
        Ok(ThreadPage { threads, more })
    }

    /// The thread rooted at `root_id`: the root and its replies, oldest first.
    pub(crate) fn thread(&self, board: &BoardRef, root_id: &str) -> Result<Option<Thread>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages m JOIN actors a ON a.id = m.actor_id
             WHERE m.board_id = ?1 AND m.thread_id = ?2 ORDER BY m.seq"
        ))?;
        let rows = stmt.query_map(params![board.id, root_id], |row| {
            message_from_row(row, 0, &board.name)
        })?;
        let mut messages = rows.collect::<rusqlite::Result<Vec<Message>>>()?;
        if messages.first().is_none_or(|first| first.id != root_id) {
            return Ok(None);
        }
        let root = messages.remove(0);
        Ok(Some(Thread {
            root,
            replies: messages,
        }))
    }

    /// Position of the newest event in the board's log, or 0 when empty.
    pub(crate) fn head(&self, board_id: i64) -> Result<i64> {
        head(&self.conn(), board_id)
    }

    /// Events after position `after` that pass `filter`, oldest first.
    pub(crate) fn events_after(
        &self,
        board: &BoardRef,
        after: i64,
        filter: &EventFilter,
        limit: u32,
    ) -> Result<EventPage> {
        let mut values = vec![Value::Integer(board.id), Value::Integer(after)];
        let mut sql = String::from(
            "SELECT e.seq, e.kind, e.channel, e.thread_id, e.message_id, e.at, e.prev_hash, e.hash,
                    a.id, a.kind, a.handle, a.profile, e.credential_id
             FROM events e JOIN actors a ON a.id = e.actor_id
             WHERE e.board_id = ? AND e.seq > ?",
        );
        if let Some(channel) = &filter.channel {
            sql.push_str(" AND ");
            sql.push_str(&channel_clause("e.channel", channel, &mut values));
        }
        if let Some(thread) = &filter.thread {
            sql.push_str(" AND e.thread_id = ?");
            values.push(Value::Text(thread.clone()));
        }
        if let Some(handle) = &filter.not_from {
            sql.push_str(" AND a.handle <> ?");
            values.push(Value::Text(handle.clone()));
        }
        if let Some(actor_id) = &filter.not_from_actor {
            sql.push_str(" AND e.actor_id <> ?");
            values.push(Value::Text(actor_id.clone()));
        }
        sql.push_str(" ORDER BY e.seq LIMIT ?");
        values.push(Value::Integer(i64::from(limit)));

        // The head and the page are read in one read transaction, so both
        // come from the same snapshot of the log and `scanned_to` is exact
        // whatever is committed meanwhile.
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let head = head(&tx, board.id)?;
        let mut stmt = tx.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), |row| {
            let seq: i64 = row.get(0)?;
            Ok((
                seq,
                Event {
                    cursor: event_cursor(&board.tag, seq),
                    kind: row.get(1)?,
                    board: board.name.clone(),
                    channel: row.get(2)?,
                    thread_id: row.get(3)?,
                    message_id: row.get(4)?,
                    at: row.get(5)?,
                    prev_hash: row.get(6)?,
                    hash: row.get(7)?,
                    by: actor_from_row(row, 8)?,
                    credential_id: row.get(12)?,
                },
            ))
        })?;
        let events = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let more = events.len() >= limit as usize;
        let scanned_to = match events.last() {
            Some((last, _)) if more => *last,
            _ => head.max(after),
        };
        Ok(EventPage {
            events,
            scanned_to,
            more,
        })
    }
}

/// The mode an existing data directory is in, read without locking,
/// creating or changing anything. `None` when nothing there says: no
/// certificate and no readable board database. A database from milestone M0
/// is a local one. Like [`Store::plan_mode`], it takes a directory with a
/// certificate, a key or any agent for a network one whatever its record
/// says.
pub(crate) fn peek_mode(data_dir: &Path) -> Option<Mode> {
    if tls_files_exist(data_dir) {
        return Some(Mode::Network);
    }
    let path = data_dir.join(DB_FILE_NAME);
    if !path.is_file() {
        return None;
    }
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.busy_timeout(Duration::from_secs(5)).ok()?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .ok()?;
    match version {
        1 => Some(Mode::Local),
        SCHEMA_VERSION => mode_of(&conn, data_dir).ok()?,
        _ => None,
    }
}

/// The mode of a data directory whose database, at the current schema, is
/// open on `conn`: the recorded one, except that a certificate, a key or
/// any agent makes it a network one. `None` when nothing says.
pub(crate) fn mode_of(conn: &Connection, data_dir: &Path) -> rusqlite::Result<Option<Mode>> {
    let recorded =
        identity::meta_get(conn, identity::META_MODE)?.and_then(|text| Mode::parse(&text));
    if recorded != Some(Mode::Network) && (tls_files_exist(data_dir) || has_identities(conn)?) {
        return Ok(Some(Mode::Network));
    }
    Ok(recorded)
}

/// Whether the directory holds a certificate or a key, in any state.
fn tls_files_exist(data_dir: &Path) -> bool {
    [CERT_FILE_NAME, KEY_FILE_NAME]
        .iter()
        .any(|name| std::fs::symlink_metadata(data_dir.join(name)).is_ok())
}

/// Whether the board has ever had a principal, a credential or an invite,
/// none of which a local board can have.
fn has_identities(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM principals) OR EXISTS (SELECT 1 FROM credentials)
             OR EXISTS (SELECT 1 FROM invites)",
        [],
        |row| row.get(0),
    )
}

/// The opaque cursor for position `seq` in the log of the board with `tag`.
pub(crate) fn event_cursor(tag: &str, seq: i64) -> String {
    format!("ev_{tag}_{seq:x}")
}

/// The opaque cursor that continues a thread listing below activity
/// position `seq`.
pub(crate) fn thread_cursor(tag: &str, seq: i64) -> String {
    format!("th_{tag}_{seq:x}")
}

/// Recovers the position from a cursor issued by the board with `tag`.
/// Returns `None` for anything this board could not have issued.
pub(crate) fn parse_cursor(prefix: &str, tag: &str, cursor: &str) -> Option<i64> {
    let digits = cursor
        .strip_prefix(prefix)?
        .strip_prefix('_')?
        .strip_prefix(tag)?
        .strip_prefix('_')?;
    let seq = i64::from_str_radix(digits, 16).ok()?;
    // Reject alternate spellings such as a sign or leading zeros.
    (seq >= 0 && format!("{seq:x}") == digits).then_some(seq)
}

fn find_board(conn: &Connection, name: &str) -> Result<Option<BoardRef>> {
    Ok(conn
        .query_row(
            "SELECT id, name, tag FROM boards WHERE name_key = ?1",
            [board_key(name)],
            |row| {
                Ok(BoardRef {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    tag: row.get(2)?,
                })
            },
        )
        .optional()?)
}

/// Inserts the board unless one with this name, compared without regard to
/// case, already exists. Returns the board either way.
fn ensure_board(conn: &Connection, name: &str) -> Result<BoardRef> {
    let tag = random_hex(conn, 8)?;
    conn.execute(
        "INSERT OR IGNORE INTO boards (name, name_key, tag, created_at, head_hash)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![name, board_key(name), tag, now_rfc3339(), GENESIS_HASH],
    )?;
    find_board(conn, name)?
        .ok_or_else(|| StoreError::Other("board vanished after creation".to_owned()))
}

fn head(conn: &Connection, board_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT head_seq FROM boards WHERE id = ?1",
        [board_id],
        |row| row.get(0),
    )?)
}

/// A SQL condition on `column` for a channel or a subtree. `LIKE` is avoided
/// because `_` is both a wildcard and a legal channel character.
fn channel_clause(column: &str, channel: &ChannelSel, values: &mut Vec<Value>) -> String {
    match channel {
        ChannelSel::Exact(path) => {
            values.push(Value::Text(path.clone()));
            format!("{column} = ?")
        }
        ChannelSel::Subtree(path) => {
            let prefix = format!("{path}/");
            values.push(Value::Text(path.clone()));
            values.push(Value::Integer(prefix.len() as i64));
            values.push(Value::Text(prefix));
            format!("({column} = ? OR substr({column}, 1, ?) = ?)")
        }
    }
}

fn random_hex(conn: &Connection, bytes: u8) -> Result<String> {
    Ok(
        conn.query_row("SELECT lower(hex(randomblob(?1)))", [bytes], |row| {
            row.get(0)
        })?,
    )
}

fn message_from_row(row: &Row<'_>, at: usize, board: &str) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(at)?,
        board: board.to_owned(),
        channel: row.get(at + 1)?,
        thread_id: row.get(at + 2)?,
        text: row.get(at + 3)?,
        created_at: row.get(at + 4)?,
        revision: row.get(at + 5)?,
        by: actor_from_row(row, at + 6)?,
        credential_id: row.get(at + 10)?,
    })
}

fn actor_from_row(row: &Row<'_>, at: usize) -> rusqlite::Result<Actor> {
    let kind: String = row.get(at + 1)?;
    let kind = parse_actor_kind(&kind).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            at + 1,
            rusqlite::types::Type::Text,
            "unknown actor kind".into(),
        )
    })?;
    Ok(Actor {
        id: row.get(at)?,
        kind,
        handle: row.get(at + 2)?,
        profile: row.get(at + 3)?,
    })
}

/// Creates the data directory, readable only by its owner where the
/// platform supports that. A directory that already exists is left as it
/// is here; [`make_private`] deals with it.
pub(crate) fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Makes the data directory and the files that hold the board private to
/// their owner, whoever created them and under whatever umask: the
/// directory becomes 0700, and the database, its `-wal` and `-shm` side
/// files and the lock file 0600. The database file is created here if it is
/// missing, because SQLite would create it with the process umask, and
/// SQLite gives the side files the mode of the database file.
///
/// A database path that is a symbolic link is refused: the board would
/// otherwise write, and change the mode of, a file outside its directory.
/// The data directory itself may be a symbolic link, which is a common way
/// to keep it on another disk.
///
/// Does nothing where the platform has no such modes.
fn make_private(data_dir: &Path) -> std::result::Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // Removes group and other access. Returns whether there was any.
        let restrict = |path: &Path, mode: u32| -> std::result::Result<bool, String> {
            let failed =
                |error: std::io::Error| format!("cannot restrict {}: {error}", path.display());
            let current = std::fs::metadata(path)
                .map_err(failed)?
                .permissions()
                .mode();
            if current & 0o077 == 0 {
                return Ok(false);
            }
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .map_err(failed)?;
            Ok(true)
        };
        let mut loose = restrict(data_dir, 0o700)?;

        let database = data_dir.join(DB_FILE_NAME);
        let is_link = |path: &Path| {
            std::fs::symlink_metadata(path).is_ok_and(|found| found.file_type().is_symlink())
        };
        if is_link(&database) {
            return Err(format!(
                "{} is a symbolic link; the board database must be a file in the data directory",
                database.display()
            ));
        }
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&database)
            .map_err(|error| format!("cannot create {}: {error}", database.display()))?;
        for name in [
            DB_FILE_NAME.to_owned(),
            format!("{DB_FILE_NAME}-wal"),
            format!("{DB_FILE_NAME}-shm"),
            LOCK_FILE_NAME.to_owned(),
        ] {
            let path = data_dir.join(name);
            if path.is_file() && !is_link(&path) {
                loose |= restrict(&path, 0o600)?;
            }
        }
        if loose {
            eprintln!(
                "openboard-server: {} or the database in it was accessible to other users; \
                 restricting both to their owner",
                data_dir.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = data_dir;
    Ok(())
}

/// Takes the exclusive lock that marks the data directory as served. The
/// lock lasts until the returned file is closed, and the system drops it if
/// the process dies.
fn lock_data_dir(data_dir: &Path) -> Result<File> {
    let path = data_dir.join(LOCK_FILE_NAME);
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let failed = |error: std::io::Error| {
        StoreError::Other(format!(
            "cannot lock the data directory with {}: {error}",
            path.display()
        ))
    };
    let file = options.open(&path).map_err(failed)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(StoreError::DataDirInUse),
        Err(TryLockError::Error(error)) => Err(failed(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_are_board_scoped() {
        let cursor = event_cursor("aabb", 255);
        assert_eq!(parse_cursor("ev", "aabb", &cursor), Some(255));
        assert_eq!(parse_cursor("ev", "ccdd", &cursor), None);
        assert_eq!(parse_cursor("th", "aabb", &cursor), None);
        for garbage in [
            "",
            "ev_aabb_",
            "ev_aabb_-1",
            "ev_aabb_0ff",
            "ev_aabb_+f",
            "ev_aabb_zz",
            "12",
        ] {
            assert_eq!(parse_cursor("ev", "aabb", garbage), None, "{garbage}");
        }
    }
}
