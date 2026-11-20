//! Derived tokens (A9, A10): a token minted from the caller's own, for one
//! board, that can only narrow. `docs/permissions.md` describes the rules.

use serde::{Deserialize, Serialize};

use crate::auth::CredentialInfo;

/// `GET` lists the derived tokens the caller may revoke; `POST` mints one.
pub const TOKENS_PATH: &str = "/v1/tokens";

/// `DELETE` revokes one derived token, and every token derived from it.
pub fn token_path(id: &str) -> String {
    format!("{TOKENS_PATH}/{id}")
}

/// How long a derived token lasts when the request does not say.
pub const DEFAULT_TOKEN_TTL_SECS: u64 = 8 * 60 * 60;

/// The longest a derived token may last. It never outlives its parent.
pub const MAX_TOKEN_TTL_SECS: u64 = 90 * 24 * 60 * 60;

/// The most `pattern=verbs` entries one token's scope may hold.
pub const MAX_SCOPE_ENTRIES: usize = 32;

/// The body of `POST /v1/tokens`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewToken {
    /// The one board the token works on.
    pub board: String,
    /// Entries such as `build/=read,post,label,task.claim`. Access is the
    /// caller's grants intersected with these at every request.
    pub can: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
    /// Binds the token to a profile: requests made with it write as that
    /// profile, whatever header they send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// A derived token, without its secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenInfo {
    pub credential: CredentialInfo,
    pub board: String,
    pub can: Vec<String>,
}

/// The answer to `POST /v1/tokens`. The token exists nowhere else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenMinted {
    pub token: String,
    #[serde(flatten)]
    pub info: TokenInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenList {
    pub tokens: Vec<TokenInfo>,
}
