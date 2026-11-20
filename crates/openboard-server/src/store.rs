//! SQLite storage. Everything here blocks, so callers run it on a blocking
//! thread. Every mutation is one transaction.

use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use openboard_api::{
    event_types, Actor, BoardInfo, ChannelInfo, Event, LabelsRequest, Mention, Message, Reaction,
    Thread, ThreadSummary, MAX_REACTION_NAMES_PER_MESSAGE,
};
use rusqlite::types::Value;
use rusqlite::{
    params, params_from_iter, Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior,
};

use crate::chain::{event_hash, EventFields, GENESIS_HASH, HASH_VERSION};
use crate::identity::{
    self, actor_kind_name, log_kinds, parse_actor_kind, AuthOutcome, Credential, InviteKind,
    LogEntry,
};
use crate::limits::LogRetention;
use crate::markdown::Candidate;
use crate::names::{board_key, handle_key, ChannelSel};
use crate::time::{now_ms, now_rfc3339, rfc3339_from_ms};
use crate::tls::{CERT_FILE_NAME, KEY_FILE_NAME};
use crate::Mode;

pub mod grants;
mod tasks;

pub(crate) use grants::{Access, Snapshot};
pub(crate) use tasks::{
    Act, AssigneeSpec, BatchEntry, Brief, ClaimOut, ClaimProof, CreatorSel, Need, NewTask,
    TaskError, TaskQuery,
};

/// File name of the board database inside the data directory.
pub const DB_FILE_NAME: &str = "openboard.db";
/// File name of the lock a running server holds on the data directory.
pub const LOCK_FILE_NAME: &str = "openboard.lock";

pub(crate) const SCHEMA_VERSION: i64 = 5;
/// The oldest schema that has the identity tables the operator functions
/// use. A database between this and [`SCHEMA_VERSION`] can be read and
/// written by them before a newer server has migrated it.
pub(crate) const OLDEST_IDENTITY_SCHEMA: i64 = 2;

/// Most labels one message holds.
pub(crate) const MAX_LABELS_PER_MESSAGE: usize = 32;
/// Most mentions kept for one message. Candidates beyond it stay plain text.
pub(crate) const MAX_MENTIONS_PER_MESSAGE: usize = 50;
/// How long a statement waits for a lock another process holds. The scrub
/// of the write-ahead log is the exception: it never waits; see
/// [`truncate_wal`].
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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

/// From `user_version` 2 to 3: edits, tombstones, labels, reactions and
/// mentions. The event log gains new types and no new columns, so events
/// keep hash encoding 2.
const MIGRATE_V2_TO_V3: &str = "
ALTER TABLE messages ADD COLUMN edited_at TEXT;
ALTER TABLE messages ADD COLUMN deleted_at TEXT;
CREATE TABLE labels (
    message_id TEXT NOT NULL REFERENCES messages(id),
    thread_id  TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    PRIMARY KEY (message_id, key)
) WITHOUT ROWID;
CREATE INDEX labels_by_thread ON labels(thread_id, key);
CREATE TABLE reactions (
    id           INTEGER PRIMARY KEY,
    message_id   TEXT NOT NULL REFERENCES messages(id),
    name         TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    handle       TEXT NOT NULL,
    UNIQUE (message_id, name, principal_id)
);
CREATE TABLE mentions (
    message_id   TEXT NOT NULL REFERENCES messages(id),
    position     INTEGER NOT NULL,
    principal_id TEXT NOT NULL,
    handle       TEXT NOT NULL,
    kind         TEXT NOT NULL,
    profile      TEXT,
    PRIMARY KEY (message_id, position)
) WITHOUT ROWID;
";

/// From `user_version` 3 to 4: tasks, their prerequisites, and the
/// idempotency keys of task creation. The event log gains the `task.*`
/// types and no new columns, so events keep hash encoding 2.
const MIGRATE_V3_TO_V4: &str = "
CREATE TABLE tasks (
    id                 TEXT PRIMARY KEY REFERENCES messages(id),
    board_id           INTEGER NOT NULL REFERENCES boards(id),
    channel            TEXT NOT NULL,
    state              TEXT NOT NULL,
    creator_actor        TEXT NOT NULL REFERENCES actors(id),
    creator_principal    TEXT NOT NULL,
    assignee_principal TEXT,
    assignee_profile   TEXT,
    assignee_handle    TEXT,
    epoch              INTEGER NOT NULL DEFAULT 0,
    lease_token        TEXT,
    holder_actor       TEXT REFERENCES actors(id),
    lease_until_ms     INTEGER,
    lease_ms           INTEGER,
    block_kind         TEXT,
    reason             TEXT,
    result             TEXT,
    changed_actor      TEXT NOT NULL REFERENCES actors(id),
    changed_at         TEXT NOT NULL,
    brief_revision     INTEGER NOT NULL,
    created_seq        INTEGER NOT NULL
);
CREATE INDEX tasks_by_state ON tasks(board_id, state, created_seq);
CREATE INDEX tasks_by_created ON tasks(board_id, created_seq);
CREATE INDEX tasks_by_lease ON tasks(state, lease_until_ms);
CREATE TABLE task_needs (
    task_id  TEXT NOT NULL REFERENCES tasks(id),
    needs_id TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (task_id, needs_id)
) WITHOUT ROWID;
CREATE INDEX task_needs_by_need ON task_needs(needs_id);
CREATE TABLE task_keys (
    board_id    INTEGER NOT NULL REFERENCES boards(id),
    actor_id    TEXT NOT NULL,
    key         TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    task_ids    TEXT NOT NULL,
    PRIMARY KEY (board_id, actor_id, key)
) WITHOUT ROWID;
";

/// The detail of the logout entry written for a session that a new sign-in
/// of the same person, from the same browser, took the place of.
pub(crate) const SESSION_REPLACED: &str = "session, replaced by a new sign-in";

const MESSAGE_COLUMNS: &str = "m.id, m.channel, m.thread_id, m.text, m.created_at, m.revision, \
                               a.id, a.kind, a.handle, a.profile, m.credential_id, \
                               m.edited_at, m.deleted_at";
/// Number of columns in [`MESSAGE_COLUMNS`].
const MESSAGE_COLUMN_COUNT: usize = 13;

#[derive(Debug)]
pub(crate) enum StoreError {
    /// The idempotency key was already used with a different request.
    IdempotencyConflict,
    /// The thread a reply names does not exist on this board.
    ThreadMissing,
    /// The message a change names does not exist on this board.
    MessageMissing,
    /// The message is a tombstone and cannot be changed.
    MessageDeleted,
    /// The message is a board notice (label `board.notice`), which nobody
    /// may edit or delete.
    BoardNotice,
    /// A conditional edit or delete named a revision that is not current.
    RevisionConflict,
    /// A conditional label write found the condition on this key false.
    /// Holds the key and never a value.
    LabelConflict(String),
    /// The change would leave the message with too many labels.
    TooManyLabels,
    /// The reaction would be one name more than a message holds.
    TooManyReactions,
    /// A task write was refused.
    Task(TaskError),
    /// A grant change or a channel creation was refused.
    Grant(grants::GrantFailure),
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
            StoreError::MessageMissing => f.write_str("message missing"),
            StoreError::MessageDeleted => f.write_str("message deleted"),
            StoreError::BoardNotice => f.write_str("board notice"),
            StoreError::RevisionConflict => f.write_str("revision conflict"),
            StoreError::LabelConflict(_) => f.write_str("label conflict"),
            StoreError::TooManyLabels => f.write_str("too many labels"),
            StoreError::TooManyReactions => f.write_str("too many reactions"),
            StoreError::Task(error) => write!(f, "task: {error:?}"),
            StoreError::Grant(error) => write!(f, "grant: {error:?}"),
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
    /// What the text addresses.
    pub mentions: MentionInput,
}

/// The mention candidates of a text, to be resolved against the board's
/// principals in the transaction that stores the text.
#[derive(Debug, Clone, Default)]
pub(crate) struct MentionInput {
    pub candidates: Vec<Candidate>,
    /// True on a local board, whose only principal is the local one and is
    /// not in the principals table.
    pub local: bool,
}

