//! Operator functions: enrolment, sign-in links, revocation and the
//! security log.
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
//! Agents and people are both principals. An agent is invited by name and
//! enrols with a token; a person is invited by email address and signs in
//! from a browser, which gives them a session. Tokens and sessions are both
//! credentials: [`list_credentials`], [`revoke_credential`] and
//! [`revoke_principal`] treat them alike, and [`CredentialInfo::kind`] says
//! which is which, since only a person holds sessions. [`invite`] takes
//! either kind of name.
//!
//! [`upgrade_check`], [`list_backups`] and [`prune_backups`] are about
//! upgrading the server: what the next start would do to the database, and
//! the backups earlier migrations left. They read the directory whatever
//! version wrote it, a newer one included.
//!
//! Nothing returned here holds a secret, with two exceptions that exist
//! nowhere else and cannot be read back: [`CreatedInvite::invite`] carries
//! a new enrolment code, and [`PersonInvite::link`] and
//! [`PersonInvite::code`] a new sign-in code.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use openboard_api::auth::{
    CredentialInfo, Invite, DEFAULT_INVITE_TTL_SECS, DEFAULT_TOKEN_TTL_SECS,
};
use openboard_api::web::{sign_in_link, DEFAULT_SESSION_TTL_SECS};
use openboard_api::ActorKind;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use crate::identity::{self, InviteKind, LogEntry};
use crate::names;
use crate::store::{BACKUP_PREFIX, DB_FILE_NAME, OLDEST_IDENTITY_SCHEMA, SCHEMA_VERSION};
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
    /// The name breaks the naming rules or is reserved, or the email
    /// address is not one the board accepts. Holds the reason.
    InvalidName(&'static str),
    /// The board URL is not usable in an invite. Holds the reason.
    InvalidUrl(&'static str),
    /// No credential has this ID.
    NoSuchCredential(String),
    /// No principal has this name.
    NoSuchPrincipal(String),
    /// No grant has this ID.
    NoSuchGrant(String),
    /// The grant, its pattern or its verbs are not valid. Holds the reason.
    InvalidGrant(String),
    /// The database is older than this version: start the server on it once
    /// to migrate it first.
    NotMigrated,
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
            OperatorError::NoSuchPrincipal(name) => write!(f, "no agent or person is named {name}"),
            OperatorError::NoSuchGrant(id) => write!(f, "no grant has the ID {id}"),
            OperatorError::InvalidGrant(reason) => write!(f, "invalid grant: {reason}"),
            OperatorError::NotMigrated => f.write_str(
                "this data directory has no grants yet: start this version of the server on it \
                 once, which migrates it",
            ),
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

/// How a board is reached, and how its clients know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardInfo {
    pub mode: Mode,
    /// The URL the last network start recorded. `None` on a local board.
    pub url: Option<String>,
    /// The pin of a self-signed certificate, as
    /// `openboard_api::auth::fingerprint` formats it. `None` on a local
    /// board and when `trusted`.
    pub fingerprint: Option<String>,
    /// True for a network board that serves a certificate its operator
    /// supplied: clients pin nothing and check it against their certificate
    /// authorities and the host name in `url`.
    pub trusted: bool,
}

/// Options of [`invite`], [`create_invite`] and [`create_person_invite`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteOptions {
    /// How long the invite or sign-in link can be used, in seconds.
    pub ttl_secs: u64,
    /// Lifetime of the credential the invite turns into, in seconds,
    /// counted from enrolment or sign-in: an agent's token or a person's
    /// browser session. `None` is the default for the kind:
    /// [`DEFAULT_TOKEN_TTL_SECS`] for a token and
    /// [`DEFAULT_SESSION_TTL_SECS`] for a session.
    pub token_ttl_secs: Option<u64>,
    /// The board URL to put in the invite or the sign-in link, for when
    /// clients reach the board by another address than the one the server
    /// recorded. Must start with `https://`.
    pub url: Option<String>,
}

