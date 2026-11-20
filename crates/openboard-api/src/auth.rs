//! Network mode: credentials, enrolment and certificate pinning.
//!
//! A board runs in one of two modes, reported by [`crate::Health::mode`]:
//!
//! - [`MODE_LOCAL`]: plain HTTP on loopback, no authentication, one `local`
//!   principal.
//! - [`MODE_NETWORK`]: TLS 1.3 on every listener, loopback included, and a
//!   bearer token on every request except `GET /v1/health` and
//!   `POST /v1/join`.
//!
//! # The board's certificate
//!
//! A network board has one of two kinds of certificate, and a client knows
//! which from its invite, never from what a server presents:
//!
//! - **Pinned.** The board generates a self-signed certificate on first
//!   start, and its invites carry the certificate's [`fingerprint`]. Clients
//!   do not use a certificate authority or check host names. They trust
//!   exactly one certificate, identified by that fingerprint, and must still
//!   verify the TLS handshake signature against that certificate's key, so
//!   that holding a copy of the certificate is not enough to impersonate
//!   the board.
//! - **Trusted.** The operator supplies a certificate for the board's host
//!   name, and its invites carry no fingerprint. Clients verify the chain
//!   against their certificate authorities and check the host name, as a
//!   browser does, so a renewed certificate changes nothing for them.
//!
//! A client never falls back from one to the other. One that holds a
//! fingerprint accepts only that certificate, whoever else vouches for the
//! one presented, and one that holds none never accepts a certificate its
//! certificate authorities do not vouch for.
//!
//! # Tokens
//!
//! A token is [`TOKEN_PREFIX`] followed by at least 32 random bytes in
//! lowercase hex. It is sent as `Authorization: Bearer <token>`. The board
//! stores only a SHA-256 hash of it. A missing, malformed, unknown, expired
//! or revoked token gets the same `401` with
//! [`crate::codes::UNAUTHENTICATED`], so a caller cannot tell which.
//!
//! # Enrolment
//!
//! The operator creates an invite on the board's host by acting on the data
//! directory. No network API can create invites, list credentials or revoke
//! them. The invite is one string (see [`Invite`]) that carries the board's
//! URL, the certificate fingerprint when the board is pinned, and a
//! single-use code. The new agent sends the code to `POST /v1/join` over a
//! connection verified as above and receives its token once.
//!
//! | Method | Path | Request | Response |
//! | --- | --- | --- | --- |
//! | POST | `/v1/join` | [`JoinRequest`] | [`JoinResponse`] |
//! | GET | `/v1/whoami` | | [`WhoAmI`] |
//! | POST | `/v1/logout` | | `204`, and the calling credential is revoked |

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::{Actor, ActorKind};

pub const MODE_LOCAL: &str = "local";
pub const MODE_NETWORK: &str = "network";

/// Prefix of every bearer token, so secret scanners can recognise one.
pub const TOKEN_PREFIX: &str = "obt_";
/// Prefix of an encoded [`Invite`].
pub const INVITE_PREFIX: &str = "obi1_";
/// Prefix of a certificate fingerprint.
pub const FINGERPRINT_PREFIX: &str = "sha256:";

/// Default lifetime of an unused invite, in seconds.
pub const DEFAULT_INVITE_TTL_SECS: u64 = 60 * 60;
/// Default lifetime of a token, in seconds (90 days).
pub const DEFAULT_TOKEN_TTL_SECS: u64 = 90 * 24 * 60 * 60;

/// Formats the pin for a certificate: [`FINGERPRINT_PREFIX`] followed by the
/// lowercase hex SHA-256 of the certificate's DER encoding. The caller
/// supplies the digest so this crate needs no hash implementation.
pub fn fingerprint(sha256_of_der: &[u8; 32]) -> String {
    let mut out = String::with_capacity(FINGERPRINT_PREFIX.len() + 64);
    out.push_str(FINGERPRINT_PREFIX);
    for byte in sha256_of_der {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Parses a fingerprint produced by [`fingerprint`] back into the digest.
/// Accepts uppercase hex. Returns `None` for anything else.
pub fn parse_fingerprint(text: &str) -> Option<[u8; 32]> {
    let hex = text.strip_prefix(FINGERPRINT_PREFIX)?;
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Everything a new agent needs to enrol, carried in one string.
///
/// The encoding is [`INVITE_PREFIX`] followed by unpadded URL-safe base64 of
/// this struct as JSON. Because the fingerprint of a pinned board travels
/// with the invite, the first connection is already pinned and there is no
/// trust-on-first-use step. An invite is a secret until it is used or
/// expires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    /// Base URL of the board, for example `https://192.168.1.20:7878`.
    pub url: String,
    /// The board certificate's pin, as produced by [`fingerprint`]. Absent
    /// for a board whose certificate is checked against the client's
    /// certificate authorities and the host name in `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Single-use enrolment code.
    pub code: String,
}

/// Why a string is not a usable invite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteError {
    /// It does not start with [`INVITE_PREFIX`].
    WrongPrefix,
    /// The part after the prefix is not base64 of the expected JSON.
    Malformed,
    /// The URL is not `https://`, or there is a fingerprint that does not
    /// parse.
    Invalid,
}

impl std::fmt::Display for InviteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            InviteError::WrongPrefix => "not an invite: it should start with `obi1_`",
            InviteError::Malformed => "the invite is damaged or incomplete",
            InviteError::Invalid => "the invite has an unusable URL or fingerprint",
        })
    }
}

