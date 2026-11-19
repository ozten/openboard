//! The event hash chain and its offline verifier.
//!
//! # Canonical encoding
//!
//! Every event records the version of the encoding its hash was computed
//! with. Version 2 is written today. Version 1 was written by milestone M0
//! and is still verified, so a data directory from M0 keeps its history.
//!
//! An event's `hash` is the SHA-256, in lowercase hex, of these bytes:
//!
//! ```text
//! openboard.event.v2\n               (`openboard.event.v1\n` in version 1)
//! prev_hash=<len>:<value>\n
//! board=<len>:<value>\n
//! seq=<len>:<value>\n
//! type=<len>:<value>\n
//! channel=<len>:<value>\n
//! thread_id=<len>:<value>\n
//! message_id=<len>:<value>\n
//! actor_id=<len>:<value>\n
//! actor_kind=<len>:<value>\n
//! actor_handle=<len>:<value>\n
//! actor_profile=<len>:<value>\n      (or `actor_profile=-\n` when there is none)
//! at=<len>:<value>\n
//! credential_id=<len>:<value>\n      (version 2 only; `credential_id=-\n` when there is none)
//! ```
//!
//! `<len>` is the value's length in bytes as a decimal number, and the value
//! is its UTF-8 bytes. `seq` is the event's position in its board's log,
//! starting at 1, in decimal. `board` is the board's name as first written.
//! `prev_hash` is the previous event's `hash`, or 64 zeros for the first
//! event. `credential_id` is the ID of the credential that wrote the event on
//! a network board, and is absent on a local one. A version 1 event never
//! has one. The version itself is covered through the first line, so an
//! event cannot be moved from one encoding to the other without breaking its
//! hash. Message text is never part of an event (design P6), so it is not
//! covered by the chain.
//!
//! `type` is one of the names in `openboard_api::event_types`. Every type
//! has the same fields and is encoded the same way, so the types added
//! after `message.posted` (edits, deletions, label and reaction changes)
//! needed no new encoding version: an event's type is covered by its hash
//! like any other field, and one chain per board runs through all of them.
//! For a label or reaction change the actor and credential are those of
//! whoever made the change. Label values and reaction names are never part
//! of an event either.

use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

use crate::store::DB_FILE_NAME;

/// `prev_hash` of the first event in a board's log.
pub(crate) const GENESIS_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Everything an event's hash covers.
pub(crate) struct EventFields<'a> {
    pub prev_hash: &'a str,
    pub board: &'a str,
    pub seq: i64,
    pub kind: &'a str,
    pub channel: &'a str,
    pub thread_id: &'a str,
    pub message_id: &'a str,
    pub actor_id: &'a str,
    pub actor_kind: &'a str,
    pub actor_handle: &'a str,
    pub actor_profile: Option<&'a str>,
    pub at: &'a str,
    /// Covered by version 2 only.
    pub credential_id: Option<&'a str>,
}

/// The encoding new events are written with.
pub(crate) const HASH_VERSION: i64 = 2;

/// Computes an event's hash over the canonical encoding described in the
/// module documentation. Returns `None` for a version this server does not
/// know, and for a version 1 event that claims a credential, which that
/// encoding cannot cover.
pub(crate) fn event_hash(version: i64, fields: &EventFields<'_>) -> Option<String> {
    let seq = fields.seq.to_string();
    let mut hasher = Sha256::new();
    match version {
        1 if fields.credential_id.is_none() => hasher.update(b"openboard.event.v1\n"),
        2 => hasher.update(b"openboard.event.v2\n"),
        _ => return None,
    }
    let mut field = |name: &str, value: Option<&str>| {
        hasher.update(name.as_bytes());
        match value {
            Some(value) => {
                hasher.update(format!("={}:", value.len()).as_bytes());
                hasher.update(value.as_bytes());
                hasher.update(b"\n");
            }
            None => hasher.update(b"=-\n"),
        }
    };
    field("prev_hash", Some(fields.prev_hash));
    field("board", Some(fields.board));
    field("seq", Some(&seq));
    field("type", Some(fields.kind));
    field("channel", Some(fields.channel));
    field("thread_id", Some(fields.thread_id));
    field("message_id", Some(fields.message_id));
    field("actor_id", Some(fields.actor_id));
    field("actor_kind", Some(fields.actor_kind));
    field("actor_handle", Some(fields.actor_handle));
    field("actor_profile", fields.actor_profile);
    field("at", Some(fields.at));
    if version >= 2 {
        field("credential_id", fields.credential_id);
    }
    Some(hex(&hasher.finalize()))
}

/// Lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The result of [`verify_log`]: one entry per board, in creation order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub boards: Vec<BoardReport>,
}

impl VerifyReport {
    /// True when no board has a broken entry.
    pub fn is_ok(&self) -> bool {
        self.boards.iter().all(|board| board.broken.is_none())
    }
}