impl Default for InviteOptions {
    /// [`DEFAULT_INVITE_TTL_SECS`], the default credential lifetime of the
    /// kind, and the recorded URL.
    fn default() -> Self {
        Self {
            ttl_secs: DEFAULT_INVITE_TTL_SECS,
            token_ttl_secs: None,
            url: None,
        }
    }
}

/// The result of [`invite`]: what to hand to the agent or the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invited {
    /// The name was an agent's. See [`create_invite`].
    Agent(CreatedInvite),
    /// The name was an email address. See [`create_person_invite`].
    Person(PersonInvite),
}

/// The result of [`create_person_invite`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonInvite {
    /// The one-time sign-in link to hand to the person, as
    /// `openboard_api::web::sign_in_link` builds it: the board's URL, the
    /// sign-in page, and the code as the fragment. A secret until used or
    /// expired.
    pub link: String,
    /// The code in the link's fragment. A secret until used or expired.
    pub code: String,
    /// The board URL the link points to.
    pub url: String,
    /// Non-secret ID of the invite, as [`PendingInvite::id`] lists it.
    pub id: String,
    /// The person's handle as the board records it: the email address,
    /// lowercased.
    pub principal: String,
    /// True when this invite created the person, false when it lets a
    /// known person sign in once more, for example from a second browser.
    pub new_principal: bool,
    /// When the link stops working. RFC 3339 timestamp in UTC.
    pub expires_at: String,
    /// Lifetime of the browser session the link turns into, in seconds,
    /// counted from sign-in.
    pub session_ttl_secs: u64,
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

/// An agent or a person known to the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalInfo {
    pub id: String,
    /// An agent's name, or a person's email address, lowercased.
    pub handle: String,
    /// [`ActorKind::Person`] for a person, [`ActorKind::Agent`] for an
    /// agent.
    pub kind: ActorKind,
    /// RFC 3339 timestamp in UTC.
    pub created_at: String,
    /// Credentials that are neither revoked nor expired: an agent's tokens,
    /// a person's browser sessions.
    pub active_credentials: u64,
    /// Invites, or for a person sign-in links, that can still be used.
    pub pending_invites: u64,
}

/// An invite or a sign-in link that can still be used. Never holds its
/// code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInvite {
    pub id: String,
    /// Handle of the principal the invite enrols.
    pub principal: String,
    /// [`ActorKind::Person`] for a person's sign-in link,
    /// [`ActorKind::Agent`] for an agent's invite.
    pub kind: ActorKind,
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
            return Err(OperatorError::Storage(crate::store::unsupported_schema(
                &conn, other,
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
            trusted: false,
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
        trusted: network(identity::META_TRUST)?.as_deref() == Some(identity::TRUST_AUTHORITIES),
    })
}

