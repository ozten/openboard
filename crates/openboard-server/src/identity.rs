//! Principals, credentials, invites and the security log.
//!
//! Everything here is plain SQL on a connection the caller owns, so the
//! running server and the operator functions, which use a second connection,
//! share one implementation. Callers wrap mutations in a transaction.
//!
//! Secrets never reach the database: a token and an enrolment code are
//! stored only as SHA-256 digests and are looked up by digest, so no secret
//! is ever compared byte by byte.

use std::net::IpAddr;

use openboard_api::auth::{CredentialInfo, TOKEN_PREFIX};
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
/// A credential's last-used time is rewritten at most this often.
const LAST_USED_GRANULARITY_MS: i64 = 60_000;

/// `meta` keys.
pub(crate) const META_MODE: &str = "mode";
pub(crate) const META_URL: &str = "public_url";
pub(crate) const META_FINGERPRINT: &str = "fingerprint";

/// Security log entry types.
pub mod log_kinds {
    /// A server started on the data directory. The detail holds the mode.
    pub const SERVER_STARTED: &str = "server.started";
    /// A local data directory became a network one.
    pub const MODE_UPGRADED: &str = "mode.upgraded";
    pub const INVITE_CREATED: &str = "invite.created";
    /// A pending invite was withdrawn when its principal was revoked.
    pub const INVITE_REVOKED: &str = "invite.revoked";
    pub const ENROL_SUCCEEDED: &str = "enrol.succeeded";
    pub const ENROL_FAILED: &str = "enrol.failed";
    pub const CREDENTIAL_REVOKED: &str = "credential.revoked";
    pub const LOGOUT: &str = "logout";
    pub const AUTH_FAILED: &str = "auth.failed";
    /// A client address or a credential ran into a rate limit. Written once
    /// per address or credential per window.
    pub const RATE_LIMITED: &str = "rate.limited";
    /// Failure entries were dropped for the rest of the minute because the
    /// log's budget for them was spent.
    pub const LOG_SUPPRESSED: &str = "log.suppressed";
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

/// The digest to look a presented token up by, or `None` when the text
/// cannot be a token, in which case nothing is looked up.
pub(crate) fn token_lookup_hash(presented: &str) -> Option<String> {
    let secret = presented.strip_prefix(TOKEN_PREFIX)?;
    let well_formed = (SECRET_BYTES * 2..=MAX_TOKEN_HEX).contains(&secret.len())
        && secret
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    well_formed.then(|| sha256_hex(presented))
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
}

impl Credential {
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
        }
    }

    fn usable_at(&self, now_ms: i64) -> bool {
        self.revoked_ms.is_none() && now_ms < self.expires_ms
    }
}

const CREDENTIAL_COLUMNS: &str = "c.id, c.principal_id, p.handle, p.kind, c.created_ms, \
                                  c.expires_ms, c.revoked_ms, c.last_used_ms";
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
    })
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
/// credential's last-used time is refreshed, at most once a minute.
pub(crate) fn authenticate(
    conn: &Connection,
    token_hash: &str,
    now_ms: i64,
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
    if credential.revoked_ms.is_some() {
        return Ok(AuthOutcome::Revoked(credential));
    }
    if now_ms >= credential.expires_ms {
        return Ok(AuthOutcome::Expired(credential));
    }
    let stale = credential
        .last_used_ms
        .is_none_or(|last| now_ms.saturating_sub(last) >= LAST_USED_GRANULARITY_MS);
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
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM credentials
                        WHERE id = ?1 AND revoked_ms IS NULL AND expires_ms > ?2)",
        params![credential_id, now_ms],
        |row| row.get(0),
    )
}

pub(crate) fn find_credential(
    conn: &Connection,
    credential_id: &str,
) -> rusqlite::Result<Option<Credential>> {
    conn.query_row(
        &format!("SELECT {CREDENTIAL_COLUMNS} FROM {CREDENTIAL_FROM} WHERE c.id = ?1"),
        [credential_id],
        credential_from_row,
    )
    .optional()
}

pub(crate) fn list_credentials(conn: &Connection) -> rusqlite::Result<Vec<Credential>> {
    conn.prepare(&format!(
        "SELECT {CREDENTIAL_COLUMNS} FROM {CREDENTIAL_FROM} ORDER BY c.created_ms, c.id"
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

/// Creates an invite for the agent `name`, creating the principal when the
/// name is new. The name must already have passed
/// [`crate::names::validate_handle`].
pub(crate) fn create_invite(
    conn: &Connection,
    name: &str,
    ttl_ms: i64,
    token_ttl_ms: i64,
    now_ms: i64,
) -> Result<NewInvite, String> {
    let sql = |error: rusqlite::Error| error.to_string();
    let (principal_id, handle, new_principal) = match find_principal(conn, name).map_err(sql)? {
        Some((id, handle)) => (id, handle, false),
        None => {
            let id = format!("prn_{}", random_hex(12)?);
            conn.execute(
                "INSERT INTO principals (id, kind, handle, handle_key, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    actor_kind_name(ActorKind::Agent),
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
    let code = random_hex(SECRET_BYTES)?;
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
        "SELECT i.id, p.handle, i.created_ms, i.expires_ms
         FROM invites i JOIN principals p ON p.id = i.principal_id
         WHERE i.used_ms IS NULL AND i.revoked_ms IS NULL AND i.expires_ms > ?1
         ORDER BY i.created_ms, i.id",
    )?
    .query_map([now_ms], |row| {
        Ok(InviteRow {
            id: row.get(0)?,
            handle: row.get(1)?,
            created_ms: row.get(2)?,
            expires_ms: row.get(3)?,
        })
    })?
    .collect()
}

/// Swaps an enrolment code for a new credential. The code is consumed by
/// one conditional update, so of any number of concurrent attempts with one
/// code exactly one succeeds. Returns the token, which exists nowhere else,
/// and the credential, or `None` when the code is wrong, expired, withdrawn
/// or already used.
///
/// The caller runs this inside a write transaction and appends the security
/// log entry.
pub(crate) fn join(
    conn: &Connection,
    code: &str,
    now_ms: i64,
) -> Result<Option<(String, Credential)>, String> {
    let sql = |error: rusqlite::Error| error.to_string();
    // Made before the code is looked at, so a failed attempt does the same
    // work as a successful one up to the first write.
    let token = format!("{TOKEN_PREFIX}{}", random_hex(SECRET_BYTES)?);
    let credential_id = format!("crd_{}", random_hex(8)?);
    let code_hash = sha256_hex(code);

    let consumed = conn
        .execute(
            "UPDATE invites SET used_ms = ?1
             WHERE code_hash = ?2 AND used_ms IS NULL AND revoked_ms IS NULL AND expires_ms > ?1",
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
