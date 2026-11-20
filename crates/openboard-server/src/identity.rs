//! Principals, credentials, invites and the security log.
//!
//! Everything here is plain SQL on a connection the caller owns, so the
//! running server and the operator functions, which use a second connection,
//! share one implementation. Callers wrap mutations in a transaction.
//!
//! Secrets never reach the database: a token, a browser session's cookie
//! value, an enrolment code and a person's sign-in code are stored only as
//! SHA-256 digests and are looked up by digest, so no secret is ever
//! compared byte by byte.
//!
//! An agent's token and a person's browser session are both rows of
//! `credentials`, and an agent's enrolment code and a person's sign-in code
//! are both rows of `invites`. What tells them apart is the kind of the
//! principal they belong to and the prefix of the secret, which is part of
//! what is hashed: a token cannot be presented as a session cookie or the
//! other way round, and neither kind of code can be spent at the other's
//! endpoint.

use std::net::IpAddr;

use openboard_api::auth::{CredentialInfo, TOKEN_PREFIX};
use openboard_api::web::WEB_CODE_PREFIX;
use openboard_api::ActorKind;
use rusqlite::{params, Connection, OptionalExtension, Row};
use sha2::{Digest, Sha256};

use crate::authz::Principal;
use crate::chain::hex;
use crate::names::handle_key;
use crate::time::{now_rfc3339, rfc3339_from_ms};

/// Random bytes in a token and in an enrolment code.
const SECRET_BYTES: usize = 32;
/// The longest token the server will hash. Anything longer is malformed.
const MAX_TOKEN_HEX: usize = 128;

/// Prefix of a session cookie's value, so secret scanners can recognise
/// one and so that it can never be taken for a bearer token.
pub(crate) const SESSION_PREFIX: &str = "obs_";
/// Domain separation of the anti-CSRF token derived from a session secret.
const CSRF_DOMAIN: &[u8] = b"openboard.csrf.v1\n";

/// `meta` keys.
pub(crate) const META_MODE: &str = "mode";
pub(crate) const META_URL: &str = "public_url";
/// The pin of the board's self-signed certificate. Absent on a board that
/// serves a supplied certificate.
pub(crate) const META_FINGERPRINT: &str = "fingerprint";
/// How clients come to trust the board's certificate. Absent when they pin
/// a self-signed one; [`TRUST_AUTHORITIES`] once a supplied certificate was
/// served, after which the board never serves a self-signed one again.
pub(crate) const META_TRUST: &str = "tls_trust";
pub(crate) const TRUST_AUTHORITIES: &str = "authorities";
/// The version, commit and time of the server that last started on the
/// directory. Written on every start; a server older than 0.1.0 writes
/// none of them and reads none of them.
pub(crate) const META_STARTED_VERSION: &str = "last_started_version";
pub(crate) const META_STARTED_COMMIT: &str = "last_started_commit";
pub(crate) const META_STARTED_AT: &str = "last_started_at";

/// Security log entry types.
pub mod log_kinds {
    /// A server started on the data directory. The detail holds the mode.
    pub const SERVER_STARTED: &str = "server.started";
    /// A local data directory became a network one.
    pub const MODE_UPGRADED: &str = "mode.upgraded";
    /// The data directory was plainly a network one, by its certificate or
    /// its agents, while its recorded mode was missing or said local. The
    /// record was put right and the board served as a network one.
    pub const MODE_REPAIRED: &str = "mode.repaired";
    /// The board began serving a certificate its operator supplied.
    pub const CERTIFICATE_SUPPLIED: &str = "certificate.supplied";
    pub const INVITE_CREATED: &str = "invite.created";
    /// A pending invite was withdrawn when its principal was revoked.
    pub const INVITE_REVOKED: &str = "invite.revoked";
    pub const ENROL_SUCCEEDED: &str = "enrol.succeeded";
    pub const ENROL_FAILED: &str = "enrol.failed";
    /// A person swapped a sign-in code for a browser session. The entry
    /// names the session's credential.
    pub const WEB_LOGIN_SUCCEEDED: &str = "web.login.succeeded";
    pub const WEB_LOGIN_FAILED: &str = "web.login.failed";
    pub const CREDENTIAL_REVOKED: &str = "credential.revoked";
    /// A derived token was minted. The detail holds its board and scope.
    pub const TOKEN_DERIVED: &str = "token.derived";
    pub const LOGOUT: &str = "logout";
    pub const AUTH_FAILED: &str = "auth.failed";
    /// A client address or a credential ran into a rate limit. Written once
    /// per address or credential per window.
    pub const RATE_LIMITED: &str = "rate.limited";
    /// Failure entries were dropped for the rest of the minute because the
    /// log's budget for them was spent.
    pub const LOG_SUPPRESSED: &str = "log.suppressed";
    /// Old entries were removed under the retention settings. The detail
    /// holds how many.
    pub const LOG_PRUNED: &str = "log.pruned";
}