/// Who is changing a message: the actor, the principal behind it and the
/// credential it authenticated with.
#[derive(Debug, Clone)]
pub(crate) struct Writer {
    pub actor: Actor,
    pub principal_id: String,
    /// `None` on a local board.
    pub credential_id: Option<String>,
}

/// The outcome of a change to a message.
pub(crate) struct Changed {
    /// The message as it now stands.
    pub message: Message,
    /// Position of the event the change appended, or `None` when nothing
    /// changed and so nothing was appended.
    pub seq: Option<i64>,
}

/// What the handlers need to know about a message before changing it.
#[derive(Debug, Clone)]
pub(crate) struct MessageMeta {
    pub channel: String,
    /// ID of the actor that wrote the message.
    pub actor_id: String,
}

/// A label condition on a thread listing: the key, and the values that
/// match. No values means any value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LabelFilter {
    pub key: String,
    pub values: Vec<String>,
}

/// The caller, for the `mentions_self` filter.
#[derive(Debug, Clone)]
pub(crate) struct MentionedSelf {
    pub principal_id: String,
    pub profile: Option<String>,
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
    /// Only events of these types.
    pub kinds: Option<Vec<&'static str>>,
    /// Only events about a message whose stored mentions name this caller.
    pub mentions: Option<MentionedSelf>,
    /// Only events in a thread where some message carries this label now.
    pub label: Option<LabelFilter>,
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
    /// The board's event position when the page was read.
    pub head: i64,
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
    /// Whether the board is recorded as serving a supplied certificate,
    /// which its clients check against their certificate authorities.
    pub trusted: bool,
}

/// Where a network board can be reached, as a start records it.
pub(crate) struct Endpoint<'a> {
    pub url: &'a str,
    /// The pin of a self-signed certificate. `None` when the certificate
    /// was supplied.
    pub fingerprint: Option<&'a str>,
}

pub(crate) struct Store {
    data_dir: PathBuf,
    conn: Mutex<Connection>,
    /// True while removed content may still be in the write-ahead log: a
    /// scrub was due and could not empty it. Written under the connection
    /// lock; read without it. See [`Store::scrub`].
    scrub_pending: AtomicBool,
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
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Read before anything is written to the database, so that a
        // database this server will not use, or cannot back up, is left
        // exactly as it was found.
        let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if !(0..=SCHEMA_VERSION).contains(&found) {
            return Err(StoreError::Other(unsupported_schema(&conn, found)));
        }
        if found != 0 && found != SCHEMA_VERSION {
            backup_before_migration(&conn, data_dir, found).map_err(StoreError::Other)?;
        }
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Deleted and replaced content is overwritten with zeros in the
        // database file, so the text of a deleted message does not linger
        // in free space. See also `Store::scrub`.
        conn.pragma_update(None, "secure_delete", "ON")?;