/// Opens for writing and requires a network board that has started once.
/// Returns the connection with the board's recorded URL and, for a board
/// with a self-signed certificate, its fingerprint.
fn open_network(data_dir: &Path) -> Result<(Connection, String, Option<String>)> {
    let (conn, schema) = open(data_dir, true)?;
    match info_of(&conn, &schema, data_dir)? {
        BoardInfo {
            mode: Mode::Network,
            url: Some(url),
            fingerprint,
            trusted,
            // One or the other: a pinned board has started once it has a
            // fingerprint, and a trusted board never has one.
        } if fingerprint.is_some() != trusted => Ok((conn, url, fingerprint)),
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
/// the directory in network mode, and with [`OperatorError::InvalidName`]
/// for an email address: a person is invited with
/// [`create_person_invite`], or with [`invite`], which takes either.
pub fn create_invite(
    data_dir: &Path,
    name: &str,
    options: &InviteOptions,
) -> Result<CreatedInvite> {
    names::validate_handle(name).map_err(OperatorError::InvalidName)?;
    let token_ttl = options.token_ttl_secs.unwrap_or(DEFAULT_TOKEN_TTL_SECS);
    let (created, url, fingerprint) =
        new_invite(data_dir, name, InviteKind::Agent, options, token_ttl)?;
    Ok(CreatedInvite {
        invite: Invite {
            url,
            fingerprint,
            code: created.code,
        },
        id: created.row.id,
        principal: created.row.handle,
        new_principal: created.new_principal,
        expires_at: rfc3339_from_ms(created.row.expires_ms),
    })
}

/// Creates a single-use sign-in link for the person with the email address
/// `email`.
///
/// A new address creates the person, a principal of kind
/// [`ActorKind::Person`] whose handle is the address lowercased. A known
/// address gets one more link for the same person: for a second browser,
/// or after a link or a session has expired. No email is sent; handing the
/// link over is the verification.
///
/// The address is checked loosely. It is at most 254 bytes of ASCII
/// letters, digits and `.`, `_`, `+`, `-` around exactly one `@`, and each
/// of those marks stands between two letters or digits, so that the handle
/// can always be mentioned as `@` and the address.
///
/// The link works once, at the board's sign-in page, for
/// [`InviteOptions::ttl_secs`]. The browser session it turns into lasts
/// [`InviteOptions::token_ttl_secs`], or
/// [`DEFAULT_SESSION_TTL_SECS`] when that is `None`.
///
/// Fails with [`OperatorError::NotNetwork`] until a server has started on
/// the directory in network mode: a local board needs no sign-in.
pub fn create_person_invite(
    data_dir: &Path,
    email: &str,
    options: &InviteOptions,
) -> Result<PersonInvite> {
    let handle = names::validate_email(email).map_err(OperatorError::InvalidName)?;
    let session_ttl_secs = options.token_ttl_secs.unwrap_or(DEFAULT_SESSION_TTL_SECS);
    let (created, url, _fingerprint) = new_invite(
        data_dir,
        &handle,
        InviteKind::Person,
        options,
        session_ttl_secs,
    )?;
    Ok(PersonInvite {
        link: sign_in_link(&url, &created.code),
        code: created.code,
        url,
        id: created.row.id,
        principal: created.row.handle,
        new_principal: created.new_principal,
        expires_at: rfc3339_from_ms(created.row.expires_ms),
        session_ttl_secs,
    })
}

/// Invites an agent or a person, whichever `name` is: a name that contains
/// `@` is a person's email address and gets a sign-in link from
/// [`create_person_invite`], and any other name is an agent's and gets an
/// invite from [`create_invite`].
pub fn invite(data_dir: &Path, name: &str, options: &InviteOptions) -> Result<Invited> {
    if name.contains('@') {
        create_person_invite(data_dir, name, options).map(Invited::Person)
    } else {
        create_invite(data_dir, name, options).map(Invited::Agent)
    }
}

/// Writes the invite. Returns it with the board URL it is for and, for a
/// board with a self-signed certificate, the board's fingerprint.
fn new_invite(
    data_dir: &Path,
    name: &str,
    kind: InviteKind,
    options: &InviteOptions,
    credential_ttl_secs: u64,
) -> Result<(identity::NewInvite, String, Option<String>)> {
    let url_override = match &options.url {
        None => None,
        Some(url) => Some(crate::normalize_public_url(url).map_err(OperatorError::InvalidUrl)?),
    };
    let (mut conn, recorded_url, fingerprint) = open_network(data_dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let created = identity::create_invite(
        &tx,
        name,
        kind,
        ttl_ms(options.ttl_secs),
        ttl_ms(credential_ttl_secs),
        now_ms(),
    )
    .map_err(OperatorError::Storage)?;
    tx.commit()?;
    Ok((created, url_override.unwrap_or(recorded_url), fingerprint))
}

fn ttl_ms(secs: u64) -> i64 {
    // Capped at a thousand years, far below where adding it to the clock
    // could overflow.
    const MAX_SECS: u64 = 1000 * 366 * 24 * 60 * 60;
    (secs.min(MAX_SECS) * 1000) as i64
}

/// Every person, oldest first: the principals of kind
/// [`ActorKind::Person`]. Empty on a local board.
pub fn list_people(data_dir: &Path) -> Result<Vec<PrincipalInfo>> {
    let mut principals = list_principals(data_dir)?;
    principals.retain(|principal| principal.kind == ActorKind::Person);
    Ok(principals)
}

/// Every agent, oldest first: the principals that are not people. Empty on
/// a local board.
pub fn list_agents(data_dir: &Path) -> Result<Vec<PrincipalInfo>> {
    let mut principals = list_principals(data_dir)?;
    principals.retain(|principal| principal.kind != ActorKind::Person);
    Ok(principals)
}

/// Every principal, agents and people alike, oldest first. Empty on a local
/// board. See [`list_agents`] and [`list_people`].
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

/// Every credential, oldest first, including revoked and expired ones:
/// agents' tokens and people's browser sessions. A credential whose
/// [`CredentialInfo::kind`] is [`ActorKind::Person`] is a session. Empty on
/// a local board.
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

/// Invites and sign-in links that can still be used, oldest first. Empty on
/// a local board.
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
            kind: row.kind,
            created_at: rfc3339_from_ms(row.created_ms),
            expires_at: rfc3339_from_ms(row.expires_ms),
        })
        .collect())
}

