//! The web UI and browser sessions.
//!
//! # Pages
//!
//! The server serves the UI itself and the UI makes no request to any other
//! origin. `GET /` and `GET /join` return the page, and `GET /assets/{file}`
//! its script and style sheet. They need no credential in either mode,
//! because they hold no board content. Every one of these responses, and
//! every API response, carries:
//!
//! - `Content-Security-Policy: default-src 'none'; script-src 'self';
//!   style-src 'self'; img-src 'self' data:; connect-src 'self';
//!   base-uri 'none'; form-action 'none'; frame-ancestors 'none'`
//! - `X-Content-Type-Options: nosniff`
//! - `Referrer-Policy: no-referrer`
//! - `Cache-Control: no-store` on API responses and the page.
//!
//! The server sends no `Access-Control-*` headers, so no other origin can
//! read a response.
//!
//! In local mode the UI needs no sign-in: the browser acts as the `local`
//! principal, as every local client does.
//!
//! # Signing a person in
//!
//! A person is a principal of kind `person` whose handle is their email
//! address, lowercased. The operator runs `openboard invite ada@example.com`
//! on the board's host, which prints a one-time link:
//!
//! ```text
//! https://192.168.1.20:7878/join#obw1_3f9c…
//! ```
//!
//! The part after `#` is a single-use code starting with
//! [`WEB_CODE_PREFIX`]. A browser never sends a fragment to a server, so the
//! code stays out of request logs, proxies and `Referer` headers, and a tool
//! that merely fetches the link to preview it cannot spend the code. The
//! page reads the fragment, removes it from the address bar, and sends the
//! code in [`LoginRequest`] only when the person presses the sign-in button.
//! No email is sent: handing over the link is the verification.
//!
//! A web code works only at `POST /v1/web/login`, and an agent's invite code
//! works only at `POST /v1/join`. Either one presented at the wrong endpoint
//! gets [`crate::codes::INVITE_INVALID`] and is not spent. Failed logins
//! share the rate limit of failed joins.
//!
//! # The session cookie
//!
//! A successful login sets one cookie named [`SESSION_COOKIE`] with the
//! attributes `Secure; HttpOnly; SameSite=Strict; Path=/` and a `Max-Age`
//! matching the session's lifetime. Its value is a fresh random secret that
//! the response body never contains, so script on the page cannot read it.
//! The board stores only its SHA-256 hash. A session is a credential like an
//! agent's token: it has an ID, it expires ([`DEFAULT_SESSION_TTL_SECS`]),
//! the operator can list and revoke it, and `POST /v1/logout` revokes it and
//! clears the cookie.
//!
//! A request that carries both an `Authorization` header and the cookie is
//! authenticated by the header alone.
//!
//! # Anti-CSRF
//!
//! Two checks protect writes. A write is any request whose method is not
//! `GET` or `HEAD`.
//!
//! 1. In both modes, a write that carries an `Origin` header whose value is
//!    not the scheme and authority the request was sent to is refused.
//! 2. A write authenticated by the session cookie must also carry the
//!    session's anti-CSRF token in the [`CSRF_HEADER`] request header. The
//!    token is [`Session::csrf_token`]. It is bound to the session and
//!    useless without the cookie.
//!
//! Both failures return `403` with [`crate::codes::CSRF_FAILED`] and change
//! nothing. Requests authenticated by a bearer token skip the second check.
//! `POST /v1/web/login` has no session yet and is covered by the first.

use serde::{Deserialize, Serialize};

use crate::auth::CredentialInfo;
use crate::Actor;

/// Name of the session cookie. The `__Host-` prefix makes browsers refuse
/// it unless it is `Secure`, has `Path=/` and names no domain.
pub const SESSION_COOKIE: &str = "__Host-openboard_session";
/// Request header carrying the anti-CSRF token on cookie-authenticated
/// writes.
pub const CSRF_HEADER: &str = "x-openboard-csrf";
/// Prefix of a person's single-use sign-in code.
pub const WEB_CODE_PREFIX: &str = "obw1_";
/// Path of the sign-in page. The code follows as the URL fragment.
pub const JOIN_PAGE_PATH: &str = "/join";
/// Default lifetime of a browser session, in seconds (30 days).
pub const DEFAULT_SESSION_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// Builds the link the operator hands to a person: the board's base URL,
/// [`JOIN_PAGE_PATH`], and the code as the fragment.
pub fn sign_in_link(base_url: &str, code: &str) -> String {
    format!("{}{JOIN_PAGE_PATH}#{code}", base_url.trim_end_matches('/'))
}

/// Body of `POST /v1/web/login`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRequest {
    /// The code from the sign-in link's fragment.
    pub code: String,
}

/// Response of `POST /v1/web/login` and `GET /v1/web/session`.
///
/// `GET /v1/web/session` describes whoever the request is authenticated as.
/// In local mode that is the `local` principal, and `csrf_token` and
/// `credential` are absent. In network mode a request with no valid session
/// or token gets the usual `401`, which is how the page learns that it must
/// show the sign-in screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub actor: Actor,
    /// [`crate::auth::MODE_LOCAL`] or [`crate::auth::MODE_NETWORK`].
    pub mode: String,
    /// The token to send in [`CSRF_HEADER`]. Present for a cookie session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csrf_token: Option<String>,
    /// The session's credential, without its secret. Absent in local mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<CredentialInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_in_link_puts_the_code_in_the_fragment() {
        assert_eq!(
            sign_in_link("https://192.168.1.20:7878/", "obw1_abc"),
            "https://192.168.1.20:7878/join#obw1_abc"
        );
    }
}