        // Migrations are forward-only and run here, in one transaction, so
        // a database is either at the old version or fully at the new one.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => {
                // A new database. Its mode is decided by the first start.
                tx.execute_batch(SCHEMA_V1)?;
                tx.execute_batch(MIGRATE_V1_TO_V2)?;
                tx.execute_batch(MIGRATE_V2_TO_V3)?;
                tx.execute_batch(MIGRATE_V3_TO_V4)?;
                // No boards yet, and the access mode is decided by the
                // first start, like the mode.
                tx.execute_batch(grants::MIGRATE_V4_TO_V5)?;
            }
            1 => {
                // Milestone M0 knew only local mode.
                tx.execute_batch(MIGRATE_V1_TO_V2)?;
                identity::meta_set(&tx, identity::META_MODE, Mode::Local.as_str())?;
                tx.execute_batch(MIGRATE_V2_TO_V3)?;
                tx.execute_batch(MIGRATE_V3_TO_V4)?;
                grants::migrate_v4_to_v5(&tx)?;
            }
            // Milestone M1.
            2 => {
                tx.execute_batch(MIGRATE_V2_TO_V3)?;
                tx.execute_batch(MIGRATE_V3_TO_V4)?;
                grants::migrate_v4_to_v5(&tx)?;
            }
            // Milestones M2 and M2.5.
            3 => {
                tx.execute_batch(MIGRATE_V3_TO_V4)?;
                grants::migrate_v4_to_v5(&tx)?;
            }
            // Milestone M3: today's access is kept.
            4 => grants::migrate_v4_to_v5(&tx)?,
            SCHEMA_VERSION => {}
            other => return Err(StoreError::Other(unsupported_schema(&tx, other))),
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        // A server that was killed, or stopped while something else read
        // the database, can have left removed content in the log. It goes
        // now, or as soon as the log can be emptied.
        let scrubbed = truncate_wal(&conn, data_dir);
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            conn: Mutex::new(conn),
            scrub_pending: AtomicBool::new(!scrubbed),
            _lock: lock,
        })
    }

    /// Whether removed content may still be in the write-ahead log, because
    /// the scrub after its removal could not empty the log. Costs nothing,
    /// so it can be asked often.
    pub(crate) fn scrub_pending(&self) -> bool {
        self.scrub_pending.load(Ordering::Acquire)
    }

    /// Moves the write-ahead log into the database file and empties it, if
    /// that can be done without waiting. Returns whether the log is empty.
    ///
    /// `secure_delete` overwrites removed content in the database file, but
    /// the log still holds the pages as they were before, and SQLite would
    /// only overwrite those as later writes happen to reuse the space. So
    /// every removal of content is followed by this, under the same lock
    /// and straight after its commit: an edit, a delete, a label that is
    /// cleared or replaced, a reaction that is taken back.
    ///
    /// The log cannot be emptied while another process reads the database:
    /// a backup, or an operator command. This never waits for that, because
    /// it runs with the store's only connection held and every request
    /// would wait with it. It notes instead that a scrub is pending, and
    /// the server's upkeep task calls it again until it succeeds; see
    /// [`Store::scrub_pending`]. It also runs when the store opens and when
    /// it closes.
    pub(crate) fn scrub(&self) -> bool {
        let conn = self.conn();
        self.scrub_wal(&conn)
    }

    /// [`Store::scrub`] for a caller that holds the connection.
    fn scrub_wal(&self, conn: &Connection) -> bool {
        let emptied = truncate_wal(conn, &self.data_dir);
        self.scrub_pending.store(!emptied, Ordering::Release);
        emptied
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
        let trusted = identity::meta_get(&conn, identity::META_TRUST)?.as_deref()
            == Some(identity::TRUST_AUTHORITIES);
        Ok(ModePlan {
            mode,
            change,
            pinned,
            trusted,
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
        endpoint: Option<Endpoint<'_>>,
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
        if let Some(endpoint) = endpoint {
            identity::meta_set(&tx, identity::META_URL, endpoint.url)?;
            match endpoint.fingerprint {
                Some(fingerprint) => {
                    identity::meta_set(&tx, identity::META_FINGERPRINT, fingerprint)?;
                }
                // A supplied certificate. No fingerprint is kept, so that
                // no invite can carry one: it would be wrong after the
                // next renewal.
                None => {
                    identity::meta_delete(&tx, identity::META_FINGERPRINT)?;
                    if !plan.trusted {
                        identity::meta_set(&tx, identity::META_TRUST, identity::TRUST_AUTHORITIES)?;
                        identity::log(
                            &tx,
                            &LogEntry {
                                kind: log_kinds::CERTIFICATE_SUPPLIED,
                                detail: Some(if plan.pinned {
                                    "replaces a self-signed certificate that clients pinned"
                                } else {
                                    "clients check it against their certificate authorities"
                                }),
                                ..LogEntry::default()
                            },
                        )?;
                    }
                }
            }
        }
        // Which build served the directory last, for a later version that
        // has to explain itself, or an earlier one that cannot open it.
        identity::meta_set(
            &tx,
            identity::META_STARTED_VERSION,
            openboard_api::build::VERSION,
        )?;
        identity::meta_set(
            &tx,
            identity::META_STARTED_COMMIT,
            openboard_api::build::COMMIT,
        )?;
        identity::meta_set(&tx, identity::META_STARTED_AT, &now_rfc3339())?;
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
        self.redeem(
            code,
            InviteKind::Agent,
            log_kinds::ENROL_SUCCEEDED,
            client,
            None,
        )
    }

    /// Swaps a person's sign-in code for a browser session. Returns the
    /// cookie value, which exists nowhere else, and the session's
    /// credential. Logged like [`Store::join`].
    ///
    /// `replaces` is the digest of the session cookie the sign-in request
    /// carried, if it carried one. When that is a usable session of the
    /// same person, the new session takes its place: the earlier one is
    /// revoked in the same transaction and logged as a logout. A session of
    /// anyone else, or one that is unknown, expired or revoked, is left
    /// exactly as it is, and so is everything when the code is refused.
    pub(crate) fn web_login(
        &self,
        code: &str,
        client: Option<IpAddr>,
        replaces: Option<&str>,
    ) -> Result<Option<(String, Credential)>> {
        self.redeem(
            code,
            InviteKind::Person,
            log_kinds::WEB_LOGIN_SUCCEEDED,
            client,
            replaces,
        )
    }

    fn redeem(
        &self,
        code: &str,
        kind: InviteKind,
        succeeded: &str,
        client: Option<IpAddr>,
        replaces: Option<&str>,
    ) -> Result<Option<(String, Credential)>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let redeemed = identity::redeem(&tx, code, kind, now).map_err(StoreError::Other)?;
        if let Some((_, credential)) = &redeemed {
            identity::log(
                &tx,
                &LogEntry {
                    kind: succeeded,
                    principal: Some(&credential.handle),
                    credential_id: Some(&credential.id),
                    client,
                    detail: None,
                },
            )?;
            let earlier = match replaces {
                None => None,
                Some(hash) => identity::find_credential_by_hash(&tx, hash)?,
            }
            .filter(|earlier| {
                earlier.principal_id == credential.principal_id
                    && earlier.id != credential.id
                    && earlier.usable_at(now)
            });
            if let Some(earlier) = earlier {
                identity::revoke_credential(&tx, &earlier.id, now)?;
                identity::log(
                    &tx,
                    &LogEntry {
                        kind: log_kinds::LOGOUT,
                        principal: Some(&earlier.handle),
                        credential_id: Some(&earlier.id),
                        client,
                        detail: Some(SESSION_REPLACED),
                    },
                )?;
            }
        }
        tx.commit()?;
        Ok(redeemed)
    }

    /// Mints a token derived from `parent`, logged in the same transaction.
    /// `None` when the chain of derived tokens is already as deep as allowed.
    pub(crate) fn derive_token(
        &self,
        parent: &Credential,
        derivation: &identity::Derivation<'_>,
        client: Option<IpAddr>,
    ) -> Result<Option<(String, Credential)>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let minted =
            identity::derive(&tx, parent, derivation, now_ms()).map_err(StoreError::Other)?;
        if let Some((_, credential)) = &minted {
            let detail = format!(
                "from {} on {}: {}",
                parent.id,
                derivation.board,
                derivation.scope.join(" ")
            );
            identity::log(
                &tx,
                &LogEntry {
                    kind: log_kinds::TOKEN_DERIVED,
                    principal: Some(&credential.handle),
                    credential_id: Some(&credential.id),
                    client,
                    detail: Some(&detail),
                },
            )?;
        }
        tx.commit()?;
        Ok(minted)
    }

    /// The live derived tokens `caller` may revoke, with board and scope.
    pub(crate) fn derived_tokens(
        &self,
        caller: &Credential,
    ) -> Result<Vec<(Credential, String, String)>> {
        Ok(identity::derived_tokens(&self.conn(), caller, now_ms())?)
    }

    /// Revokes a derived token `caller` may revoke (see
    /// [`Store::derived_tokens`]), which ends every token below it too.
    /// `None` when there is no such token for this caller.
    pub(crate) fn revoke_token(
        &self,
        caller: &Credential,
        id: &str,
        client: Option<IpAddr>,
    ) -> Result<Option<(Credential, String, String)>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let Some(found) = identity::derived_tokens(&tx, caller, now)?
            .into_iter()
            .find(|(credential, _, _)| credential.id == id)
        else {
            return Ok(None);
        };
        identity::revoke_credential(&tx, id, now)?;
        identity::log(
            &tx,
            &LogEntry {
                kind: log_kinds::CREDENTIAL_REVOKED,
                principal: Some(&found.0.handle),
                credential_id: Some(id),
                client,
                detail: Some(&format!("derived token, by {}", caller.id)),
            },
        )?;
        tx.commit()?;
        Ok(Some(found))
    }

    /// Revokes the caller's own credential: a token, or a browser session,
    /// for which `detail` says so in the security log.
    pub(crate) fn logout(
        &self,
        credential: &Credential,
        client: Option<IpAddr>,
        detail: Option<&str>,
    ) -> Result<()> {
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
                detail,
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
        let board = ensure_board(&tx, name, None)?;
        tx.commit()?;
        Ok(board)
    }

    /// Records the instance's access mode on its first start: closed for a
    /// fresh network board unless `open`, and open for a local one, whose
    /// single principal can do everything anyway, so that a later upgrade to
    /// network mode keeps what agents could do. `open` on a later start opens
    /// a closed instance; nothing here closes one.
    pub(crate) fn settle_access(&self, network: bool, open: bool) -> Result<Access> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let access = if !grants::access_recorded(&tx)? {
            let access = if open || !network {
                Access::Open
            } else {
                Access::Closed
            };
            grants::set_access(&tx, access)?;
            access
        } else if open {
            grants::set_access(&tx, Access::Open)?;
            Access::Open
        } else {
            grants::access(&tx)?
        };
        tx.commit()?;
        Ok(access)
    }

    /// Gives the principal named `handle` `all` at `*` on every board where
    /// it does not already hold that. Returns the boards it was given on.
    pub(crate) fn make_admin(&self, handle: &str) -> Result<Option<Vec<String>>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some((principal_id, _, handle)) = grants::principal_by_handle(&tx, handle)? else {
            return Ok(None);
        };
        let held = grants::list(&tx, None)?;
        let boards: Vec<(i64, String)> = tx
            .prepare("SELECT id, name FROM boards ORDER BY id")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let me = crate::grants::Grantee::Principal(principal_id);
        let mut given = Vec::new();
        for (board_id, name) in boards {
            let already = held.iter().any(|stored| {
                stored.board == name
                    && stored.grant.grantee == me
                    && stored.grant.pattern == crate::grants::Pattern::Board
                    && stored.grant.verbs == crate::grants::Verbs::all()
                    && stored.grant.expires_at.is_none()
            });
            if !already {
                grants::insert(
                    &tx,
                    board_id,
                    &crate::grants::Pattern::Board,
                    &me,
                    crate::grants::Verbs::all(),
                    None,
                    grants::OPERATOR,
                )?;
                given.push(name);
            }
        }
        let _ = handle;
        tx.commit()?;
        Ok(Some(given))
    }

    /// Whether any principal holds `grant.manage` on any board.
    pub(crate) fn anyone_manages(&self) -> Result<bool> {
        Ok(grants::anyone_manages(&self.conn())?)
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

    /// The channel and author of a message on this board.
    pub(crate) fn message_meta(
        &self,
        board_id: i64,
        message_id: &str,
    ) -> Result<Option<MessageMeta>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT channel, actor_id FROM messages WHERE board_id = ?1 AND id = ?2",
                params![board_id, message_id],
                |row| {
                    Ok(MessageMeta {
                        channel: row.get(0)?,
                        actor_id: row.get(1)?,
                    })
                },
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
            PostTarget::NewBoard(name) => {
                ensure_board(&tx, &name, Some((&post.principal_id, &post.actor.handle)))?
            }
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
                let (mut message, seq) = tx.query_row(
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
                fill_messages(&tx, &mut [&mut message])?;
                return Ok(Posted {
                    board: board.clone(),
                    message,
                    seq,
                    duplicate: true,
                });
            }
        }

        let by = Writer {
            actor: post.actor,
            principal_id: post.principal_id,
            credential_id: post.credential_id,
        };
        let inserted = insert_message(
            &tx,
            board,
            &post.channel,
            post.thread_id.as_deref(),
            &by,
            &post.text,
            &post.mentions,
        )?;
        let (id, thread_id, seq, now, mentions) = (
            inserted.id,
            inserted.thread_id,
            inserted.seq,
            inserted.now,
            inserted.mentions,
        );
        let actor = &by.actor;
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
                by: by.actor,
                text: post.text,
                created_at: now,
                revision: 1,
                credential_id: by.credential_id,
                edited_at: None,
                deleted_at: None,
                labels: Default::default(),
                reactions: Vec::new(),
                mentions: mentions.into_iter().map(|(_, mention)| mention).collect(),
                html: None,
                task: None,
            },
            seq,
            duplicate: false,
        })
    }

    /// Replaces a message's text. The earlier text is not kept: it is
    /// overwritten in the database file, and the digest of it that an
    /// idempotency key kept is removed, as [`Store::delete`] removes it.
    pub(crate) fn edit(
        &self,
        board: &BoardRef,
        message_id: &str,
        by: &Writer,
        text: &str,
        revision: Option<u32>,
        mentions: &MentionInput,
    ) -> Result<Changed> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = current_message(&tx, board, message_id)?;
        if current.deleted {
            return Err(StoreError::MessageDeleted);
        }
        refuse_notice(&tx, message_id)?;
        if revision.is_some_and(|expected| expected != current.revision) {
            return Err(StoreError::RevisionConflict);
        }
        let now = now_rfc3339();
        tx.execute(
            "UPDATE messages SET text = ?1, revision = revision + 1, edited_at = ?2 WHERE id = ?3",
            params![text, now, message_id],
        )?;
        let resolved = resolve_mentions(&tx, mentions)?;
        write_mentions(&tx, message_id, &resolved)?;
        forget_fingerprint(&tx, message_id)?;
        tasks::forget_task_fingerprint(&tx, board, message_id)?;
        upsert_actor(&tx, by)?;
        let seq = append_event(
            &tx,
            board,
            event_types::MESSAGE_EDITED,
            (&current.channel, &current.thread_id, message_id),
            by,
            &now,
        )?;
        let seq = tasks::on_brief_edited(&tx, board, message_id, by)?.unwrap_or(seq);
        let message = load_message(&tx, board, message_id)?;
        tx.commit()?;
        self.scrub_wal(&conn);
        Ok(Changed {
            message,
            seq: Some(seq),
        })
    }

    /// Turns a message into a tombstone: its text, labels, reactions and
    /// mentions are removed, and so is the digest of its text that an
    /// idempotency key kept. Deleting a tombstone changes nothing.
    pub(crate) fn delete(
        &self,
        board: &BoardRef,
        message_id: &str,
        by: &Writer,
        revision: Option<u32>,
    ) -> Result<Changed> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = current_message(&tx, board, message_id)?;
        if current.deleted {
            let message = load_message(&tx, board, message_id)?;
            return Ok(Changed { message, seq: None });
        }
        refuse_notice(&tx, message_id)?;
        if revision.is_some_and(|expected| expected != current.revision) {
            return Err(StoreError::RevisionConflict);
        }
        let now = now_rfc3339();
        tx.execute(
            "UPDATE messages SET text = '', revision = revision + 1, deleted_at = ?1 WHERE id = ?2",
            params![now, message_id],
        )?;
        for table in ["labels", "reactions", "mentions"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE message_id = ?1"),
                [message_id],
            )?;
        }
        forget_fingerprint(&tx, message_id)?;
        upsert_actor(&tx, by)?;
        let seq = append_event(
            &tx,
            board,
            event_types::MESSAGE_DELETED,
            (&current.channel, &current.thread_id, message_id),
            by,
            &now,
        )?;
        let seq = tasks::on_root_deleted(&tx, board, message_id, by)?.unwrap_or(seq);
        let message = load_message(&tx, board, message_id)?;
        tx.commit()?;
        self.scrub_wal(&conn);
        Ok(Changed {
            message,
            seq: Some(seq),
        })
    }

    /// Applies a label request: conditions first, then all of it or none.
    pub(crate) fn label(
        &self,
        board: &BoardRef,
        message_id: &str,
        by: &Writer,
        request: &LabelsRequest,
    ) -> Result<Changed> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = current_message(&tx, board, message_id)?;
        if current.deleted {
            return Err(StoreError::MessageDeleted);
        }
        let before = labels_of(&tx, message_id)?;
        for (key, value) in &request.if_equals {
            if before.get(key) != Some(value) {
                return Err(StoreError::LabelConflict(key.clone()));
            }
        }
        if let Some(key) = request
            .if_unset
            .iter()
            .find(|key| before.contains_key(*key))
        {
            return Err(StoreError::LabelConflict(key.clone()));
        }
        let mut after = before.clone();
        for key in &request.clear {
            after.remove(key);
        }
        for (key, value) in &request.set {
            after.insert(key.clone(), value.clone());
        }
        if after.len() > MAX_LABELS_PER_MESSAGE {
            return Err(StoreError::TooManyLabels);
        }
        if after == before {
            let message = load_message(&tx, board, message_id)?;
            return Ok(Changed { message, seq: None });
        }
        // A value that is cleared or replaced is content that goes away.
        let removes = before
            .iter()
            .any(|(key, value)| after.get(key) != Some(value));
        for key in before.keys().filter(|key| !after.contains_key(*key)) {
            tx.execute(
                "DELETE FROM labels WHERE message_id = ?1 AND key = ?2",
                params![message_id, key],
            )?;
        }
        for (key, value) in &after {
            if before.get(key) != Some(value) {
                tx.execute(
                    "INSERT INTO labels (message_id, thread_id, key, value) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (message_id, key) DO UPDATE SET value = excluded.value",
                    params![message_id, current.thread_id, key, value],
                )?;
            }
        }
        upsert_actor(&tx, by)?;
        let seq = append_event(
            &tx,
            board,
            event_types::LABELS_CHANGED,
            (&current.channel, &current.thread_id, message_id),
            by,
            &now_rfc3339(),
        )?;
        let message = load_message(&tx, board, message_id)?;
        tx.commit()?;
        if removes {
            self.scrub_wal(&conn);
        }
        Ok(Changed {
            message,
            seq: Some(seq),
        })
    }

    /// Adds (`add`) or removes the principal's reaction `name`. `handle` is
    /// the principal's handle, without any profile.
    ///
    /// A message holds at most [`MAX_REACTION_NAMES_PER_MESSAGE`] different
    /// names. A name that is already on the message can always be joined,
    /// and a reaction can always be removed.
    pub(crate) fn react(
        &self,
        board: &BoardRef,
        message_id: &str,
        by: &Writer,
        handle: &str,
        name: &str,
        add: bool,
    ) -> Result<Changed> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = current_message(&tx, board, message_id)?;
        if current.deleted {
            return Err(StoreError::MessageDeleted);
        }
        let changed = if add {
            let (present, names): (bool, i64) = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM reactions WHERE message_id = ?1 AND name = ?2),
                        (SELECT count(DISTINCT name) FROM reactions WHERE message_id = ?1)",
                params![message_id, name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if !present && names >= MAX_REACTION_NAMES_PER_MESSAGE as i64 {
                return Err(StoreError::TooManyReactions);
            }
            tx.execute(
                "INSERT OR IGNORE INTO reactions (message_id, name, principal_id, handle)
                 VALUES (?1, ?2, ?3, ?4)",
                params![message_id, name, by.principal_id, handle],
            )?
        } else {
            tx.execute(
                "DELETE FROM reactions WHERE message_id = ?1 AND name = ?2 AND principal_id = ?3",
                params![message_id, name, by.principal_id],
            )?
        };
        if changed == 0 {
            let message = load_message(&tx, board, message_id)?;
            return Ok(Changed { message, seq: None });
        }
        upsert_actor(&tx, by)?;
        let seq = append_event(
            &tx,
            board,
            event_types::REACTIONS_CHANGED,
            (&current.channel, &current.thread_id, message_id),
            by,
            &now_rfc3339(),
        )?;
        let message = load_message(&tx, board, message_id)?;
        tx.commit()?;
        if !add {
            // The name that was taken back is content that goes away.
            self.scrub_wal(&conn);
        }
        Ok(Changed {
            message,
            seq: Some(seq),
        })
    }

    /// Every board, ordered by name, with the time of its newest message.
    pub(crate) fn boards(&self) -> Result<Vec<BoardInfo>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT b.name,
                    (SELECT max(c.last_activity_at) FROM channels c WHERE c.board_id = b.id)
             FROM boards b ORDER BY b.name_key, b.name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(BoardInfo {
                name: row.get(0)?,
                last_activity_at: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn channels(&self, board_id: i64) -> Result<Vec<ChannelInfo>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT path, thread_count, last_activity_at, intake FROM channels
             WHERE board_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map([board_id], |row| {
            Ok(ChannelInfo {
                path: row.get(0)?,
                thread_count: row.get::<_, i64>(1)?.max(0) as u64,
                last_activity_at: row.get(2)?,
                intake: row.get(3)?,
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
        label: Option<&LabelFilter>,
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
        if let Some(label) = label {
            // The root or any reply carries the label.
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM labels l WHERE l.thread_id = t.root_id AND l.key = ?",
            );
            values.push(Value::Text(label.key.clone()));
            if !label.values.is_empty() {
                sql.push_str(" AND l.value IN (");
                sql.push_str(&vec!["?"; label.values.len()].join(", "));
                sql.push(')');
                values.extend(label.values.iter().cloned().map(Value::Text));
            }
            sql.push(')');
        }
        if let Some(before) = before {
            sql.push_str(" AND t.last_seq < ?");
            values.push(Value::Integer(before));
        }
        sql.push_str(" ORDER BY t.last_seq DESC LIMIT ?");
        values.push(Value::Integer(i64::from(limit) + 1));

        // One read transaction, so each root and what hangs off it come
        // from the same snapshot.
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let conn = &tx;
        let head = head(conn, board.id)?;
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
        let mut roots: Vec<&mut Message> = threads
            .iter_mut()
            .map(|(_, summary)| &mut summary.root)
            .collect();
        fill_messages(conn, &mut roots)?;
        Ok(ThreadPage {
            threads,
            more,
            head,
        })
    }

    /// The thread rooted at `root_id`: the root and its replies, oldest first.
    pub(crate) fn thread(&self, board: &BoardRef, root_id: &str) -> Result<Option<Thread>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let conn = &tx;
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
        fill_messages(conn, &mut messages.iter_mut().collect::<Vec<_>>())?;
        let root = messages.remove(0);
        let trust = self.trust_of(conn, board, root_id)?;
        let head = head(conn, board.id)?;
        Ok(Some(Thread {
            root,
            replies: messages,
            cursor: Some(event_cursor(&board.tag, head)),
            trust,
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
        if let Some(kinds) = &filter.kinds {
            sql.push_str(" AND e.kind IN (");
            sql.push_str(&vec!["?"; kinds.len()].join(", "));
            sql.push(')');
            values.extend(kinds.iter().map(|kind| Value::Text((*kind).to_owned())));
        }
        if let Some(caller) = &filter.mentions {
            // Decided from the mentions as they are stored now. A tombstone
            // has none, and an edit replaces them. A mention that names a
            // profile is for that profile alone; `lower` folds ASCII only.
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM mentions mn WHERE mn.message_id = e.message_id
                              AND mn.principal_id = ? AND (mn.profile IS NULL",
            );
            values.push(Value::Text(caller.principal_id.clone()));
            if let Some(profile) = &caller.profile {
                sql.push_str(" OR lower(mn.profile) = lower(?)");
                values.push(Value::Text(profile.clone()));
            }
            sql.push_str("))");
        }
        if let Some(label) = &filter.label {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM labels l WHERE l.thread_id = e.thread_id AND l.key = ?",
            );
            values.push(Value::Text(label.key.clone()));
            if !label.values.is_empty() {
                sql.push_str(" AND l.value IN (");
                sql.push_str(&vec!["?"; label.values.len()].join(", "));
                sql.push(')');
                values.extend(label.values.iter().cloned().map(Value::Text));
            }
            sql.push(')');
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

/// The build that last started a server on a data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartedBy {
    pub version: String,
    pub commit: Option<String>,
    pub at: Option<String>,
}

impl StartedBy {
    /// For example `Openboard 0.1.0 (3ad55f3c0a1b)`.
    pub(crate) fn describe(&self) -> String {
        match &self.commit {
            Some(commit) => format!("Openboard {} ({commit})", self.version),
            None => format!("Openboard {}", self.version),
        }
    }
}

/// Who last started a server on the database open on `conn`. `None` for a
/// database only versions older than 0.1.0 have served, and for one whose
/// `meta` table cannot be read, whatever the reason.
pub(crate) fn started_by(conn: &Connection) -> Option<StartedBy> {
    let get = |key| identity::meta_get(conn, key).ok().flatten();
    let version = get(identity::META_STARTED_VERSION)?;
    Some(StartedBy {
        version,
        commit: get(identity::META_STARTED_COMMIT)
            .filter(|commit| commit != openboard_api::build::UNKNOWN),
        at: get(identity::META_STARTED_AT),
    })
}

/// Why a database with schema version `found` cannot be used, and what to
/// do about it.
pub(crate) fn unsupported_schema(conn: &Connection, found: i64) -> String {
    let this = openboard_api::build::describe();
    if found < 0 {
        return format!(
            "the database has schema version {found}, which no version of Openboard writes"
        );
    }
    let (writer, install) = match started_by(conn) {
        Some(by) => (
            format!("; it was last started by {}", by.describe()),
            format!("install Openboard {} or newer", by.version),
        ),
        None => (
            String::new(),
            "install the version of Openboard that last ran this board, or a newer one".to_owned(),
        ),
    };
    format!(
        "the database has schema version {found}, which is newer than this version of \
         Openboard ({this}) supports: it reads and writes up to schema version \
         {SCHEMA_VERSION}{writer}. To use this data directory, {install}; or restore a backup \
         taken before the upgrade: stop the server, move a file named {} over {DB_FILE_NAME}, \
         and remove {DB_FILE_NAME}-wal and {DB_FILE_NAME}-shm (`openboard admin backups` lists \
         the backups)",
        backup_pattern(found)
    )
}

/// The start of every backup's file name.
pub(crate) const BACKUP_PREFIX: &str = "openboard.db.pre-v";

/// The file names of the backups taken before a migration to `schema`, as
/// a pattern for people.
pub(crate) fn backup_pattern(schema: i64) -> String {
    format!("{BACKUP_PREFIX}{schema}-<UTC time>")
}

/// The file name of a backup taken now, before a migration to `schema`:
/// `openboard.db.pre-v3-20261119T153000Z`.
pub(crate) fn backup_file_name(schema: i64) -> String {
    // `2026-11-19T15:30:00.123Z` without its punctuation and milliseconds.
    let now = now_rfc3339();
    let compact: String = now
        .chars()
        .take(19)
        .filter(|c| !matches!(c, '-' | ':'))
        .collect();
    format!("{BACKUP_PREFIX}{schema}-{compact}Z")
}

/// Writes a consistent copy of the database beside it, before a migration
/// from schema version `found` changes anything, and tells the operator on
/// stderr where it is and how to go back. An error means no migration may
/// run: the database has not been touched, and what there is of the copy
/// has been removed.
///
/// The copy is made by SQLite's `VACUUM INTO` through the connection that
/// will migrate, so it holds exactly what that connection sees, including
/// what is still in the write-ahead log. It is one file, private to its
/// owner.
fn backup_before_migration(
    conn: &Connection,
    data_dir: &Path,
    found: i64,
) -> std::result::Result<PathBuf, String> {
    let path = data_dir.join(backup_file_name(SCHEMA_VERSION));
    let refused = |detail: String| {
        format!(
            "the database is at schema version {found} and must be migrated to version \
             {SCHEMA_VERSION}, but the backup that comes first could not be written to {}: \
             {detail}. Nothing was changed. Make the data directory writable with room for a \
             copy of the database (`openboard admin upgrade-check` shows how much), then start \
             again",
            path.display()
        )
    };
    // Created here, empty, so that it has its final mode before it has any
    // content. `VACUUM INTO` accepts an empty file and no other.
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|error| refused(error.to_string()))?;
    let written = (|| -> std::result::Result<u64, String> {
        let target = path
            .to_str()
            .ok_or_else(|| "the path is not valid UTF-8".to_owned())?;
        conn.execute("VACUUM INTO ?1", [target])
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        // The copy must open, be whole, and be at the old version.
        let copy = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| error.to_string())?;
        let version: i64 = copy
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        let check: String = copy
            .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if version != found || check != "ok" {
            return Err("the copy does not match the database".to_owned());
        }
        drop(copy);
        std::fs::metadata(&path)
            .map(|found| found.len())
            .map_err(|error| error.to_string())
    })();
    let bytes = match written {
        Ok(bytes) => bytes,
        Err(detail) => {
            let _ = std::fs::remove_file(&path);
            return Err(refused(detail));
        }
    };
    let database = data_dir.join(DB_FILE_NAME);
    let old = match started_by(conn) {
        Some(by) => format!("the OLD binary ({})", by.describe()),
        None => "the OLD binary (the version that ran this board until now)".to_owned(),
    };
    eprintln!(
        "openboard-server: migrating the database from schema version {found} to \
         {SCHEMA_VERSION} (Openboard {})",
        openboard_api::build::describe()
    );
    eprintln!(
        "openboard-server: backup written first: {} ({bytes} bytes, readable only by its owner)",
        path.display()
    );
    eprintln!(
        "openboard-server: to roll back: 1. stop the server; 2. move the backup into place: mv \
         {} {}; 3. remove {}-wal and {}-shm if they exist; 4. start {old}",
        path.display(),
        database.display(),
        database.display(),
        database.display(),
    );
    eprintln!(
        "openboard-server: the backup keeps the text of messages that are deleted or edited \
         after now, so delete it once the upgrade is confirmed: openboard admin backups --prune \
         --data-dir {}",
        data_dir.display()
    );
    Ok(path)
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
        OLDEST_IDENTITY_SCHEMA..=SCHEMA_VERSION => mode_of(&conn, data_dir).ok()?,
        _ => None,
    }
}

/// The mode of a data directory whose database, at a schema that has the
/// identity tables, is open on `conn`: the recorded one, except that a certificate, a key or
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
/// case, already exists. Returns the board either way. A board inserted
/// now gets the grants a new board starts with, in the same transaction:
/// `creator`, a principal ID and the handle it acts as, gets `all`.
fn ensure_board(conn: &Connection, name: &str, creator: Option<(&str, &str)>) -> Result<BoardRef> {
    let tag = random_hex(conn, 8)?;
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO boards (name, name_key, tag, created_at, head_hash)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![name, board_key(name), tag, now_rfc3339(), GENESIS_HASH],
    )? == 1;
    let board = find_board(conn, name)?
        .ok_or_else(|| StoreError::Other("board vanished after creation".to_owned()))?;
    if inserted {
        grants::board_created(conn, board.id, creator)?;
    }
    Ok(board)
}

/// [`ensure_board`] for an operator command: a board it creates has no
/// creator. Returns the board's ID.
pub(crate) fn ensure_board_for_operator(conn: &Connection, name: &str) -> Result<i64> {
    Ok(ensure_board(conn, name, None)?.id)
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
        edited_at: row.get(at + 11)?,
        deleted_at: row.get(at + 12)?,
        labels: Default::default(),
        reactions: Vec::new(),
        mentions: Vec::new(),
        html: None,
        task: None,
    })
}