/// `n` bytes from the operating system's random number generator, in hex.
pub(crate) fn random_hex(n: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("the system random number generator failed: {error}"))?;
    Ok(hex(&bytes))
}

pub(crate) fn sha256_hex(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

fn is_lower_hex(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn secret_lookup_hash(presented: &str, prefix: &str) -> Option<String> {
    let secret = presented.strip_prefix(prefix)?;
    let well_formed =
        (SECRET_BYTES * 2..=MAX_TOKEN_HEX).contains(&secret.len()) && is_lower_hex(secret);
    well_formed.then(|| sha256_hex(presented))
}

/// The digest to look a presented token up by, or `None` when the text
/// cannot be a token, in which case nothing is looked up.
pub(crate) fn token_lookup_hash(presented: &str) -> Option<String> {
    secret_lookup_hash(presented, TOKEN_PREFIX)
}

/// The digest to look a presented session cookie value up by, or `None`
/// when the text cannot be one. The prefix differs from a token's, so a
/// token is never accepted from the cookie, nor a session from the
/// `Authorization` header.
pub(crate) fn session_lookup_hash(presented: &str) -> Option<String> {
    secret_lookup_hash(presented, SESSION_PREFIX)
}

/// The anti-CSRF token of the session whose cookie value is `session`. It
/// is derived, so nothing more is stored, and it cannot be turned back into
/// the cookie value.
pub(crate) fn csrf_token(session: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CSRF_DOMAIN);
    hasher.update(session.as_bytes());
    hex(&hasher.finalize())
}

/// Compares two strings in time that depends only on their lengths.
pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b) {
        difference |= x ^ y;
    }
    std::hint::black_box(difference) == 0
}

/// The client address as the security log records it: the last octet of an
/// IPv4 address and the last 80 bits of an IPv6 address are zeroed.
pub(crate) fn truncate_ip(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            std::net::Ipv4Addr::new(a, b, c, 0).to_string()
        }
        IpAddr::V6(v6) => {
            let [a, b, c, ..] = v6.segments();
            std::net::Ipv6Addr::new(a, b, c, 0, 0, 0, 0, 0).to_string()
        }
    }
}

pub(crate) fn parse_actor_kind(name: &str) -> Option<ActorKind> {
    Some(match name {
        "local" => ActorKind::Local,
        "person" => ActorKind::Person,
        "service" => ActorKind::Service,
        "agent" => ActorKind::Agent,
        "bridge" => ActorKind::Bridge,
        _ => return None,
    })
}

pub(crate) fn actor_kind_name(kind: ActorKind) -> &'static str {
    match kind {
        ActorKind::Local => "local",
        ActorKind::Person => "person",
        ActorKind::Service => "service",
        ActorKind::Agent => "agent",
        ActorKind::Bridge => "bridge",
    }
}

fn kind_from_row(row: &Row<'_>, at: usize) -> rusqlite::Result<ActorKind> {
    let kind: String = row.get(at)?;
    parse_actor_kind(&kind).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            at,
            rusqlite::types::Type::Text,
            "unknown principal kind".into(),
        )
    })
}

// ---------------------------------------------------------------------------
// Meta
// ---------------------------------------------------------------------------

pub(crate) fn meta_get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .optional()
}

pub(crate) fn meta_set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub(crate) fn meta_delete(conn: &Connection, key: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Security log
// ---------------------------------------------------------------------------

/// One security log entry to append. Never holds a secret.
#[derive(Debug, Clone, Default)]
pub(crate) struct LogEntry<'a> {
    pub kind: &'a str,
    /// The principal's handle.
    pub principal: Option<&'a str>,
    pub credential_id: Option<&'a str>,
    pub client: Option<IpAddr>,
    pub detail: Option<&'a str>,
}

pub(crate) fn log(conn: &Connection, entry: &LogEntry<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO security_log (at, kind, principal, credential_id, client, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            now_rfc3339(),
            entry.kind,
            entry.principal,
            entry.credential_id,
            entry.client.map(truncate_ip),
            entry.detail
        ],
    )?;
    Ok(())
}

const LOG_DELETE_TRIGGER: &str = "security_log_is_append_only_delete";