/// The state of one board's event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardReport {
    pub board: String,
    /// Events that verified, counted from the start of the log. Checking
    /// stops at the first broken entry.
    pub verified_events: u64,
    /// The first broken entry, if any.
    pub broken: Option<BrokenEntry>,
}

/// The first place a board's chain fails to verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokenEntry {
    /// Position in the board's log, starting at 1. For a missing entry this
    /// is the position where an event was expected.
    pub position: u64,
    /// What is wrong. Never contains message text.
    pub reason: String,
}

/// Why [`verify_log`] could not produce a report.
#[derive(Debug)]
pub enum VerifyError {
    /// The data directory holds no board database.
    NoDatabase(PathBuf),
    /// The database could not be read.
    Storage(String),
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::NoDatabase(path) => {
                write!(f, "no board database at {}", path.display())
            }
            VerifyError::Storage(detail) => write!(f, "cannot read the board database: {detail}"),
        }
    }
}

impl std::error::Error for VerifyError {}

impl From<rusqlite::Error> for VerifyError {
    fn from(error: rusqlite::Error) -> Self {
        VerifyError::Storage(error.to_string())
    }
}

/// Re-computes every board's hash chain from the database in `data_dir` and
/// reports the first broken entry per board.
///
/// The check is offline and read-only. It does not need a running server and
/// is safe to run beside one. For each board it confirms that positions run
/// from 1 with no gap, that each entry's `prev_hash` is the hash of the entry
/// before it, that each entry's `hash` matches its fields, and that the log
/// ends where the board record says it ends, which catches a truncated tail.
///
/// This function blocks on disk I/O. Call it from a blocking context.
pub fn verify_log(data_dir: &Path) -> Result<VerifyReport, VerifyError> {
    let path = data_dir.join(DB_FILE_NAME);
    if !path.is_file() {
        return Err(VerifyError::NoDatabase(path));
    }
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    // One read transaction, so a live server cannot move the log mid-check.
    let tx = conn.unchecked_transaction()?;

    let boards = tx
        .prepare("SELECT id, name, head_seq, head_hash FROM boards ORDER BY id")?
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut report = VerifyReport { boards: Vec::new() };
    for (board_id, board, head_seq, head_hash) in boards {
        let (verified_events, broken) = verify_board(&tx, board_id, &board, head_seq, &head_hash)?;
        report.boards.push(BoardReport {
            board,
            verified_events,
            broken,
        });
    }
    Ok(report)
}