/// Adds what is stored beside the rows of `messages`: their labels,
/// reactions and mentions. A tombstone has none.
///
/// It runs three statements however many messages there are, one for each
/// kind of thing, so reading a long thread or a full page of threads holds
/// the store's connection for three statements and not three per message.
/// The IDs travel as one JSON array, which no number of them outgrows.
fn fill_messages(conn: &Connection, messages: &mut [&mut Message]) -> Result<()> {
    let at: std::collections::HashMap<String, usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.deleted_at.is_none())
        .map(|(index, message)| (message.id.clone(), index))
        .collect();
    if at.is_empty() {
        return Ok(());
    }
    let ids = serde_json::to_string(&at.keys().collect::<Vec<_>>())
        .map_err(|error| StoreError::Other(format!("cannot list message IDs: {error}")))?;
    const WANTED: &str = "message_id IN (SELECT value FROM json_each(?1))";

    count_fill_statement();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT message_id, key, value FROM labels WHERE {WANTED}"
    ))?;
    let mut rows = stmt.query([&ids])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        if let Some(index) = at.get(&id) {
            messages[*index].labels.insert(row.get(1)?, row.get(2)?);
        }
    }

    count_fill_statement();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT message_id, name, handle FROM reactions WHERE {WANTED}
         ORDER BY message_id, name, id"
    ))?;
    let mut rows = stmt.query([&ids])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let Some(index) = at.get(&id) else {
            continue;
        };
        let (name, handle): (String, String) = (row.get(1)?, row.get(2)?);
        let reactions = &mut messages[*index].reactions;
        match reactions.last_mut() {
            Some(last) if last.name == name => last.by.push(handle),
            _ => reactions.push(Reaction {
                name,
                by: vec![handle],
            }),
        }
    }

    count_fill_statement();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT message_id, handle, kind, profile FROM mentions WHERE {WANTED}
         ORDER BY message_id, position"
    ))?;
    let mut rows = stmt.query([&ids])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let Some(index) = at.get(&id) else {
            continue;
        };
        let kind: String = row.get(2)?;
        let kind = parse_actor_kind(&kind).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                "unknown principal kind".into(),
            )
        })?;
        messages[*index].mentions.push(Mention {
            handle: row.get(1)?,
            kind,
            profile: row.get(3)?,
        });
    }

    count_fill_statement();
    tasks::fill_tasks(conn, &ids, &at, messages)?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Statements [`fill_messages`] has run on this thread.
    static FILL_STATEMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Counts one statement of [`fill_messages`], in tests only.