/// Removes security log entries written before `cutoff`, an RFC 3339
/// timestamp as the log stores them, and the oldest entries beyond the
/// newest `max_entries`. When anything was removed, one
/// [`log_kinds::LOG_PRUNED`] entry records how many. Returns that number.
///
/// The log is append-only: a trigger refuses every `DELETE`. This function
/// is the one exception, and it is deliberate about it. It drops that
/// trigger, deletes, and creates the trigger again exactly as the schema
/// defines it. The caller runs it in one write transaction, so no other
/// connection ever sees the log without its trigger, and a failure part-way
/// rolls everything back.
pub(crate) fn prune_log(
    conn: &Connection,
    cutoff: &str,
    max_entries: u64,
) -> rusqlite::Result<u64> {
    let keep = i64::try_from(max_entries).unwrap_or(i64::MAX);
    // The newest entry that is beyond the count, if the log is that long.
    let overflow: Option<i64> = conn
        .query_row(
            "SELECT id FROM security_log ORDER BY id DESC LIMIT 1 OFFSET ?1",
            [keep],
            |row| row.get(0),
        )
        .optional()?;
    let overflow = overflow.unwrap_or(0);
    let doomed: i64 = conn.query_row(
        "SELECT count(*) FROM security_log WHERE at < ?1 OR id <= ?2",
        params![cutoff, overflow],
        |row| row.get(0),
    )?;
    if doomed == 0 {
        return Ok(0);
    }
    let definition: String = conn.query_row(
        "SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND name = ?1",
        [LOG_DELETE_TRIGGER],
        |row| row.get(0),
    )?;
    conn.execute_batch(&format!("DROP TRIGGER {LOG_DELETE_TRIGGER}"))?;
    let removed = conn.execute(
        "DELETE FROM security_log WHERE at < ?1 OR id <= ?2",
        params![cutoff, overflow],
    )?;
    conn.execute_batch(&definition)?;
    log(
        conn,
        &LogEntry {
            kind: log_kinds::LOG_PRUNED,
            detail: Some(&format!("{removed} entries removed by retention")),
            ..LogEntry::default()
        },
    )?;
    Ok(removed as u64)
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// A credential and the principal it belongs to. Holds no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credential {
    pub id: String,
    pub principal_id: String,
    pub handle: String,
    pub kind: ActorKind,
    pub created_ms: i64,
    pub expires_ms: i64,
    pub revoked_ms: Option<i64>,
    pub last_used_ms: Option<i64>,
    /// The credential this one was derived from (A9).
    pub parent_id: Option<String>,
    /// The profile a derived token is bound to.
    pub profile: Option<String>,
    /// The scopes of this credential and every credential above it, as
    /// (board, `pattern=verbs` entries separated by spaces). Filled in by
    /// [`authenticate`]; empty from the other lookups.
    pub scopes: Vec<(String, String)>,
}

impl Credential {
    /// Made with `openboard agent token` from another credential.
    pub(crate) fn derived(&self) -> bool {
        self.parent_id.is_some()
    }

    pub(crate) fn principal(&self) -> Principal {
        Principal {
            id: self.principal_id.clone(),
            kind: self.kind,
            handle: self.handle.clone(),
        }
    }

    pub(crate) fn info(&self) -> CredentialInfo {
        CredentialInfo {
            id: self.id.clone(),
            principal: self.handle.clone(),
            kind: self.kind,
            created_at: rfc3339_from_ms(self.created_ms),
            expires_at: rfc3339_from_ms(self.expires_ms),
            revoked_at: self.revoked_ms.map(rfc3339_from_ms),
            last_used_at: self.last_used_ms.map(rfc3339_from_ms),
            parent: self.parent_id.clone(),
            profile: self.profile.clone(),
        }
    }

    pub(crate) fn usable_at(&self, now_ms: i64) -> bool {
        self.revoked_ms.is_none() && now_ms < self.expires_ms
    }
}

/// The credential a secret's digest belongs to, whatever state it is in.
/// Unlike [`authenticate`] this changes nothing.
pub(crate) fn find_credential_by_hash(
    conn: &Connection,
    token_hash: &str,
) -> rusqlite::Result<Option<Credential>> {
    let columns = credential_columns(conn)?;
    conn.query_row(
        &format!("SELECT {columns} FROM {CREDENTIAL_FROM} WHERE c.token_hash = ?1"),
        [token_hash],
        credential_from_row,
    )
    .optional()
}

const CREDENTIAL_COLUMNS: &str = "c.id, c.principal_id, p.handle, p.kind, c.created_ms, \
                                  c.expires_ms, c.revoked_ms, c.last_used_ms, c.parent_id, \
                                  c.profile";

/// The same columns from a database older than schema 5, which the offline
/// operator tools read without migrating: no credential is derived there.
const CREDENTIAL_COLUMNS_V4: &str = "c.id, c.principal_id, p.handle, p.kind, c.created_ms, \
                                     c.expires_ms, c.revoked_ms, c.last_used_ms, NULL, NULL";

