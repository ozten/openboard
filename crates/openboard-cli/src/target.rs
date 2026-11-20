//! Which board a command talks to, and as whom.
//!
//! The order, also documented in `openboard --help`:
//!
//! 1. `BOARD_TOKEN` in the environment. It needs `BOARD_URL`, and
//!    `BOARD_FINGERPRINT` when the board has a self-signed certificate.
//!    The login file is not read.
//! 2. A stored login, filtered by `--url`/`BOARD_URL` and
//!    `--login`/`BOARD_LOGIN` where given. Exactly one must match.
//! 3. With no URL and no stored login, the local board at
//!    [`crate::cli::DEFAULT_URL`].
//!
//! An `http://` URL always means a local board: no login is looked up and no
//! token is ever sent to it.
//!
//! An `https://` board with a fingerprint is pinned to it. One without is
//! checked against this machine's certificate authorities and its host
//! name. See [`crate::tls`].

use std::path::PathBuf;

use openboard_api::auth::parse_fingerprint;

use crate::cli::DEFAULT_URL;
use crate::error::{codes, Failure, EXIT_USAGE};
use crate::logins::{same_principal, same_url, Login, Secret, Store};

const GET_AN_INVITE: &str = "ask the board's operator for an invite (`openboard invite <name>` \
                             on the board's host), then run `openboard join <invite>`";

/// Where a target's credentials came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A local board: plain HTTP, no credentials.
    Local,
    /// `BOARD_TOKEN`, `BOARD_URL` and `BOARD_FINGERPRINT`.
    Env,
    /// A login in the file at this path.
    Stored(PathBuf),
}

/// A board to talk to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub url: String,
    /// The certificate pin of an `https://` board with a self-signed
    /// certificate. `None` for one whose certificate the certificate
    /// authorities vouch for, and for a local board.
    pub fingerprint: Option<String>,
    /// Set only for an `https://` board.
    pub token: Option<Secret>,
    /// The enrolled name, when a stored login was chosen.
    pub principal: Option<String>,
    pub credential_id: Option<String>,
    pub origin: Origin,
    /// True when nothing chose this board: no URL was given and no login is
    /// stored, so the local default is tried.
    pub assumed: bool,
}

impl Target {
    pub fn is_network(&self) -> bool {
        is_https(&self.url)
    }

    fn local(url: &str) -> Self {
        Self {
            url: url.to_owned(),
            fingerprint: None,
            token: None,
            principal: None,
            credential_id: None,
            origin: Origin::Local,
            assumed: false,
        }
    }

    /// The board an invite names, before there is a token for it.
    pub fn for_join(url: &str, fingerprint: Option<&str>) -> Self {
        Self {
            url: url.to_owned(),
            fingerprint: fingerprint.map(str::to_owned),
            token: None,
            principal: None,
            credential_id: None,
            origin: Origin::Env,
            assumed: false,
        }
    }

    /// The board and token of one login, wherever it is kept.
    pub fn for_login(login: &Login) -> Result<Self, Failure> {
        Self::stored(login, PathBuf::new())
    }

    fn stored(login: &Login, path: PathBuf) -> Result<Self, Failure> {
        let unreadable_pin = login
            .fingerprint
            .as_deref()
            .is_some_and(|pin| parse_fingerprint(pin).is_none());
        if !is_https(&login.url) || unreadable_pin {
            return Err(Failure::new(
                codes::LOGIN_FILE,
                format!(
                    "the stored login for {} at {} has an unusable URL or fingerprint",
                    login.principal, login.url
                ),
            )
            .next(format!(
                "forget it with `openboard logout --local-only`, then {GET_AN_INVITE}"
            )));
        }
        Ok(Self {
            url: login.url.clone(),
            fingerprint: login.fingerprint.clone(),
            token: Some(login.token.clone()),
            principal: Some(login.principal.clone()),
            credential_id: Some(login.credential_id.clone()),
            origin: Origin::Stored(path),
            assumed: false,
        })
    }
}