fn count_fill_statement() {
    #[cfg(test)]
    FILL_STATEMENTS.with(|count| count.set(count.get() + 1));
}

fn labels_of(
    conn: &Connection,
    message_id: &str,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut stmt = conn.prepare_cached("SELECT key, value FROM labels WHERE message_id = ?1")?;
    let rows = stmt.query_map([message_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// One message with everything stored beside it.
fn load_message(conn: &Connection, board: &BoardRef, message_id: &str) -> Result<Message> {
    let mut message = conn.query_row(
        &format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages m JOIN actors a ON a.id = m.actor_id
             WHERE m.board_id = ?1 AND m.id = ?2"
        ),
        params![board.id, message_id],
        |row| message_from_row(row, 0, &board.name),
    )?;
    fill_messages(conn, &mut [&mut message])?;
    Ok(message)
}

/// The state of a message that a change is checked against.
struct Current {
    channel: String,
    thread_id: String,
    revision: u32,
    deleted: bool,
}

/// Refuses to change a board notice: a message with the reserved label
/// `board.notice`. No request can set or clear that label.
fn refuse_notice(conn: &Connection, message_id: &str) -> Result<()> {
    if labels_of(conn, message_id)?.contains_key(openboard_api::grants::NOTICE_LABEL) {
        return Err(StoreError::BoardNotice);
    }
    Ok(())
}

fn current_message(conn: &Connection, board: &BoardRef, message_id: &str) -> Result<Current> {
    conn.query_row(
        "SELECT channel, thread_id, revision, deleted_at IS NOT NULL FROM messages
         WHERE board_id = ?1 AND id = ?2",
        params![board.id, message_id],
        |row| {
            Ok(Current {
                channel: row.get(0)?,
                thread_id: row.get(1)?,
                revision: row.get(2)?,
                deleted: row.get(3)?,
            })
        },
    )
    .optional()?
    .ok_or(StoreError::MessageMissing)
}

/// Records the actor behind a write, the first time it writes.
fn upsert_actor(conn: &Connection, by: &Writer) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO actors (id, kind, principal, profile, handle)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            by.actor.id,
            actor_kind_name(by.actor.kind),
            by.principal_id,
            by.actor.profile,
            by.actor.handle
        ],
    )?;
    Ok(())
}

