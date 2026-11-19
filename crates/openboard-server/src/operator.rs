//! Operator functions: enrolment, revocation and the security log.
//!
//! These act on a board's data directory, on the board's own host. There is
//! no network API for any of them, so only someone who can already read and
//! write the data directory can create an invite, list or revoke
//! credentials, or read the security log.
//!
//! Every function works whether or not a server is running on the
//! directory. They open their own database connection, wait up to a few
//! seconds for a busy database, and never take the lock a server holds. A
//! running server sees a revocation on its next request, and ends the
//! long-polls and streams of the revoked credential within
//! [`crate::Limits::credential_recheck`], also when their reader has stalled.
//!
//! They block on disk I/O. Call them from a blocking context.
//!
//! Nothing returned here holds a secret, with one exception:
//! [`CreatedInvite::invite`] carries the new enrolment code, which exists
//! nowhere else and cannot be read back.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use openboard_api::auth::{
    CredentialInfo, Invite, DEFAULT_INVITE_TTL_SECS, DEFAULT_TOKEN_TTL_SECS,
};
use openboard_api::ActorKind;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use crate::identity::{self, LogEntry};
use crate::names;
use crate::store::{DB_FILE_NAME, OLDEST_IDENTITY_SCHEMA, SCHEMA_VERSION};
use crate::time::{now_ms, rfc3339_from_ms};
use crate::Mode;

pub use crate::identity::log_kinds;

/// Why an operator function failed.
#[derive(Debug)]
pub enum OperatorError {
    /// The data directory holds no board database.
    NoDatabase(PathBuf),
    /// The data directory is not a network board, or no server has started
    /// on it in network mode yet, so it has no certificate to pin.
    NotNetwork,
    /// The name breaks the naming rules or is reserved. Holds the reason.
    InvalidName(&'static str),
    /// The board URL is not usable in an invite. Holds the reason.
    InvalidUrl(&'static str),
    /// No credential has this ID.
    NoSuchCredential(String),
    /// No principal has this name.
    NoSuchPrincipal(String),
    /// The database could not be read or written.
    Storage(String),
}

impl fmt::Display for OperatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OperatorError::NoDatabase(path) => {
                write!(f, "no board database at {}", path.display())
            }
            OperatorError::NotNetwork => f.write_str(
                "this data directory is not a network board yet: start the server on it once \
                 in network mode, which creates its certificate",
            ),
            OperatorError::InvalidName(reason) => write!(f, "invalid name: {reason}"),
            OperatorError::InvalidUrl(reason) => write!(f, "invalid board URL: {reason}"),
            OperatorError::NoSuchCredential(id) => write!(f, "no credential has the ID {id}"),
            OperatorError::NoSuchPrincipal(name) => write!(f, "no agent is named {name}"),
            OperatorError::Storage(detail) => {
                write!(f, "cannot use the board database: {detail}")
            }
        }
    }
}

impl std::error::Error for OperatorError {}

impl From<rusqlite::Error> for OperatorError {
    fn from(error: rusqlite::Error) -> Self {
        OperatorError::Storage(error.to_string())
    }
}

type Result<T> = std::result::Result<T, OperatorError>;

/// How a board is reached and pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardInfo {
    pub mode: Mode,
    /// The URL the last network start recorded. `None` on a local board.
    pub url: Option<String>,
    /// The certificate's pin, as `openboard_api::auth::fingerprint` formats
    /// it. `None` on a local board.
    pub fingerprint: Option<String>,
}

/// Options of [`create_invite`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteOptions {
    /// How long the invite can be used, in seconds.
    pub ttl_secs: u64,
    /// Lifetime of the token the invite turns into, in seconds, counted
    /// from enrolment.
    pub token_ttl_secs: u64,
    /// The board URL to put in the invite, for when clients reach the board
    /// by another address than the one the server recorded. Must start with
    /// `https://`.
    pub url: Option<String>,
}

impl Default for InviteOptions {
    /// [`DEFAULT_INVITE_TTL_SECS`], [`DEFAULT_TOKEN_TTL_SECS`] and the
    /// recorded URL.
    fn default() -> Self {
        Self {
            ttl_secs: DEFAULT_INVITE_TTL_SECS,
            token_ttl_secs: DEFAULT_TOKEN_TTL_SECS,
            url: None,
        }
    }
}

/// The result of [`create_invite`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedInvite {
    /// Ready for [`Invite::encode`]. A secret until used or expired.
    pub invite: Invite,
    /// Non-secret ID of the invite, as [`PendingInvite::id`] lists it.
    pub id: String,
    /// The principal's handle as the board records it.
    pub principal: String,
    /// True when this invite created the principal, false when it adds a
    /// credential to an existing one.
    pub new_principal: bool,
    /// RFC 3339 timestamp in UTC.
    pub expires_at: String,
}

/// An agent known to the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalInfo {
    pub id: String,
    pub handle: String,
    pub kind: ActorKind,
    /// RFC 3339 timestamp in UTC.
    pub created_at: String,
    /// Credentials that are neither revoked nor expired.
    pub active_credentials: u64,
    /// Invites that can still be used.
    pub pending_invites: u64,
}