/// The credential columns this database has.
fn credential_columns(conn: &Connection) -> rusqlite::Result<&'static str> {
    let derived: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('credentials') WHERE name = 'parent_id')",
        [],
        |row| row.get(0),
    )?;
    Ok(if derived {
        CREDENTIAL_COLUMNS
    } else {
        CREDENTIAL_COLUMNS_V4
    })
}
const CREDENTIAL_FROM: &str = "credentials c JOIN principals p ON p.id = c.principal_id";

fn credential_from_row(row: &Row<'_>) -> rusqlite::Result<Credential> {
    Ok(Credential {
        id: row.get(0)?,
        principal_id: row.get(1)?,
        handle: row.get(2)?,
        kind: kind_from_row(row, 3)?,
        created_ms: row.get(4)?,
        expires_ms: row.get(5)?,
        revoked_ms: row.get(6)?,
        last_used_ms: row.get(7)?,
        parent_id: row.get(8)?,
        profile: row.get(9)?,
        scopes: Vec::new(),
    })
}

/// The deepest a chain of derived tokens goes.
pub(crate) const MAX_TOKEN_DEPTH: i64 = 8;

/// A credential and every credential above it, nearest first.
struct Link {
    revoked_ms: Option<i64>,
    expires_ms: i64,
    board: Option<String>,
    scope: Option<String>,
}

fn chain(conn: &Connection, credential_id: &str) -> rusqlite::Result<Vec<Link>> {
    conn.prepare_cached(
        "WITH RECURSIVE chain(id, parent_id, revoked_ms, expires_ms, board, scope, depth) AS (
             SELECT id, parent_id, revoked_ms, expires_ms, board, scope, 0
               FROM credentials WHERE id = ?1
             UNION ALL
             SELECT c.id, c.parent_id, c.revoked_ms, c.expires_ms, c.board, c.scope,
                    chain.depth + 1
               FROM credentials c JOIN chain ON c.id = chain.parent_id
              WHERE chain.depth < ?2)
         SELECT revoked_ms, expires_ms, board, scope FROM chain ORDER BY depth",
    )?
    .query_map(params![credential_id, MAX_TOKEN_DEPTH + 1], |row| {
        Ok(Link {
            revoked_ms: row.get(0)?,
            expires_ms: row.get(1)?,
            board: row.get(2)?,
            scope: row.get(3)?,
        })
    })?
    .collect()
}

/// What a presented token resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthOutcome {
    Valid(Credential),
    Unknown,
    Expired(Credential),
    Revoked(Credential),
}

/// Resolves a token digest to its credential and principal. A valid
/// credential's last-used time is refreshed when it is at least
/// `last_used_interval_ms` old, so that reads do not all become writes.
pub(crate) fn authenticate(
    conn: &Connection,
    token_hash: &str,
    now_ms: i64,
    last_used_interval_ms: i64,
) -> rusqlite::Result<AuthOutcome> {
    let found = conn
        .query_row(
            &format!("SELECT {CREDENTIAL_COLUMNS} FROM {CREDENTIAL_FROM} WHERE c.token_hash = ?1"),
            [token_hash],
            credential_from_row,
        )
        .optional()?;
    let Some(credential) = found else {
        return Ok(AuthOutcome::Unknown);
    };
    let mut credential = credential;
    let links = if credential.derived() {
        chain(conn, &credential.id)?
    } else {
        Vec::new()
    };
    // A derived token is only as alive as every token above it.
    if credential.revoked_ms.is_some() || links.iter().any(|link| link.revoked_ms.is_some()) {
        return Ok(AuthOutcome::Revoked(credential));
    }
    if now_ms >= credential.expires_ms || links.iter().any(|link| now_ms >= link.expires_ms) {
        return Ok(AuthOutcome::Expired(credential));
    }
    credential.scopes = links
        .into_iter()
        .filter_map(|link| Some((link.board?, link.scope?)))
        .collect();
    let stale = credential
        .last_used_ms
        .is_none_or(|last| now_ms.saturating_sub(last) >= last_used_interval_ms);
    if stale {
        conn.execute(
            "UPDATE credentials SET last_used_ms = ?1 WHERE id = ?2",
            params![now_ms, credential.id],
        )?;
    }
    Ok(AuthOutcome::Valid(credential))
}

/// Whether the credential exists, is not revoked and has not expired. Read
/// from the database every time, because another process may have revoked it.
pub(crate) fn credential_usable(
    conn: &Connection,
    credential_id: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let links = chain(conn, credential_id)?;
    Ok(!links.is_empty()
        && links
            .iter()
            .all(|link| link.revoked_ms.is_none() && now_ms < link.expires_ms))
}