/// Revokes one credential by its ID: a token or a browser session. Revoking
/// a credential that is already revoked succeeds and changes nothing.
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

/// Revokes every credential of the principal `name`, an agent's name or a
/// person's email address in any case, and withdraws its pending invites
/// and sign-in links. The principal keeps its handle, and a later
/// [`invite`] for the same name enrols it again.
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

/// The build that last started a server on a data directory. Servers older
/// than 0.1.0 did not record it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedBy {
    /// As `openboard_api::build::VERSION` of that server.
    pub version: String,
    /// As `openboard_api::build::COMMIT` of that server, when it knew.
    pub commit: Option<String>,
    /// When it started. RFC 3339 timestamp in UTC.
    pub at: Option<String>,
}

/// What starting this version of the server on a data directory would do
/// to its database. The result of [`upgrade_check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeCheck {
    /// The schema version the database is at.
    pub schema_version: i64,
    /// The schema version this version of the server reads and writes.
    pub supported_schema_version: i64,
    /// Who started a server on the directory last, when recorded.
    pub last_started_by: Option<StartedBy>,
    /// What the next start would do.
    pub action: UpgradeAction,
    /// The directory the backup would be written to: the data directory.
    pub backup_dir: PathBuf,
    /// The backup's file name, with `<UTC time>` where the time goes. Set
    /// when [`UpgradeCheck::action`] is [`UpgradeAction::Migrate`].
    pub backup_name: Option<String>,
    /// Size of the database with its write-ahead log, in bytes. The backup
    /// is at most this large.
    pub database_bytes: u64,
    /// Free space in the data directory's file system, in bytes, when the
    /// system would say.
    pub free_bytes: Option<u64>,
    /// Whether the backup fits, with a margin. `None` when the free space
    /// is not known.
    pub room_for_backup: Option<bool>,
}

/// What the next start of this version of the server does to a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeAction {
    /// The database is at this version's schema. Nothing is migrated and no
    /// backup is written.
    None,
    /// The database is older. The server writes a backup, then migrates.
    Migrate { from: i64, to: i64 },
    /// The database is newer than this version supports. The server refuses
    /// to start on it.
    Refuse,
}

/// One backup the server wrote before a migration. See [`list_backups`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    pub path: PathBuf,
    pub file_name: String,
    pub size_bytes: u64,
    /// When the file was last written. RFC 3339 timestamp in UTC.
    pub modified_at: Option<String>,
    /// How long ago that was, in seconds.
    pub age_secs: Option<u64>,
    /// The schema version the migration went to, from the file name. The
    /// backup holds the database as it was before, at an older version.
    pub before_schema_version: Option<i64>,
}

fn database_path(data_dir: &Path) -> Result<PathBuf> {
    let path = data_dir.join(DB_FILE_NAME);
    if path.is_file() {
        Ok(path)
    } else {
        Err(OperatorError::NoDatabase(path))
    }
}

/// The schema version of the database and who last started a server on it,
/// whatever version wrote it.
fn read_schema(data_dir: &Path) -> Result<(i64, Option<StartedBy>)> {
    let path = database_path(data_dir)?;
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let started = crate::store::started_by(&conn).map(|by| StartedBy {
        version: by.version,
        commit: by.commit,
        at: by.at,
    });
    Ok((version, started))
}