/// An invite that can still be used. Never holds its code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInvite {
    pub id: String,
    /// Handle of the principal the invite enrols.
    pub principal: String,
    /// RFC 3339 timestamps in UTC.
    pub created_at: String,
    pub expires_at: String,
}

/// What a revocation changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revoked {
    /// Handle of the principal concerned.
    pub principal: String,
    /// IDs of the credentials this call revoked. Empty when they already
    /// were.
    pub credentials: Vec<String>,
    /// IDs of the pending invites this call withdrew.
    pub invites: Vec<String>,
}

/// One security log entry. Never holds a token, a code or message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityLogEntry {
    /// Position in the log. Entries are numbered in the order written.
    pub id: i64,
    /// RFC 3339 timestamp in UTC.
    pub at: String,
    /// One of [`log_kinds`].
    pub kind: String,
    /// Handle of the principal concerned, where known.
    pub principal: Option<String>,
    pub credential_id: Option<String>,
    /// The client address with its host part zeroed: the last octet of an
    /// IPv4 address, the last 80 bits of an IPv6 address.
    pub client: Option<String>,
    /// Non-secret detail, such as the mode or an invite ID.
    pub detail: Option<String>,
}

/// The schema state of an opened database.
enum Schema {
    /// Written by milestone M0 and not opened by a newer server yet. It is
    /// a local board and has none of the tables used here.
    Old,
    Current,
}