/// A message [`insert_message`] stored.
pub(crate) struct Inserted {
    pub id: String,
    pub thread_id: String,
    /// Position of its `message.posted` event.
    pub seq: i64,
    pub now: String,
    pub mentions: Vec<(String, Mention)>,
}

/// Stores a new thread (`thread_id` is `None`) or a reply, with its
/// mentions, the channel and thread bookkeeping and its `message.posted`
/// event. The caller holds a write transaction.
fn insert_message(
    conn: &Connection,
    board: &BoardRef,
    channel: &str,
    thread_id: Option<&str>,
    by: &Writer,
    text: &str,
    mentions: &MentionInput,
) -> Result<Inserted> {
    // The message records the position its event is about to take.
    let seq = head(conn, board.id)? + 1;
    let now = now_rfc3339();
    let id = format!("msg_{}", random_hex(conn, 12)?);
    let is_root = thread_id.is_none();
    let thread_id = thread_id.map_or_else(|| id.clone(), str::to_owned);
    let actor = &by.actor;

    upsert_actor(conn, by)?;
    conn.execute(
        "INSERT INTO messages (id, board_id, channel, thread_id, actor_id, text, created_at, seq, credential_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            board.id,
            channel,
            thread_id,
            actor.id,
            text,
            now,
            seq,
            by.credential_id
        ],
    )?;
    let mentions = resolve_mentions(conn, mentions)?;
    write_mentions(conn, &id, &mentions)?;
    conn.execute(
        "INSERT INTO channels (board_id, path, created_at, thread_count, last_activity_at)
         VALUES (?1, ?2, ?3, ?4, ?3)
         ON CONFLICT (board_id, path) DO UPDATE SET
             thread_count = thread_count + excluded.thread_count,
             last_activity_at = excluded.last_activity_at",
        params![board.id, channel, now, i64::from(is_root)],
    )?;
    if is_root {
        conn.execute(
            "INSERT INTO threads (root_id, board_id, channel, last_seq, last_activity_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, board.id, channel, seq, now],
        )?;
    } else {
        let updated = conn.execute(
            "UPDATE threads SET reply_count = reply_count + 1, last_seq = ?1, last_activity_at = ?2
             WHERE root_id = ?3 AND board_id = ?4",
            params![seq, now, thread_id, board.id],
        )?;
        if updated != 1 {
            return Err(StoreError::ThreadMissing);
        }
    }
    let appended = append_event(
        conn,
        board,
        event_types::MESSAGE_POSTED,
        (channel, &thread_id, &id),
        by,
        &now,
    )?;
    if appended != seq {
        return Err(StoreError::Other(
            "the event log moved inside a write transaction".to_owned(),
        ));
    }
    Ok(Inserted {
        id,
        thread_id,
        seq,
        now,
        mentions,
    })
}