/// Free space where `dir` is, as `df` reports it to an unprivileged user.
/// `None` where there is no `df` or its answer cannot be read.
fn free_bytes(dir: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        // POSIX output: a header, then one line whose fourth column is the
        // available space in 1024-byte blocks.
        let output = std::process::Command::new("df")
            .arg("-Pk")
            .arg(dir)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let line = text.lines().nth(1)?;
        // Counted from the end: the first column, a device name, may hold
        // spaces. The last is the mount point, which may too, so the
        // columns are found between them.
        let numbers: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .map_while(|column| column.trim_end_matches('%').parse().ok())
            .collect();
        numbers.get(2)?.checked_mul(1024)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// Reports what the next start of this version of the server would do to
/// the database in `data_dir`: nothing, a migration with its backup, or a
/// refusal because the database is newer. Read-only; it works beside a
/// running server and on a directory any version wrote.
pub fn upgrade_check(data_dir: &Path) -> Result<UpgradeCheck> {
    let (schema_version, last_started_by) = read_schema(data_dir)?;
    let action = match schema_version {
        SCHEMA_VERSION => UpgradeAction::None,
        // A file SQLite has not written a schema to yet.
        0 => UpgradeAction::None,
        older if (1..SCHEMA_VERSION).contains(&older) => UpgradeAction::Migrate {
            from: older,
            to: SCHEMA_VERSION,
        },
        _ => UpgradeAction::Refuse,
    };
    let size = |name: String| std::fs::metadata(data_dir.join(name)).map_or(0, |found| found.len());
    let database_bytes = size(DB_FILE_NAME.to_owned()) + size(format!("{DB_FILE_NAME}-wal"));
    let free_bytes = free_bytes(data_dir);
    // The copy is never larger than the database; the margin is for the
    // migration itself, which writes to the log.
    let needed = database_bytes
        .saturating_add(database_bytes / 10)
        .saturating_add(1024 * 1024);
    Ok(UpgradeCheck {
        schema_version,
        supported_schema_version: SCHEMA_VERSION,
        last_started_by,
        action,
        backup_dir: data_dir.to_path_buf(),
        backup_name: matches!(action, UpgradeAction::Migrate { .. })
            .then(|| crate::store::backup_pattern(SCHEMA_VERSION)),
        database_bytes,
        free_bytes,
        room_for_backup: free_bytes.map(|free| free >= needed),
    })
}

fn backup_files(data_dir: &Path) -> Result<Vec<Backup>> {
    let unreadable = |error: std::io::Error| {
        OperatorError::Storage(format!("cannot read {}: {error}", data_dir.display()))
    };
    let mut backups = Vec::new();
    for entry in std::fs::read_dir(data_dir).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(rest) = file_name.strip_prefix(BACKUP_PREFIX) else {
            continue;
        };
        // Never a link or a directory: pruning removes what is listed.
        let Ok(found) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !found.is_file() {
            continue;
        }
        let modified_ms = found
            .modified()
            .ok()
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|since| i64::try_from(since.as_millis()).ok());
        backups.push(Backup {
            path: entry.path(),
            size_bytes: found.len(),
            modified_at: modified_ms.map(rfc3339_from_ms),
            age_secs: modified_ms
                .map(|at| u64::try_from(now_ms().saturating_sub(at) / 1000).unwrap_or(0)),
            before_schema_version: rest.split('-').next().and_then(|n| n.parse().ok()),
            file_name,
        });
    }
    backups.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(backups)
}

/// The backups in `data_dir` that servers wrote before migrating its
/// database, by file name, which is oldest first for one schema version.
///
/// A backup is a whole copy of the database from before an upgrade. It
/// keeps everything the board held then, including the text of messages
/// that were deleted or edited afterwards, so it should not be kept longer
/// than it takes to trust the upgrade; see [`prune_backups`].
pub fn list_backups(data_dir: &Path) -> Result<Vec<Backup>> {
    database_path(data_dir)?;
    backup_files(data_dir)
}