/// The derived tokens `caller` may see and revoke, live ones only: every
/// token derived from it, and, for a credential that is not itself derived,
/// every derived token of its principal. With each one's board and scope.
pub(crate) fn derived_tokens(
    conn: &Connection,
    caller: &Credential,
    now_ms: i64,
) -> rusqlite::Result<Vec<(Credential, String, String)>> {
    let sql = format!(
        "WITH RECURSIVE below(id, depth) AS (
             SELECT id, 0 FROM credentials WHERE parent_id = ?1
             UNION ALL
             SELECT c.id, below.depth + 1 FROM credentials c JOIN below ON c.parent_id = below.id
              WHERE below.depth < ?4)
         SELECT {CREDENTIAL_COLUMNS}, c.board, c.scope FROM {CREDENTIAL_FROM}
          WHERE c.parent_id IS NOT NULL AND c.revoked_ms IS NULL AND c.expires_ms > ?3
            AND (c.id IN (SELECT id FROM below) OR (?5 AND c.principal_id = ?2))
          ORDER BY c.created_ms, c.id"
    );
    let rows: Vec<(Credential, String, String)> = conn
        .prepare(&sql)?
        .query_map(
            params![
                caller.id,
                caller.principal_id,
                now_ms,
                MAX_TOKEN_DEPTH + 1,
                !caller.derived()
            ],
            |row| {
                Ok((
                    credential_from_row(row)?,
                    row.get::<_, Option<String>>(10)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(11)?.unwrap_or_default(),
                ))
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    // Live means every token above is live too.
    let mut live = Vec::new();
    for row in rows {
        if credential_usable(conn, &row.0.id, now_ms)? {
            live.push(row);
        }
    }
    Ok(live)
}

/// What a derived token is limited to.
pub(crate) struct Derivation<'a> {
    pub board: &'a str,
    /// `pattern=verbs` entries, already validated.
    pub scope: &'a [String],
    pub ttl_ms: i64,
    pub profile: Option<&'a str>,
}

/// Mints a token derived from `parent` (A9): the same principal, its own
/// credential ID, the scope given, and an expiry no later than the
/// parent's. `None` when the chain would be deeper than
/// [`MAX_TOKEN_DEPTH`]. The caller runs this inside a write transaction.
pub(crate) fn derive(
    conn: &Connection,
    parent: &Credential,
    derivation: &Derivation<'_>,
    now_ms: i64,
) -> Result<Option<(String, Credential)>, String> {
    let sql = |error: rusqlite::Error| error.to_string();
    let depth = chain(conn, &parent.id).map_err(sql)?.len() as i64;
    if depth >= MAX_TOKEN_DEPTH {
        return Ok(None);
    }
    // Always a bearer token, even from a person's browser session: it is
    // for handing to an agent.
    let token = format!("{TOKEN_PREFIX}{}", random_hex(SECRET_BYTES)?);
    let credential_id = format!("crd_{}", random_hex(8)?);
    conn.execute(
        "INSERT INTO credentials (id, principal_id, token_hash, created_ms, expires_ms,
                                  parent_id, board, scope, profile)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            credential_id,
            parent.principal_id,
            sha256_hex(&token),
            now_ms,
            now_ms
                .saturating_add(derivation.ttl_ms)
                .min(parent.expires_ms),
            parent.id,
            derivation.board,
            derivation.scope.join(" "),
            derivation.profile.or(parent.profile.as_deref()),
        ],
    )
    .map_err(sql)?;
    let credential = find_credential(conn, &credential_id)
        .map_err(sql)?
        .ok_or_else(|| "the new credential vanished".to_owned())?;
    Ok(Some((token, credential)))
}

pub(crate) fn find_credential(
    conn: &Connection,
    credential_id: &str,
) -> rusqlite::Result<Option<Credential>> {
    let columns = credential_columns(conn)?;
    conn.query_row(
        &format!("SELECT {columns} FROM {CREDENTIAL_FROM} WHERE c.id = ?1"),
        [credential_id],
        credential_from_row,
    )
    .optional()
}

pub(crate) fn list_credentials(conn: &Connection) -> rusqlite::Result<Vec<Credential>> {
    let columns = credential_columns(conn)?;
    conn.prepare(&format!(
        "SELECT {columns} FROM {CREDENTIAL_FROM} ORDER BY c.created_ms, c.id"
    ))?
    .query_map([], credential_from_row)?
    .collect()
}