fn verify_board(
    conn: &Connection,
    board_id: i64,
    board: &str,
    head_seq: i64,
    head_hash: &str,
) -> Result<(u64, Option<BrokenEntry>), VerifyError> {
    // A data directory that milestone M0 wrote and no newer server has
    // opened yet has neither column. All its events are version 1.
    let has_versions: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('events') WHERE name = 'hash_version')",
        [],
        |row| row.get(0),
    )?;
    let extra = if has_versions {
        "e.hash_version, e.credential_id"
    } else {
        "1, NULL"
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT e.seq, e.kind, e.channel, e.thread_id, e.message_id, e.at, e.prev_hash, e.hash,
                e.actor_id, a.kind, a.handle, a.profile, {extra}
         FROM events e LEFT JOIN actors a ON a.id = e.actor_id
         WHERE e.board_id = ?1 ORDER BY e.seq"
    ))?;
    let mut rows = stmt.query([board_id])?;
    let mut verified: u64 = 0;
    let mut prev_hash = GENESIS_HASH.to_owned();
    let broken = |position: u64, reason: &str| {
        Some(BrokenEntry {
            position,
            reason: reason.to_owned(),
        })
    };

    while let Some(row) = rows.next()? {
        let expected = verified + 1;
        let Ok(seq) = row.get::<_, i64>(0) else {
            return Ok((
                verified,
                broken(expected, "the entry has no readable position"),
            ));
        };
        if seq != expected as i64 {
            return Ok((verified, broken(expected, "the entry is missing")));
        }
        let text = |index: usize| row.get::<_, String>(index);
        let (
            Ok(kind),
            Ok(channel),
            Ok(thread_id),
            Ok(message_id),
            Ok(at),
            Ok(stored_prev),
            Ok(stored_hash),
            Ok(actor_id),
        ) = (
            text(1),
            text(2),
            text(3),
            text(4),
            text(5),
            text(6),
            text(7),
            text(8),
        )
        else {
            return Ok((
                verified,
                broken(expected, "the entry has an unreadable field"),
            ));
        };
        let (Ok(actor_kind), Ok(actor_handle), Ok(actor_profile)) =
            (text(9), text(10), row.get::<_, Option<String>>(11))
        else {
            return Ok((
                verified,
                broken(
                    expected,
                    "the entry's actor record is missing or unreadable",
                ),
            ));
        };
        let (Ok(version), Ok(credential_id)) =
            (row.get::<_, i64>(12), row.get::<_, Option<String>>(13))
        else {
            return Ok((
                verified,
                broken(expected, "the entry has an unreadable field"),
            ));
        };
        if stored_prev != prev_hash {
            return Ok((
                verified,
                broken(
                    expected,
                    "prev_hash does not match the previous entry's hash",
                ),
            ));
        }
        let computed = event_hash(
            version,
            &EventFields {
                prev_hash: &stored_prev,
                board,
                seq,
                kind: &kind,
                channel: &channel,
                thread_id: &thread_id,
                message_id: &message_id,
                actor_id: &actor_id,
                actor_kind: &actor_kind,
                actor_handle: &actor_handle,
                actor_profile: actor_profile.as_deref(),
                at: &at,
                credential_id: credential_id.as_deref(),
            },
        );
        let Some(computed) = computed else {
            return Ok((
                verified,
                broken(
                    expected,
                    "the entry uses a hash encoding this version does not know, or one that cannot cover its credential",
                ),
            ));
        };
        if computed != stored_hash {
            return Ok((
                verified,
                broken(
                    expected,
                    "the stored hash does not match the entry's fields",
                ),
            ));
        }
        prev_hash = stored_hash;
        verified += 1;
    }

    if verified as i64 != head_seq {
        let reason = if (verified as i64) < head_seq {
            "the log ends before the board's recorded head"
        } else {
            "the log continues past the board's recorded head"
        };
        return Ok((verified, broken(verified + 1, reason)));
    }
    if prev_hash != head_hash {
        return Ok((
            verified,
            broken(
                verified.max(1),
                "the last entry's hash does not match the board's recorded head",
            ),
        ));
    }
    Ok((verified, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields<'a>(profile: Option<&'a str>, channel: &'a str) -> EventFields<'a> {
        EventFields {
            prev_hash: GENESIS_HASH,
            board: "general",
            seq: 1,
            kind: "message.posted",
            channel,
            thread_id: "msg_a",
            message_id: "msg_a",
            actor_id: "act_1",
            actor_kind: "local",
            actor_handle: "local",
            actor_profile: profile,
            at: "2026-01-01T00:00:00.000Z",
            credential_id: None,
        }
    }

    fn v1(fields: &EventFields<'_>) -> String {
        event_hash(1, fields).unwrap()
    }

    fn v2(fields: &EventFields<'_>) -> String {
        event_hash(2, fields).unwrap()
    }

    #[test]
    fn hash_is_deterministic_and_field_sensitive() {
        for hash in [v1, v2] {
            let base = hash(&fields(None, "a"));
            assert_eq!(base.len(), 64);
            assert_eq!(base, hash(&fields(None, "a")));
            assert_ne!(base, hash(&fields(None, "b")));
            assert_ne!(base, hash(&fields(Some("fx"), "a")));
        }
    }

    #[test]
    fn versions_differ_and_version_2_covers_the_credential() {
        let plain = fields(None, "a");
        assert_ne!(v1(&plain), v2(&plain));
        let with = |credential_id| EventFields {
            credential_id,
            ..fields(None, "a")
        };
        assert_ne!(v2(&plain), v2(&with(Some("crd_1"))));
        assert_ne!(v2(&with(Some("crd_1"))), v2(&with(Some("crd_2"))));
        // Version 1 cannot cover a credential, and unknown versions have no
        // encoding at all.
        assert_eq!(event_hash(1, &with(Some("crd_1"))), None);
        assert_eq!(event_hash(0, &plain), None);
        assert_eq!(event_hash(3, &plain), None);
    }

    #[test]
    fn version_2_matches_the_documented_encoding() {
        let encoded = |credential: &str| {
            format!(
                "openboard.event.v2\nprev_hash=64:{GENESIS_HASH}\nboard=7:general\nseq=1:1\n\
                 type=14:message.posted\nchannel=1:a\nthread_id=5:msg_a\nmessage_id=5:msg_a\n\
                 actor_id=5:act_1\nactor_kind=5:local\nactor_handle=5:local\nactor_profile=-\n\
                 at=24:2026-01-01T00:00:00.000Z\ncredential_id={credential}\n"
            )
        };
        assert_eq!(
            v2(&fields(None, "a")),
            hex(&Sha256::digest(encoded("-").as_bytes()))
        );
        let with = EventFields {
            credential_id: Some("crd_1"),
            ..fields(None, "a")
        };
        assert_eq!(
            v2(&with),
            hex(&Sha256::digest(encoded("5:crd_1").as_bytes()))
        );
    }

    #[test]
    fn hash_matches_the_documented_encoding() {
        let encoded = format!(
            "openboard.event.v1\nprev_hash=64:{GENESIS_HASH}\nboard=7:general\nseq=1:1\n\
             type=14:message.posted\nchannel=1:a\nthread_id=5:msg_a\nmessage_id=5:msg_a\n\
             actor_id=5:act_1\nactor_kind=5:local\nactor_handle=5:local\nactor_profile=-\n\
             at=24:2026-01-01T00:00:00.000Z\n"
        );
        assert_eq!(
            v1(&fields(None, "a")),
            hex(&Sha256::digest(encoded.as_bytes()))
        );
    }
}