/// Deletes every backup [`list_backups`] finds and returns what it
/// deleted. The database itself is never touched. Fails on the first file
/// that cannot be removed, having removed the ones before it.
pub fn prune_backups(data_dir: &Path) -> Result<Vec<Backup>> {
    let backups = list_backups(data_dir)?;
    for backup in &backups {
        std::fs::remove_file(&backup.path).map_err(|error| {
            OperatorError::Storage(format!("cannot delete {}: {error}", backup.path.display()))
        })?;
    }
    Ok(backups)
}

/// One grant, as the operator commands show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantInfo {
    pub id: String,
    pub board: String,
    /// `*`, `build/` or `build/api`.
    pub pattern: String,
    /// The handle of a principal, a group (`agents`, `people`, `services`)
    /// or `everyone`.
    pub grantee: String,
    pub verbs: String,
    pub expires_at: Option<String>,
    /// Expired: it no longer applies.
    pub expired: bool,
    /// The handle that made it, or `operator`.
    pub created_by: String,
    pub created_at: String,
}

/// A principal's access to one channel, and the grants that decide it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explained {
    pub verbs: String,
    /// Empty when no grant applies, which means no access.
    pub deciding: Vec<GrantInfo>,
}

fn open_grants(data_dir: &Path, write: bool) -> Result<Connection> {
    let (conn, _) = open(data_dir, write)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        return Err(OperatorError::NotMigrated);
    }
    Ok(conn)
}

fn grant_info(stored: &crate::store::grants::StoredGrant, now: i64) -> GrantInfo {
    use crate::grants::Grantee;
    let grant = &stored.grant;
    GrantInfo {
        id: grant.id.clone(),
        board: stored.board.clone(),
        pattern: grant.pattern.to_string(),
        grantee: match &grant.grantee {
            Grantee::Principal(id) => stored.grantee_handle.clone().unwrap_or_else(|| id.clone()),
            Grantee::Group(group) => group.name().to_owned(),
            Grantee::Everyone => "everyone".to_owned(),
        },
        verbs: grant.verbs.to_string(),
        expires_at: grant.expires_at.map(rfc3339_from_ms),
        expired: !grant.is_active(now),
        created_by: stored.created_by.clone(),
        created_at: rfc3339_from_ms(stored.created_ms),
    }
}

fn grantee_of(conn: &Connection, name: &str) -> Result<crate::grants::Grantee> {
    use crate::grants::{Grantee, Group};
    if name == "everyone" {
        return Ok(Grantee::Everyone);
    }
    if let Some(group) = Group::parse(name) {
        return Ok(Grantee::Group(group));
    }
    match crate::store::grants::principal_by_handle(conn, name)? {
        Some((id, _, _)) => Ok(Grantee::Principal(id)),
        None => Err(OperatorError::NoSuchPrincipal(name.to_owned())),
    }
}

/// Gives `grantee` (a handle, `agents`, `people`, `services` or `everyone`)
/// `verbs` on `pattern` of `board`, optionally for `ttl`. The operator's
/// grants need no authorizing grant: whoever can write the data directory
/// already controls the board. Creates the board when it does not exist.
pub fn grant(
    data_dir: &Path,
    board: &str,
    pattern: &str,
    grantee: &str,
    verbs: &str,
    ttl: Option<Duration>,
) -> Result<GrantInfo> {
    use crate::grants::{Grant, Pattern, Verbs};
    names::validate_board(board).map_err(OperatorError::InvalidName)?;
    let pattern = Pattern::parse(pattern, names::validate_channel)
        .map_err(|error| OperatorError::InvalidGrant(error.to_string()))?;
    let verbs =
        Verbs::parse(verbs).map_err(|error| OperatorError::InvalidGrant(error.to_string()))?;
    let mut conn = open_grants(data_dir, true)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let grantee = grantee_of(&tx, grantee)?;
    let now = now_ms();
    let expires_at =
        ttl.map(|ttl| now.saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)));
    Grant {
        id: String::new(),
        pattern: pattern.clone(),
        grantee: grantee.clone(),
        verbs,
        expires_at,
    }
    .validate()
    .map_err(|error| OperatorError::InvalidGrant(error.to_string()))?;
    let board_id = crate::store::ensure_board_for_operator(&tx, board)
        .map_err(|error| OperatorError::Storage(error.to_string()))?;
    let id = crate::store::grants::insert(
        &tx,
        board_id,
        &pattern,
        &grantee,
        verbs,
        expires_at,
        crate::store::grants::OPERATOR,
    )?;
    let stored = crate::store::grants::list(&tx, Some(board_id))?
        .into_iter()
        .find(|stored| stored.grant.id == id)
        .ok_or_else(|| OperatorError::Storage("the new grant vanished".to_owned()))?;
    tx.commit()?;
    Ok(grant_info(&stored, now))
}