/// Revokes one credential. Returns it as it now stands and whether this call
/// is what revoked it, or `None` when there is no such credential.
pub(crate) fn revoke_credential(
    conn: &Connection,
    credential_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Option<(Credential, bool)>> {
    let changed = conn.execute(
        "UPDATE credentials SET revoked_ms = ?1 WHERE id = ?2 AND revoked_ms IS NULL",
        params![now_ms, credential_id],
    )?;
    Ok(find_credential(conn, credential_id)?.map(|credential| (credential, changed == 1)))
}

// ---------------------------------------------------------------------------
// Principals and invites
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrincipalRow {
    pub id: String,
    pub handle: String,
    pub kind: ActorKind,
    pub created_ms: i64,
    /// Credentials that are neither revoked nor expired.
    pub active_credentials: u64,
    pub pending_invites: u64,
}

pub(crate) fn find_principal(
    conn: &Connection,
    name: &str,
) -> rusqlite::Result<Option<(String, String)>> {
    conn.query_row(
        "SELECT id, handle FROM principals WHERE handle_key = ?1",
        [handle_key(name)],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

pub(crate) fn list_principals(
    conn: &Connection,
    now_ms: i64,
) -> rusqlite::Result<Vec<PrincipalRow>> {
    conn.prepare(
        "SELECT p.id, p.handle, p.kind, p.created_ms,
                (SELECT count(*) FROM credentials c WHERE c.principal_id = p.id
                    AND c.revoked_ms IS NULL AND c.expires_ms > ?1),
                (SELECT count(*) FROM invites i WHERE i.principal_id = p.id
                    AND i.used_ms IS NULL AND i.revoked_ms IS NULL AND i.expires_ms > ?1)
         FROM principals p ORDER BY p.created_ms, p.id",
    )?
    .query_map([now_ms], |row| {
        Ok(PrincipalRow {
            id: row.get(0)?,
            handle: row.get(1)?,
            kind: kind_from_row(row, 2)?,
            created_ms: row.get(3)?,
            active_credentials: row.get::<_, i64>(4)?.max(0) as u64,
            pending_invites: row.get::<_, i64>(5)?.max(0) as u64,
        })
    })?
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InviteRow {
    pub id: String,
    pub handle: String,
    /// The kind of the principal the invite is for. An invite for a person
    /// is a sign-in code.
    pub kind: ActorKind,
    pub created_ms: i64,
    pub expires_ms: i64,
}

/// A freshly created invite. `code` is the only copy of the secret.
pub(crate) struct NewInvite {
    pub row: InviteRow,
    pub code: String,
    /// True when the invite also created its principal.
    pub new_principal: bool,
}

/// Who an invite is for, which decides the shape of its code and the one
/// endpoint that takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InviteKind {
    /// An enrolment code, swapped for a bearer token at `POST /v1/join`.
    Agent,
    /// A sign-in code, swapped for a browser session at
    /// `POST /v1/web/login`.
    Person,
}

impl InviteKind {
    fn actor_kind(self) -> ActorKind {
        match self {
            InviteKind::Agent => ActorKind::Agent,
            InviteKind::Person => ActorKind::Person,
        }
    }

    fn code_prefix(self) -> &'static str {
        match self {
            InviteKind::Agent => "",
            InviteKind::Person => WEB_CODE_PREFIX,
        }
    }

    /// Prefix of the secret the code is swapped for.
    fn secret_prefix(self) -> &'static str {
        match self {
            InviteKind::Agent => TOKEN_PREFIX,
            InviteKind::Person => SESSION_PREFIX,
        }
    }

    /// SQL that is true for the principals this kind of code may enrol.
    fn principal_clause(self) -> &'static str {
        match self {
            InviteKind::Agent => "kind <> 'person'",
            InviteKind::Person => "kind = 'person'",
        }
    }
}

/// Creates an invite for the principal `name`, creating the principal when
/// the name is new. An agent's name must already have passed
/// [`crate::names::validate_handle`], and a person's must be what
/// [`crate::names::validate_email`] returned. `token_ttl_ms` is the lifetime
/// of the token or session the code turns into.
pub(crate) fn create_invite(
    conn: &Connection,
    name: &str,
    kind: InviteKind,
    ttl_ms: i64,
    token_ttl_ms: i64,
    now_ms: i64,
) -> Result<NewInvite, String> {
    let sql = |error: rusqlite::Error| error.to_string();
    let existing: Option<(String, String, bool)> = conn
        .query_row(
            &format!(
                "SELECT id, handle, {} FROM principals WHERE handle_key = ?1",
                kind.principal_clause()
            ),
            [handle_key(name)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql)?;
    let (principal_id, handle, new_principal) = match existing {
        // The two kinds of name cannot collide, since only a person's holds
        // an `@`. This guards the invariant all the same.
        Some((_, _, false)) => {
            return Err("that name belongs to a principal of another kind".to_owned())
        }
        Some((id, handle, true)) => (id, handle, false),
        None => {
            let id = format!("prn_{}", random_hex(12)?);
            conn.execute(
                "INSERT INTO principals (id, kind, handle, handle_key, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    actor_kind_name(kind.actor_kind()),
                    name,
                    handle_key(name),
                    now_ms
                ],
            )
            .map_err(sql)?;
            (id, name.to_owned(), true)
        }
    };
    let id = format!("inv_{}", random_hex(8)?);
    let code = format!("{}{}", kind.code_prefix(), random_hex(SECRET_BYTES)?);
    let expires_ms = now_ms.saturating_add(ttl_ms);
    conn.execute(
        "INSERT INTO invites (id, principal_id, code_hash, token_ttl_ms, created_ms, expires_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            principal_id,
            sha256_hex(&code),
            token_ttl_ms,
            now_ms,
            expires_ms
        ],
    )
    .map_err(sql)?;
    log(
        conn,
        &LogEntry {
            kind: log_kinds::INVITE_CREATED,
            principal: Some(&handle),
            detail: Some(&id),
            ..LogEntry::default()
        },
    )
    .map_err(sql)?;
    Ok(NewInvite {
        row: InviteRow {
            id,
            handle,
            kind: kind.actor_kind(),
            created_ms: now_ms,
            expires_ms,
        },
        code,
        new_principal,
    })
}