impl std::error::Error for InviteError {}

impl Invite {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("an invite always serialises");
        format!("{INVITE_PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    /// Decodes and validates an invite. Surrounding whitespace is ignored so
    /// a pasted string with a trailing newline works.
    pub fn decode(text: &str) -> Result<Self, InviteError> {
        let body = text
            .trim()
            .strip_prefix(INVITE_PREFIX)
            .ok_or(InviteError::WrongPrefix)?;
        let json = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| InviteError::Malformed)?;
        let invite: Invite = serde_json::from_slice(&json).map_err(|_| InviteError::Malformed)?;
        if !invite.url.starts_with("https://")
            || invite
                .fingerprint
                .as_deref()
                .is_some_and(|pin| parse_fingerprint(pin).is_none())
            || invite.code.is_empty()
        {
            return Err(InviteError::Invalid);
        }
        Ok(invite)
    }
}

/// Body of `POST /v1/join`. The endpoint takes no bearer token. A wrong,
/// expired or already used code gets [`crate::codes::INVITE_INVALID`] in
/// every case.
///
/// Failed attempts are rate limited per client address: an address that has
/// made too many gets [`crate::codes::RATE_LIMITED`] for further failures
/// until the limit resets. A valid code is accepted regardless and does not
/// count, so other clients behind the same address cannot be locked out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinResponse {
    /// The bearer token. Returned once and never retrievable again.
    pub token: String,
    pub credential: CredentialInfo,
}

/// A credential, without its secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialInfo {
    /// Stable, non-secret identifier, shown in audit records.
    pub id: String,
    /// Handle of the principal this credential authenticates.
    pub principal: String,
    pub kind: ActorKind,
    /// RFC 3339 timestamps in UTC.
    pub created_at: String,
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    /// The credential this one was derived from, for a derived token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The profile a derived token is bound to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// Response of `GET /v1/whoami`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoAmI {
    /// The caller as the board will record it, including any declared profile.
    pub actor: Actor,
    /// [`MODE_LOCAL`] or [`MODE_NETWORK`].
    pub mode: String,
    /// Absent in local mode, where there are no credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<CredentialInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_round_trips() {
        let digest = [0xabu8; 32];
        let text = fingerprint(&digest);
        assert!(text.starts_with("sha256:abab"));
        assert_eq!(parse_fingerprint(&text), Some(digest));
        assert_eq!(
            parse_fingerprint(&text.to_uppercase().replace("SHA256", "sha256")),
            Some(digest)
        );
        assert_eq!(parse_fingerprint("sha256:abc"), None);
        assert_eq!(parse_fingerprint("md5:abab"), None);
    }

    #[test]
    fn invite_round_trips_and_rejects_bad_input() {
        let invite = Invite {
            url: "https://192.168.1.20:7878".into(),
            fingerprint: Some(fingerprint(&[7u8; 32])),
            code: "code".into(),
        };
        let text = invite.encode();
        assert_eq!(Invite::decode(&format!("  {text}\n")), Ok(invite.clone()));
        assert_eq!(Invite::decode("nope"), Err(InviteError::WrongPrefix));
        assert_eq!(Invite::decode("obi1_!!!"), Err(InviteError::Malformed));
        let plain = Invite {
            url: "http://192.168.1.20:7878".into(),
            ..invite
        };
        assert_eq!(Invite::decode(&plain.encode()), Err(InviteError::Invalid));
    }

    #[test]
    fn an_invite_to_a_trusted_board_has_no_fingerprint() {
        let invite = Invite {
            url: "https://board.example".into(),
            fingerprint: None,
            code: "code".into(),
        };
        let text = invite.encode();
        let json = URL_SAFE_NO_PAD
            .decode(&text[INVITE_PREFIX.len()..])
            .unwrap();
        assert!(!String::from_utf8(json).unwrap().contains("fingerprint"));
        assert_eq!(Invite::decode(&text), Ok(invite.clone()));
        // A fingerprint that is there must still be a fingerprint: a
        // damaged one never turns a pinned invite into a trusted one.
        let damaged = Invite {
            fingerprint: Some("sha256:beef".into()),
            ..invite
        };
        assert_eq!(Invite::decode(&damaged.encode()), Err(InviteError::Invalid));
    }
}
