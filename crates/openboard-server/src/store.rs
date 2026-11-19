//! SQLite storage. Everything here blocks, so callers run it on a blocking
//! thread. Every mutation is one transaction.

use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use openboard_api::{
    event_types, Actor, ActorKind, ChannelInfo, Event, Message, Thread, ThreadSummary,
};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row, TransactionBehavior};

use crate::chain::{event_hash, EventFields, GENESIS_HASH};
use crate::names::{board_key, ChannelSel};
use crate::time::now_rfc3339;

/// File name of the board database inside the data directory.
pub const DB_FILE_NAME: &str = "openboard.db";
/// File name of the lock a running server holds on the data directory.
pub const LOCK_FILE_NAME: &str = "openboard.lock";

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
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

const MESSAGE_COLUMNS: &str =
    "m.id, m.channel, m.thread_id, m.text, m.created_at, m.revision, a.id, a.kind, a.handle, a.profile";

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

pub(crate) struct Store {
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
        let mut conn = Connection::open(data_dir.join(DB_FILE_NAME))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => {
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            other => {
                return Err(StoreError::Other(format!(
                    "the database has schema version {other}, but this server supports version {SCHEMA_VERSION}"
                )));
            }
        }
        tx.commit()?;
        Ok(Self {
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
                    |row| Ok((message_from_row(row, 0, &board.name)?, row.get(10)?)),
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
            "INSERT INTO messages (id, board_id, channel, thread_id, actor_id, text, created_at, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, board.id, post.channel, thread_id, actor.id, post.text, now, seq],
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

        let hash = event_hash(&EventFields {
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
        });
        tx.execute(
            "INSERT INTO events (board_id, seq, kind, channel, thread_id, message_id, actor_id, at, prev_hash, hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                hash
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
                    a.id, a.kind, a.handle, a.profile
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

fn actor_kind_name(kind: ActorKind) -> &'static str {
    match kind {
        ActorKind::Local => "local",
        ActorKind::Person => "person",
        ActorKind::Service => "service",
        ActorKind::Agent => "agent",
        ActorKind::Bridge => "bridge",
    }
}

fn parse_actor_kind(name: &str) -> Option<ActorKind> {
    Some(match name {
        "local" => ActorKind::Local,
        "person" => ActorKind::Person,
        "service" => ActorKind::Service,
        "agent" => ActorKind::Agent,
        "bridge" => ActorKind::Bridge,
        _ => return None,
    })
}

/// Creates the data directory, readable only by its owner where the
/// platform supports that.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
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