/// What the command line and the environment ask for. Empty values count as
/// unset.
#[derive(Debug, Default)]
pub struct Wanted {
    /// `--url` or `BOARD_URL`.
    pub url: Option<String>,
    /// `--login` or `BOARD_LOGIN`.
    pub login: Option<String>,
    /// `BOARD_TOKEN`.
    pub token: Option<Secret>,
    /// `BOARD_FINGERPRINT`.
    pub fingerprint: Option<String>,
}

fn starts_with_ignoring_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

pub fn is_https(url: &str) -> bool {
    starts_with_ignoring_case(url, "https://")
}

fn is_http(url: &str) -> bool {
    starts_with_ignoring_case(url, "http://")
}

pub fn bad_url(url: &str, why: &str) -> Failure {
    Failure::usage(format!("the board URL {url:?} {why}")).next(
        "set --url or BOARD_URL to something like http://127.0.0.1:7878 for a local board, or \
         https://192.168.1.20:7878 for a network board you have joined",
    )
}

fn describe(logins: &[&Login]) -> String {
    logins
        .iter()
        .map(|login| format!("{} at {}", login.principal, login.url))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Chooses the board and credentials. `load` reads the login file and is
/// called only when a stored login could be needed.
pub fn resolve(
    wanted: &Wanted,
    load: impl FnOnce() -> Result<Store, Failure>,
) -> Result<Target, Failure> {
    if let Some(url) = &wanted.url {
        if !is_http(url) && !is_https(url) {
            return Err(bad_url(url, "must start with http:// or https://"));
        }
    }

    if let Some(token) = &wanted.token {
        let Some(url) = &wanted.url else {
            return Err(
                Failure::usage("BOARD_TOKEN is set but BOARD_URL is not").next(
                    "set BOARD_URL beside BOARD_TOKEN (and BOARD_FINGERPRINT for a board with a \
                     self-signed certificate), or unset BOARD_TOKEN to use a stored login",
                ),
            );
        };
        if !is_https(url) {
            return Err(Failure::usage(format!(
                "BOARD_TOKEN is set, but {url} is a plain-HTTP board, and a token is never sent \
                 without TLS"
            ))
            .next(
                "unset BOARD_TOKEN to use a local board, or set BOARD_URL to the network \
                 board's https:// URL",
            ));
        }
        let fingerprint =
            match &wanted.fingerprint {
                Some(fingerprint) if parse_fingerprint(fingerprint).is_some() => {
                    Some(fingerprint.clone())
                }
                Some(_) => return Err(Failure::usage(
                    "BOARD_FINGERPRINT is not a certificate fingerprint: it should be `sha256:` \
                     followed by 64 hex digits",
                )
                .next(
                    "copy it from `openboard admin info` on the board's host, or use `openboard \
                     join <invite>`, which carries it; unset it for a board whose certificate \
                     comes from a certificate authority",
                )),
                // A board known by its host name: its certificate is checked
                // against the certificate authorities.
                None => None,
            };
        return Ok(Target {
            url: url.trim_end_matches('/').to_owned(),
            fingerprint,
            token: Some(token.clone()),
            principal: None,
            credential_id: None,
            origin: Origin::Env,
            assumed: false,
        });
    }

    if let Some(url) = &wanted.url {
        if is_http(url) {
            return Ok(Target::local(url));
        }
    }

    let store = load()?;
    if wanted.url.is_none() && wanted.login.is_none() && store.logins.is_empty() {
        return Ok(Target {
            assumed: true,
            ..Target::local(DEFAULT_URL)
        });
    }
    let matches: Vec<&Login> = store
        .logins
        .iter()
        .filter(|login| {
            wanted
                .url
                .as_deref()
                .is_none_or(|url| same_url(&login.url, url))
                && wanted
                    .login
                    .as_deref()
                    .is_none_or(|name| same_principal(&login.principal, name))
        })
        .collect();
    match matches.as_slice() {
        [login] => Target::stored(login, store.path.clone()),
        [] => {
            let message = match (&wanted.url, &wanted.login) {
                (Some(url), Some(name)) => format!("no stored login for {url} as {name}"),
                (Some(url), None) => {
                    format!("no stored login for {url}, so there is no token to send")
                }
                (None, Some(name)) => format!("no stored login is named {name}"),
                (None, None) => "no stored login".to_owned(),
            };
            Err(Failure::usage(message).next(format!(
                "{GET_AN_INVITE}; `openboard logins` lists the logins you have"
            )))
        }
        several => {
            let urls_differ = several
                .iter()
                .any(|login| !same_url(&login.url, &several[0].url));
            let names_differ = several
                .iter()
                .any(|login| !same_principal(&login.principal, &several[0].principal));
            let pass = match (names_differ, urls_differ && wanted.url.is_none()) {
                (true, true) => {
                    "pass --login <name> (or set BOARD_LOGIN) and --url <url> (or BOARD_URL)"
                }
                (false, true) => "pass --url <url> (or set BOARD_URL)",
                _ => "pass --login <name> (or set BOARD_LOGIN)",
            };
            Err(Failure::new(
                codes::AMBIGUOUS_LOGIN,
                format!(
                    "{} stored logins match: {}",
                    several.len(),
                    describe(several)
                ),
            )
            .exit(EXIT_USAGE)
            .next(format!("{pass} to choose one")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp() -> String {
        openboard_api::auth::fingerprint(&[9; 32])
    }

    fn login(url: &str, principal: &str) -> Login {
        Login {
            url: url.into(),
            fingerprint: Some(fp()),
            principal: principal.into(),
            credential_id: format!("crd_{principal}"),
            token: Secret::new(format!("obt_secret_{principal}")),
            expires_at: "2027-01-01T00:00:00.000Z".into(),
        }
    }

    fn store(logins: Vec<Login>) -> impl FnOnce() -> Result<Store, Failure> {
        move || {
            Ok(Store {
                path: PathBuf::from("/cfg/logins.json"),
                logins,
            })
        }
    }

    fn never() -> Result<Store, Failure> {
        panic!("the login file must not be read")
    }

    fn wanted(url: Option<&str>, login: Option<&str>) -> Wanted {
        Wanted {
            url: url.map(str::to_owned),
            login: login.map(str::to_owned),
            ..Wanted::default()
        }
    }

    #[test]
    fn nothing_given_and_nothing_stored_is_the_local_default() {
        let target = resolve(&wanted(None, None), store(Vec::new())).unwrap();
        assert_eq!(target.url, DEFAULT_URL);
        assert_eq!(target.origin, Origin::Local);
        assert!(!target.is_network());
        // Only this case is a guess. A URL that was given is not.
        assert!(target.assumed);
        let given = resolve(&wanted(Some(DEFAULT_URL), None), never).unwrap();
        assert_eq!(given, Target::local(DEFAULT_URL));
        assert!(!given.assumed);
    }

    #[test]
    fn an_http_url_is_local_and_never_reads_logins_or_gets_a_token() {
        let target = resolve(&wanted(Some("http://127.0.0.1:9"), Some("agent-a")), never).unwrap();
        assert_eq!(target.origin, Origin::Local);
        assert!(target.token.is_none() && target.fingerprint.is_none());
    }

    #[test]
    fn the_only_stored_login_is_used_without_any_flag() {
        let target = resolve(
            &wanted(None, None),
            store(vec![login("https://b:1", "agent-a")]),
        )
        .unwrap();
        assert_eq!(target.url, "https://b:1");
        assert_eq!(target.principal.as_deref(), Some("agent-a"));
        assert_eq!(target.token.unwrap().expose(), "obt_secret_agent-a");
        assert_eq!(target.origin, Origin::Stored("/cfg/logins.json".into()));
    }

    #[test]
    fn url_and_login_filter_the_stored_logins() {
        let all = || {
            store(vec![
                login("https://b:1", "agent-a"),
                login("https://b:1", "agent-b"),
                login("https://c:1", "agent-a"),
            ])
        };
        let target = resolve(&wanted(Some("https://B:1/"), Some("Agent-B")), all()).unwrap();
        assert_eq!(target.principal.as_deref(), Some("agent-b"));
        let target = resolve(&wanted(Some("https://c:1"), None), all()).unwrap();
        assert_eq!(target.url, "https://c:1");
        let target = resolve(&wanted(None, Some("agent-b")), all()).unwrap();
        assert_eq!(target.url, "https://b:1");

        for (url, name, hint) in [
            (Some("https://b:1"), None, "pass --login <name>"),
            (None, Some("agent-a"), "pass --url <url>"),
            (
                None,
                None,
                "pass --login <name> (or set BOARD_LOGIN) and --url",
            ),
        ] {
            let failure = resolve(&wanted(url, name), all()).unwrap_err();
            assert_eq!(failure.code, codes::AMBIGUOUS_LOGIN);
            assert_eq!(failure.exit, EXIT_USAGE);
            assert!(
                failure.message.contains("agent-a at https://"),
                "{failure:?}"
            );
            assert!(
                failure.next.as_deref().unwrap().contains(hint),
                "{failure:?}"
            );
            assert!(!format!("{failure:?}").contains("obt_"), "{failure:?}");
        }
    }

    #[test]
    fn an_https_url_without_a_login_is_a_usage_error_pointing_at_join() {
        for logins in [Vec::new(), vec![login("https://other:1", "agent-a")]] {
            let failure = resolve(&wanted(Some("https://b:1"), None), store(logins)).unwrap_err();
            assert_eq!(failure.exit, EXIT_USAGE);
            assert!(failure.message.contains("no token"), "{failure:?}");
            assert!(failure.next.unwrap().contains("openboard join"));
        }
        let failure = resolve(
            &wanted(None, Some("nobody")),
            store(vec![login("https://b:1", "agent-a")]),
        )
        .unwrap_err();
        assert_eq!(failure.exit, EXIT_USAGE);
    }

    #[test]
    fn a_stored_login_without_a_fingerprint_is_checked_by_certificate_authorities() {
        let trusted = Login {
            fingerprint: None,
            ..login("https://board.example", "agent-a")
        };
        let target = resolve(&wanted(None, None), store(vec![trusted.clone()])).unwrap();
        assert!(target.is_network());
        assert_eq!(target.fingerprint, None);
        assert_eq!(target.token.unwrap().expose(), "obt_secret_agent-a");
        // A fingerprint that is stored but damaged is an error, not a
        // reason to ask the certificate authorities.
        let damaged = Login {
            fingerprint: Some("sha256:beef".into()),
            ..trusted
        };
        let failure = resolve(&wanted(None, None), store(vec![damaged])).unwrap_err();
        assert_eq!(failure.code, codes::LOGIN_FILE);
    }

    #[test]
    fn a_token_in_the_environment_wins_and_needs_a_url() {
        let env = |url: Option<&str>, fingerprint: Option<String>| Wanted {
            url: url.map(str::to_owned),
            login: None,
            token: Some(Secret::new("obt_envtoken".into())),
            fingerprint,
        };
        let target = resolve(&env(Some("https://b:1/"), Some(fp())), never).unwrap();
        assert_eq!(target.origin, Origin::Env);
        assert_eq!(target.url, "https://b:1");
        assert_eq!(target.fingerprint, Some(fp()));
        assert_eq!(target.token.unwrap().expose(), "obt_envtoken");
        // Without a fingerprint the board is known by its host name.
        let target = resolve(&env(Some("https://board.example"), None), never).unwrap();
        assert!(target.is_network());
        assert_eq!(target.fingerprint, None);
        assert_eq!(target.token.unwrap().expose(), "obt_envtoken");

        for wanted in [
            env(None, Some(fp())),
            // A fingerprint that is given must be one.
            env(Some("https://b:1"), Some("sha256:beef".into())),
            // A token is never sent over plain HTTP.
            env(Some("http://127.0.0.1:7878"), Some(fp())),
        ] {
            let failure = resolve(&wanted, never).unwrap_err();
            assert_eq!(failure.exit, EXIT_USAGE, "{failure:?}");
            assert!(!format!("{failure:?}").contains("envtoken"), "{failure:?}");
        }
    }

    #[test]
    fn other_schemes_are_usage_errors() {
        for bad in ["127.0.0.1:7878", "ftp://x", "httpx://y"] {
            let failure = resolve(&wanted(Some(bad), None), never).unwrap_err();
            assert_eq!(failure.exit, EXIT_USAGE, "{bad}");
        }
    }
}