fn open(data_dir: &Path, write: bool) -> Result<(Connection, Schema)> {
    let path = data_dir.join(DB_FILE_NAME);
    if !path.is_file() {
        return Err(OperatorError::NoDatabase(path));
    }
    let access = if write {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let conn = Connection::open_with_flags(&path, access | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let schema = match version {
        1 => Schema::Old,
        // Milestone M1's schema has every table used here.
        OLDEST_IDENTITY_SCHEMA..=SCHEMA_VERSION => Schema::Current,
        other => {
            return Err(OperatorError::Storage(format!(
            "the database has schema version {other}, but this version supports {SCHEMA_VERSION}"
        )))
        }
    };
    Ok((conn, schema))
}

fn info_of(conn: &Connection, schema: &Schema, data_dir: &Path) -> Result<BoardInfo> {
    if matches!(schema, Schema::Old) {
        return Ok(BoardInfo {
            mode: Mode::Local,
            url: None,
            fingerprint: None,
        });
    }
    // As the server reads it: a directory with a certificate or any agent
    // is a network one even if its mode record is missing.
    let mode = crate::store::mode_of(conn, data_dir)?.unwrap_or(Mode::Local);
    let network = |key| -> Result<Option<String>> {
        Ok(match mode {
            Mode::Network => identity::meta_get(conn, key)?,
            Mode::Local => None,
        })
    };
    Ok(BoardInfo {
        mode,
        url: network(identity::META_URL)?,
        fingerprint: network(identity::META_FINGERPRINT)?,
    })
}

/// Opens for writing and requires a network board that has started once.
/// Returns the connection with the board's recorded URL and fingerprint.
fn open_network(data_dir: &Path) -> Result<(Connection, String, String)> {
    let (conn, schema) = open(data_dir, true)?;
    match info_of(&conn, &schema, data_dir)? {
        BoardInfo {
            mode: Mode::Network,
            url: Some(url),
            fingerprint: Some(fingerprint),
        } => Ok((conn, url, fingerprint)),
        _ => Err(OperatorError::NotNetwork),
    }
}

/// The board's mode and, for a network board, its URL and certificate
/// fingerprint as recorded by the last server start.
pub fn board_info(data_dir: &Path) -> Result<BoardInfo> {
    let (conn, schema) = open(data_dir, false)?;
    info_of(&conn, &schema, data_dir)
}

/// Creates a single-use invite for the agent `name`.
///
/// A new name creates the principal, which reserves the handle from then
/// on. An existing name, compared without regard to case, gets an invite
/// for one more credential of the same principal. The name starts with a
/// letter and has up to 32 letters, digits and hyphens; `local` and
/// `anonymous` are reserved.
///
/// Fails with [`OperatorError::NotNetwork`] until a server has started on
/// the directory in network mode.
pub fn create_invite(
    data_dir: &Path,
    name: &str,
    options: &InviteOptions,
) -> Result<CreatedInvite> {
    names::validate_handle(name).map_err(OperatorError::InvalidName)?;
    let url_override = match &options.url {
        None => None,
        Some(url) => Some(crate::normalize_public_url(url).map_err(OperatorError::InvalidUrl)?),
    };
    let (mut conn, recorded_url, fingerprint) = open_network(data_dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let created = identity::create_invite(
        &tx,
        name,
        ttl_ms(options.ttl_secs),
        ttl_ms(options.token_ttl_secs),
        now_ms(),
    )
    .map_err(OperatorError::Storage)?;
    tx.commit()?;
    Ok(CreatedInvite {
        invite: Invite {
            url: url_override.unwrap_or(recorded_url),
            fingerprint,
            code: created.code,
        },
        id: created.row.id,
        principal: created.row.handle,
        new_principal: created.new_principal,
        expires_at: rfc3339_from_ms(created.row.expires_ms),
    })
}

fn ttl_ms(secs: u64) -> i64 {
    // Capped at a thousand years, far below where adding it to the clock
    // could overflow.
    const MAX_SECS: u64 = 1000 * 366 * 24 * 60 * 60;
    (secs.min(MAX_SECS) * 1000) as i64
}

/// Every principal, oldest first. Empty on a local board.
pub fn list_principals(data_dir: &Path) -> Result<Vec<PrincipalInfo>> {
    let (conn, schema) = open(data_dir, false)?;
    if matches!(schema, Schema::Old) {
        return Ok(Vec::new());
    }
    Ok(identity::list_principals(&conn, now_ms())?
        .into_iter()
        .map(|row| PrincipalInfo {
            id: row.id,
            handle: row.handle,
            kind: row.kind,
            created_at: rfc3339_from_ms(row.created_ms),
            active_credentials: row.active_credentials,
            pending_invites: row.pending_invites,
        })
        .collect())
}

/// Every credential, oldest first, including revoked and expired ones.
/// Empty on a local board.
pub fn list_credentials(data_dir: &Path) -> Result<Vec<CredentialInfo>> {
    let (conn, schema) = open(data_dir, false)?;
    if matches!(schema, Schema::Old) {
        return Ok(Vec::new());
    }
    Ok(identity::list_credentials(&conn)?
        .iter()
        .map(identity::Credential::info)
        .collect())
}

/// Invites that can still be used, oldest first. Empty on a local board.
pub fn list_invites(data_dir: &Path) -> Result<Vec<PendingInvite>> {
    let (conn, schema) = open(data_dir, false)?;
    if matches!(schema, Schema::Old) {
        return Ok(Vec::new());
    }
    Ok(identity::list_pending_invites(&conn, now_ms())?
        .into_iter()
        .map(|row| PendingInvite {
            id: row.id,
            principal: row.handle,
            created_at: rfc3339_from_ms(row.created_ms),
            expires_at: rfc3339_from_ms(row.expires_ms),
        })
        .collect())
}

/// Revokes one credential by its ID. Revoking a credential that is already
/// revoked succeeds and changes nothing.
pub fn revoke_credential(data_dir: &Path, credential_id: &str) -> Result<Revoked> {
    let (mut conn, _, _) = open_network(data_dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some((credential, changed)) = identity::revoke_credential(&tx, credential_id, now_ms())?
    else {
        return Err(OperatorError::NoSuchCredential(credential_id.to_owned()));
    };
    if changed {
        identity::log(
            &tx,
            &LogEntry {
                kind: log_kinds::CREDENTIAL_REVOKED,
                principal: Some(&credential.handle),
                credential_id: Some(&credential.id),
                detail: Some("by operator"),
                ..LogEntry::default()
            },
        )?;
    }
    tx.commit()?;
    Ok(Revoked {
        principal: credential.handle,
        credentials: if changed {
            vec![credential.id]
        } else {
            Vec::new()
        },
        invites: Vec::new(),
    })
}

/// Revokes every credential of the principal `name` and withdraws its
/// pending invites. The principal keeps its handle, and a later
/// [`create_invite`] for the same name enrols it again.
pub fn revoke_principal(data_dir: &Path, name: &str) -> Result<Revoked> {
    let (mut conn, _, _) = open_network(data_dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(revoked) = identity::revoke_principal(&tx, name, now_ms())? else {
        return Err(OperatorError::NoSuchPrincipal(name.to_owned()));
    };
    tx.commit()?;
    Ok(Revoked {
        principal: revoked.handle,
        credentials: revoked.credentials,
        invites: revoked.invites,
    })
}

/// The security log, oldest entry first. With `limit`, only the newest
/// `limit` entries. Empty for a directory no M1 server has opened yet.
///
/// A server trims the log to [`crate::Config::security_log`] when it starts
/// and while it runs, and leaves a [`log_kinds::LOG_PRUNED`] entry when it
/// removes anything. Nothing else can remove or change an entry.
pub fn security_log(data_dir: &Path, limit: Option<u32>) -> Result<Vec<SecurityLogEntry>> {
    let (conn, schema) = open(data_dir, false)?;
    if matches!(schema, Schema::Old) {
        return Ok(Vec::new());
    }
    let limit = limit.map_or(-1, i64::from);
    let mut entries = conn
        .prepare(
            "SELECT id, at, kind, principal, credential_id, client, detail
             FROM security_log ORDER BY id DESC LIMIT ?1",
        )?
        .query_map([limit], |row| {
            Ok(SecurityLogEntry {
                id: row.get(0)?,
                at: row.get(1)?,
                kind: row.get(2)?,
                principal: row.get(3)?,
                credential_id: row.get(4)?,
                client: row.get(5)?,
                detail: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    entries.reverse();
    Ok(entries)
}