/// Appends one event to the board's log, chained to the one before it, and
/// moves the board's head. Returns the event's position. Every event of
/// every type is written here, so every one is covered by the hash chain.
/// The caller holds a write transaction and has recorded the actor.
fn append_event(
    conn: &Connection,
    board: &BoardRef,
    kind: &str,
    (channel, thread_id, message_id): (&str, &str, &str),
    by: &Writer,
    at: &str,
) -> Result<i64> {
    let (head_seq, head_hash): (i64, String) = conn.query_row(
        "SELECT head_seq, head_hash FROM boards WHERE id = ?1",
        [board.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let seq = head_seq + 1;
    let hash = event_hash(
        HASH_VERSION,
        &EventFields {
            prev_hash: &head_hash,
            board: &board.name,
            seq,
            kind,
            channel,
            thread_id,
            message_id,
            actor_id: &by.actor.id,
            actor_kind: actor_kind_name(by.actor.kind),
            actor_handle: &by.actor.handle,
            actor_profile: by.actor.profile.as_deref(),
            at,
            credential_id: by.credential_id.as_deref(),
        },
    )
    .ok_or_else(|| StoreError::Other("the current hash encoding is unknown".to_owned()))?;
    conn.execute(
        "INSERT INTO events (board_id, seq, kind, channel, thread_id, message_id, actor_id, at, prev_hash, hash, credential_id, hash_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            board.id,
            seq,
            kind,
            channel,
            thread_id,
            message_id,
            by.actor.id,
            at,
            head_hash,
            hash,
            by.credential_id,
            HASH_VERSION
        ],
    )?;
    conn.execute(
        "UPDATE boards SET head_seq = ?1, head_hash = ?2 WHERE id = ?3",
        params![seq, hash, board.id],
    )?;
    Ok(seq)
}

/// Resolves mention candidates to the principals they name, in order,
/// without repeats, at most [`MAX_MENTIONS_PER_MESSAGE`]. A candidate that
/// names nobody is dropped. Returns each principal's ID with its mention.
fn resolve_mentions(conn: &Connection, input: &MentionInput) -> Result<Vec<(String, Mention)>> {
    let mut known: std::collections::HashMap<String, Option<(String, Mention)>> =
        std::collections::HashMap::new();
    let mut resolved: Vec<(String, Mention)> = Vec::new();
    for candidate in &input.candidates {
        if resolved.len() >= MAX_MENTIONS_PER_MESSAGE {
            break;
        }
        let key = handle_key(&candidate.handle);
        let principal = match known.get(&key) {
            Some(found) => found.clone(),
            None => {
                let found = find_mentioned(conn, &key, input.local)?;
                known.insert(key, found.clone());
                found
            }
        };
        let Some((principal_id, mut mention)) = principal else {
            continue;
        };
        mention.profile = candidate.profile.clone();
        let repeated = resolved.iter().any(|(id, earlier)| {
            *id == principal_id
                && match (&earlier.profile, &mention.profile) {
                    (None, None) => true,
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    _ => false,
                }
        });
        if !repeated {
            resolved.push((principal_id, mention));
        }
    }
    Ok(resolved)
}

/// The principal a mention's handle names, by its comparison key, with a
/// mention of it that names no profile.
fn find_mentioned(conn: &Connection, key: &str, local: bool) -> Result<Option<(String, Mention)>> {
    let (id, handle, kind) = if local {
        // A local board's one principal is in no table.
        let local = crate::authz::Principal::local();
        if key != handle_key(&local.handle) {
            return Ok(None);
        }
        (local.id, local.handle, local.kind)
    } else {
        let found: Option<(String, String, String)> = conn
            .query_row(
                "SELECT id, handle, kind FROM principals WHERE handle_key = ?1",
                [key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((id, handle, kind)) = found else {
            return Ok(None);
        };
        let Some(kind) = parse_actor_kind(&kind) else {
            return Ok(None);
        };
        (id, handle, kind)
    };
    Ok(Some((
        id,
        Mention {
            handle,
            kind,
            profile: None,
        },
    )))
}

/// Replaces the stored mentions of a message.
fn write_mentions(
    conn: &Connection,
    message_id: &str,
    mentions: &[(String, Mention)],
) -> Result<()> {
    conn.execute("DELETE FROM mentions WHERE message_id = ?1", [message_id])?;
    for (position, (principal_id, mention)) in mentions.iter().enumerate() {
        conn.execute(
            "INSERT INTO mentions (message_id, position, principal_id, handle, kind, profile)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                message_id,
                position as i64,
                principal_id,
                mention.handle,
                actor_kind_name(mention.kind),
                mention.profile
            ],
        )?;
    }
    Ok(())
}

/// Removes the digest of a message's original text that an idempotency key
/// kept, when that text goes away. The key stays, spent: with the digest
/// gone it can never be matched again, so a retry is refused as a conflict
/// and never posts the text a second time.
fn forget_fingerprint(conn: &Connection, message_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE idempotency SET fingerprint = '' WHERE message_id = ?1 AND fingerprint <> ''",
        [message_id],
    )?;
    Ok(())
}

/// Tries to move the write-ahead log into the database file and empty it,
/// without waiting for anyone. Returns whether the log is now empty.
///
/// The statement runs with no busy timeout. Where another connection
/// stands in the way, SQLite then moves what it can and reports that it
/// was held up, in the first column of the row the statement returns,
/// instead of calling the busy handler for as long as the timeout lasts.
/// That row is what decides the outcome, together with the length of the
/// log file itself; an error counts as not emptied.
fn truncate_wal(conn: &Connection, data_dir: &Path) -> bool {
    if conn.busy_timeout(Duration::ZERO).is_err() {
        return false;
    }
    let held_up = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        row.get::<_, i64>(0)
    });
    if conn.busy_timeout(BUSY_TIMEOUT).is_err() {
        eprintln!("openboard-server: could not restore the database's busy timeout");
    }
    let wal = data_dir.join(format!("{DB_FILE_NAME}-wal"));
    let empty = match std::fs::metadata(&wal) {
        Ok(found) => found.len() == 0,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    };
    matches!(held_up, Ok(0)) && empty
}