/// Invites that can still be used.
pub(crate) fn list_pending_invites(
    conn: &Connection,
    now_ms: i64,
) -> rusqlite::Result<Vec<InviteRow>> {
    conn.prepare(
        "SELECT i.id, p.handle, p.kind, i.created_ms, i.expires_ms
         FROM invites i JOIN principals p ON p.id = i.principal_id
         WHERE i.used_ms IS NULL AND i.revoked_ms IS NULL AND i.expires_ms > ?1
         ORDER BY i.created_ms, i.id",
    )?
    .query_map([now_ms], |row| {
        Ok(InviteRow {
            id: row.get(0)?,
            handle: row.get(1)?,
            kind: kind_from_row(row, 2)?,
            created_ms: row.get(3)?,
            expires_ms: row.get(4)?,
        })
    })?
    .collect()
}

/// Whether `code` has the shape of a code of this kind: the kind's prefix
/// and [`SECRET_BYTES`] bytes in lowercase hex. Anything else cannot be one
/// and is refused without touching the database. The two kinds never share
/// a shape, so a code of one kind is refused here for the other.
pub(crate) fn code_is_well_formed(code: &str, kind: InviteKind) -> bool {
    code.strip_prefix(kind.code_prefix())
        .is_some_and(|secret| secret.len() == SECRET_BYTES * 2 && is_lower_hex(secret))
}

/// Swaps a code for a new credential: an agent's enrolment code for a
/// token, a person's sign-in code for a browser session. The code is
/// consumed by one conditional update, so of any number of concurrent
/// attempts with one code exactly one succeeds. Returns the secret, which
/// exists nowhere else, and the credential, or `None` when the code is
/// wrong, expired, withdrawn, already used or of the other kind. A code of
/// the other kind is left unspent.
///
/// The caller runs this inside a write transaction and appends the security
/// log entry.
pub(crate) fn redeem(
    conn: &Connection,
    code: &str,
    kind: InviteKind,
    now_ms: i64,
) -> Result<Option<(String, Credential)>, String> {
    let sql = |error: rusqlite::Error| error.to_string();
    // Made before the code is looked at, so a failed attempt does the same
    // work as a successful one up to the first write.
    let token = format!("{}{}", kind.secret_prefix(), random_hex(SECRET_BYTES)?);
    let credential_id = format!("crd_{}", random_hex(8)?);
    let code_hash = sha256_hex(code);

    let consumed = conn
        .execute(
            &format!(
                "UPDATE invites SET used_ms = ?1
                 WHERE code_hash = ?2 AND used_ms IS NULL AND revoked_ms IS NULL
                   AND expires_ms > ?1
                   AND principal_id IN (SELECT id FROM principals WHERE {})",
                kind.principal_clause()
            ),
            params![now_ms, code_hash],
        )
        .map_err(sql)?;
    if consumed != 1 {
        return Ok(None);
    }
    let (principal_id, token_ttl_ms): (String, i64) = conn
        .query_row(
            "SELECT principal_id, token_ttl_ms FROM invites WHERE code_hash = ?1",
            [&code_hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(sql)?;
    conn.execute(
        "INSERT INTO credentials (id, principal_id, token_hash, created_ms, expires_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            credential_id,
            principal_id,
            sha256_hex(&token),
            now_ms,
            now_ms.saturating_add(token_ttl_ms)
        ],
    )
    .map_err(sql)?;
    let credential = find_credential(conn, &credential_id)
        .map_err(sql)?
        .ok_or_else(|| "the new credential vanished".to_owned())?;
    debug_assert!(credential.usable_at(now_ms) || token_ttl_ms <= 0);
    Ok(Some((token, credential)))
}

/// What a revocation changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RevokedIds {
    pub handle: String,
    pub credentials: Vec<String>,
    pub invites: Vec<String>,
}