/// Removes a grant. Removing one can widen access: removing a grant to
/// `everyone` reopens what it closed to the broader grants.
pub fn ungrant(data_dir: &Path, id: &str) -> Result<GrantInfo> {
    let mut conn = open_grants(data_dir, true)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = now_ms();
    let Some(stored) = crate::store::grants::list(&tx, None)?
        .into_iter()
        .find(|stored| stored.grant.id == id)
    else {
        return Err(OperatorError::NoSuchGrant(id.to_owned()));
    };
    let board_id: i64 = tx.query_row("SELECT board_id FROM grants WHERE id = ?1", [id], |row| {
        row.get(0)
    })?;
    crate::store::grants::remove(&tx, board_id, id, crate::store::grants::OPERATOR)?;
    tx.commit()?;
    Ok(grant_info(&stored, now))
}

/// The grants of one board, or of every board, oldest first.
pub fn list_grants(data_dir: &Path, board: Option<&str>) -> Result<Vec<GrantInfo>> {
    let conn = open_grants(data_dir, false)?;
    let now = now_ms();
    let key = board.map(names::board_key);
    Ok(crate::store::grants::list(&conn, None)?
        .iter()
        .filter(|stored| {
            key.as_ref()
                .is_none_or(|key| names::board_key(&stored.board) == *key)
        })
        .map(|stored| grant_info(stored, now))
        .collect())
}

/// What the principal named `handle` may do in `channel` of `board` by its
/// grants, and which grants decide it.
pub fn explain(data_dir: &Path, board: &str, handle: &str, channel: &str) -> Result<Explained> {
    names::validate_channel(channel).map_err(OperatorError::InvalidName)?;
    let conn = open_grants(data_dir, false)?;
    let Some((id, kind, _)) = crate::store::grants::principal_by_handle(&conn, handle)? else {
        return Err(OperatorError::NoSuchPrincipal(handle.to_owned()));
    };
    let now = now_ms();
    let key = names::board_key(board);
    let stored: Vec<_> = crate::store::grants::list(&conn, None)?
        .into_iter()
        .filter(|stored| names::board_key(&stored.board) == key)
        .collect();
    let grants: Vec<_> = stored.iter().map(|stored| stored.grant.clone()).collect();
    let access =
        crate::grants::evaluate(&grants, &crate::grants::Subject { id, kind }, channel, now);
    Ok(Explained {
        verbs: access.verbs.to_string(),
        deciding: stored
            .iter()
            .filter(|stored| access.deciding.contains(&stored.grant.id))
            .map(|stored| grant_info(stored, now))
            .collect(),
    })
}

/// The instance's access mode: whether any enrolled agent or person may
/// create boards, and new boards admit all agents and people.
pub fn access(data_dir: &Path) -> Result<crate::store::Access> {
    let conn = open_grants(data_dir, false)?;
    Ok(crate::store::grants::access(&conn)?)
}

/// Sets the instance's access mode. Closing it changes no existing grant:
/// it only stops new boards admitting everyone, and principals creating
/// boards.
pub fn set_access(data_dir: &Path, access: crate::store::Access) -> Result<()> {
    let conn = open_grants(data_dir, true)?;
    crate::store::grants::set_access(&conn, access)?;
    Ok(())
}