impl Drop for Store {
    /// The last scrub, as the store closes. Closing the connection empties
    /// the log too, but only when no other process has the database open.
    fn drop(&mut self) {
        let conn = self
            .conn
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner());
        let emptied = truncate_wal(conn, &self.data_dir);
        if !emptied && self.scrub_pending() {
            eprintln!(
                "openboard-server: another process was reading the database, so content \
                 removed from the board may remain in its write-ahead log until the next start"
            );
        }
    }
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

    fn local_writer() -> Writer {
        let local = crate::authz::Principal::local();
        Writer {
            actor: local.actor(None),
            principal_id: local.id.clone(),
            credential_id: None,
        }
    }

    fn post(store: &Store, board: &BoardRef, thread: Option<&str>, text: &str) -> Message {
        let by = local_writer();
        let post = NewPost {
            channel: "dev".to_owned(),
            thread_id: thread.map(str::to_owned),
            actor: by.actor,
            principal_id: by.principal_id,
            credential_id: None,
            text: text.to_owned(),
            idempotency: None,
            mentions: MentionInput {
                candidates: crate::markdown::mention_candidates(text),
                local: true,
            },
        };
        store
            .post(PostTarget::Board(board.clone()), post)
            .unwrap()
            .message
    }

    /// Runs `read` and returns how many statements it spent on labels,
    /// reactions and mentions.
    fn fill_statements<T>(read: impl FnOnce() -> T) -> (T, usize) {
        FILL_STATEMENTS.set(0);
        let result = read();
        (result, FILL_STATEMENTS.get())
    }

    #[test]
    fn a_read_loads_labels_reactions_mentions_and_tasks_in_four_statements() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let board = store.ensure_board("general").unwrap();
        let by = local_writer();
        let label = |id: &str, key: &str, value: &str| {
            let request = LabelsRequest {
                set: [(key.to_owned(), value.to_owned())].into(),
                ..LabelsRequest::default()
            };
            store.label(&board, id, &by, &request).unwrap();
        };
        let react = |id: &str, name: &str| {
            store.react(&board, id, &by, "local", name, true).unwrap();
        };

        let root = post(&store, &board, None, "the root, for @local");
        label(&root.id, "status", "open");
        react(&root.id, "eyes");
        let replies = 60;
        let mut deleted = String::new();
        for n in 0..replies {
            let text = if n % 2 == 0 {
                format!("reply {n} to @local")
            } else {
                format!("reply {n}")
            };
            let reply = post(&store, &board, Some(&root.id), &text);
            if n % 3 == 0 {
                label(&reply.id, "n", &n.to_string());
                label(&reply.id, "kind", "third");
            }
            if n % 4 == 0 {
                react(&reply.id, "zz");
                react(&reply.id, &format!("r{n}"));
                react(&reply.id, "aa");
            }
            if n == 7 {
                label(&reply.id, "gone", "with the message");
                store.delete(&board, &reply.id, &by, None).unwrap();
                deleted = reply.id;
            }
        }

        let (thread, statements) = fill_statements(|| store.thread(&board, &root.id));
        let thread = thread.unwrap().unwrap();
        assert_eq!(thread.replies.len(), replies);
        assert_eq!(
            statements,
            4,
            "a thread of {} messages",
            thread.replies.len() + 1
        );

        // Everything landed on the message it belongs to, in order.
        assert_eq!(thread.root.labels["status"], "open");
        assert_eq!(thread.root.reactions.len(), 1);
        assert_eq!(thread.root.mentions.len(), 1);
        for (n, reply) in thread.replies.iter().enumerate() {
            assert!(reply.text.is_empty() || reply.text.starts_with(&format!("reply {n}")));
            if reply.id == deleted {
                assert!(reply.deleted_at.is_some());
                assert!(reply.labels.is_empty() && reply.reactions.is_empty());
                assert!(reply.mentions.is_empty());
                continue;
            }
            let labels: Vec<(&str, &str)> = reply
                .labels
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            if n % 3 == 0 {
                assert_eq!(labels, [("kind", "third"), ("n", n.to_string().as_str())]);
            } else {
                assert!(labels.is_empty(), "reply {n}: {labels:?}");
            }
            let names: Vec<&str> = reply.reactions.iter().map(|r| r.name.as_str()).collect();
            if n % 4 == 0 {
                assert_eq!(names, ["aa", format!("r{n}").as_str(), "zz"], "reply {n}");
                assert!(reply.reactions.iter().all(|r| r.by == ["local"]));
            } else {
                assert!(names.is_empty(), "reply {n}: {names:?}");
            }
            let mentioned: Vec<&str> = reply.mentions.iter().map(|m| m.handle.as_str()).collect();
            if n % 2 == 0 {
                assert_eq!(mentioned, ["local"], "reply {n}");
            } else {
                assert!(mentioned.is_empty(), "reply {n}");
            }
            // The same as reading the message on its own.
            assert_eq!(
                *reply,
                load_message(&store.conn(), &board, &reply.id).unwrap()
            );
        }

        // A page of threads costs the same three statements.
        for n in 0..40 {
            let other = post(&store, &board, None, &format!("thread {n} @local"));
            label(&other.id, "n", &n.to_string());
        }
        let channel = ChannelSel::Exact("dev".to_owned());
        let (page, statements) =
            fill_statements(|| store.threads(&board, &channel, None, None, 500));
        let page = page.unwrap();
        assert_eq!(page.threads.len(), 41);
        assert_eq!(statements, 4, "a page of {} threads", page.threads.len());
        for (_, summary) in &page.threads[..40] {
            assert_eq!(summary.root.labels.len(), 1);
            assert_eq!(summary.root.mentions.len(), 1);
            assert!(summary.root.text.ends_with("@local"));
        }
        assert_eq!(page.threads[40].1.root.id, root.id);
        assert_eq!(page.threads[40].1.root.labels["status"], "open");

        // Nothing to load, nothing asked.
        let lone = post(&store, &board, None, "soon gone");
        store.delete(&board, &lone.id, &by, None).unwrap();
        let (_, statements) = fill_statements(|| store.thread(&board, &lone.id));
        assert_eq!(statements, 0);
    }

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