/// Revokes every live credential and withdraws every pending invite of the
/// principal `name`. Returns `None` when there is no such principal.
pub(crate) fn revoke_principal(
    conn: &Connection,
    name: &str,
    now_ms: i64,
) -> rusqlite::Result<Option<RevokedIds>> {
    let Some((principal_id, handle)) = find_principal(conn, name)? else {
        return Ok(None);
    };
    let ids = |sql: &str| -> rusqlite::Result<Vec<String>> {
        conn.prepare(sql)?
            .query_map([&principal_id], |row| row.get(0))?
            .collect()
    };
    let credentials = ids("SELECT id FROM credentials
         WHERE principal_id = ?1 AND revoked_ms IS NULL ORDER BY created_ms, id")?;
    let invites = ids("SELECT id FROM invites
         WHERE principal_id = ?1 AND used_ms IS NULL AND revoked_ms IS NULL
         ORDER BY created_ms, id")?;
    conn.execute(
        "UPDATE credentials SET revoked_ms = ?1 WHERE principal_id = ?2 AND revoked_ms IS NULL",
        params![now_ms, principal_id],
    )?;
    conn.execute(
        "UPDATE invites SET revoked_ms = ?1
         WHERE principal_id = ?2 AND used_ms IS NULL AND revoked_ms IS NULL",
        params![now_ms, principal_id],
    )?;
    for id in &credentials {
        log(
            conn,
            &LogEntry {
                kind: log_kinds::CREDENTIAL_REVOKED,
                principal: Some(&handle),
                credential_id: Some(id),
                detail: Some("by operator, with its principal"),
                ..LogEntry::default()
            },
        )?;
    }
    for id in &invites {
        log(
            conn,
            &LogEntry {
                kind: log_kinds::INVITE_REVOKED,
                principal: Some(&handle),
                detail: Some(id),
                ..LogEntry::default()
            },
        )?;
    }
    Ok(Some(RevokedIds {
        handle,
        credentials,
        invites,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_well_formed_tokens_are_looked_up() {
        let good = format!("{TOKEN_PREFIX}{}", "0a".repeat(32));
        assert_eq!(token_lookup_hash(&good), Some(sha256_hex(&good)));
        for bad in [
            String::new(),
            "obt_".to_owned(),
            "0a".repeat(34),
            format!("{TOKEN_PREFIX}{}", "0a".repeat(31)),
            format!("{TOKEN_PREFIX}{}", "0A".repeat(32)),
            format!("{TOKEN_PREFIX}{}", "zz".repeat(32)),
            format!("{TOKEN_PREFIX}{}", "0a".repeat(65)),
            format!(" {good}"),
        ] {
            assert_eq!(token_lookup_hash(&bad), None, "{bad}");
        }
    }

    #[test]
    fn tokens_and_sessions_never_share_a_shape() {
        let token = format!("{TOKEN_PREFIX}{}", "0a".repeat(32));
        let session = format!("{SESSION_PREFIX}{}", "0a".repeat(32));
        assert!(session_lookup_hash(&session).is_some());
        assert_eq!(session_lookup_hash(&token), None);
        assert_eq!(token_lookup_hash(&session), None);
        assert_ne!(session_lookup_hash(&session), token_lookup_hash(&token));

        let agent = "0a".repeat(32);
        let web = format!("{WEB_CODE_PREFIX}{agent}");
        assert!(code_is_well_formed(&agent, InviteKind::Agent));
        assert!(code_is_well_formed(&web, InviteKind::Person));
        assert!(!code_is_well_formed(&web, InviteKind::Agent));
        assert!(!code_is_well_formed(&agent, InviteKind::Person));
        assert!(!code_is_well_formed(WEB_CODE_PREFIX, InviteKind::Person));
    }

    #[test]
    fn the_csrf_token_is_bound_to_its_session() {
        let a = format!("{SESSION_PREFIX}{}", "0a".repeat(32));
        let b = format!("{SESSION_PREFIX}{}", "0b".repeat(32));
        assert_eq!(csrf_token(&a), csrf_token(&a));
        assert_ne!(csrf_token(&a), csrf_token(&b));
        assert_eq!(csrf_token(&a).len(), 64);
        // It is not the digest the session is stored under.
        assert_ne!(csrf_token(&a), sha256_hex(&a));

        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn random_values_are_long_and_differ() {
        let a = random_hex(SECRET_BYTES).unwrap();
        let b = random_hex(SECRET_BYTES).unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    #[test]
    fn client_addresses_are_truncated() {
        let truncated = |text: &str| truncate_ip(text.parse().unwrap());
        assert_eq!(truncated("192.168.1.77"), "192.168.1.0");
        assert_eq!(truncated("127.0.0.1"), "127.0.0.0");
        assert_eq!(truncated("2001:db8:aa:bb:cc:dd:ee:ff"), "2001:db8:aa::");
        assert_eq!(truncated("::ffff:10.1.2.3"), "10.1.2.0");
    }
}
