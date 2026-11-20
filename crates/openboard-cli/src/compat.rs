//! What this CLI and the board know about each other's versions.
//!
//! The board says what it is on every response, in two headers (see
//! `openboard_api`'s crate documentation). The client reads them from
//! whatever answer a command gets anyway; nothing here makes a request.
//!
//! Two things follow from them:
//!
//! - When the board's API level is higher than this CLI's, the CLI says so
//!   once, on stderr: a newer CLI exists and this one may lack commands.
//! - When the board's API level is lower than a command needs, the failure
//!   that results gets a hint that says so; see [`REQUIREMENTS`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use openboard_api::{build, API_LEVEL, API_LEVEL_HEADER, VERSION_HEADER};
use reqwest::header::HeaderMap;

use crate::error::Failure;

/// Set to `1` to keep the "a newer CLI is available" line off stderr.
pub const NO_NOTICE_ENV: &str = "OPENBOARD_NO_UPGRADE_NOTICE";

/// Where upgrading is explained, as a path in the repository.
pub const UPGRADING_DOC: &str = "docs/upgrading.md";

/// The repository, for pointing at [`UPGRADING_DOC`].
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// What a board said about itself in the headers of a response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    /// `None` from a server older than 0.1.0, which sends no such header.
    pub version: Option<String>,
    /// `None` from a server older than API level 1.
    pub api_level: Option<u32>,
}

impl ServerInfo {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let text = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty() && value.len() <= 64)
        };
        Self {
            version: text(VERSION_HEADER).map(clean),
            api_level: text(API_LEVEL_HEADER).and_then(|value| value.parse().ok()),
        }
    }

    /// The level as a number. A board that reports none is older than the
    /// reporting, which came with level 1.
    pub fn level(&self) -> u32 {
        self.api_level.unwrap_or(0)
    }

    /// For example `Openboard 0.1.0 (API level 1)`.
    pub fn describe(&self) -> String {
        match (&self.version, self.api_level) {
            (Some(version), Some(level)) => format!("Openboard {version} (API level {level})"),
            (Some(version), None) => format!("Openboard {version}"),
            (None, Some(level)) => format!("an Openboard with API level {level}"),
            (None, None) => {
                "a version of Openboard older than 0.1.0, which reports no API level".to_owned()
            }
        }
    }
}

/// A header value from the board, safe to print: what a version is made of.
fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
        .collect()
}

/// What the board has said so far in this invocation.
#[derive(Debug, Default)]
pub struct Seen {
    info: Mutex<Option<ServerInfo>>,
    noticed: AtomicBool,
}

impl Seen {
    /// Takes note of a response's headers, and says once, on stderr, when
    /// they show a board newer than this CLI.
    pub fn record(&self, headers: &HeaderMap) {
        let info = ServerInfo::from_headers(headers);
        let newer = info.level() > API_LEVEL;
        let notice = newer.then(|| upgrade_notice(&info));
        *self
            .info
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(info);
        if let Some(notice) = notice {
            let silenced = std::env::var(NO_NOTICE_ENV).is_ok_and(|value| value.trim() == "1");
            if !silenced && !self.noticed.swap(true, Ordering::AcqRel) {
                eprintln!("{notice}");
            }
        }
    }

    /// What the board said last. `None` until it has answered anything.
    pub fn info(&self) -> Option<ServerInfo> {
        self.info
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

/// The one line that says a newer CLI exists.
pub fn upgrade_notice(server: &ServerInfo) -> String {
    format!(
        "openboard: the board runs {}, newer than this openboard ({}, API level {API_LEVEL}): a \
         newer CLI is available and this one may lack commands. To upgrade, see {UPGRADING_DOC} \
         in the repository ({REPOSITORY}); `openboard version` shows both versions. Set \
         {NO_NOTICE_ENV}=1 to silence this line.",
        server.describe(),
        build::VERSION,
    )
}

/// Something a command or a flag needs from the board, and the API level
/// that brought it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requirement {
    /// What needs it, as the hint names it.
    pub what: &'static str,
    pub min_api_level: u32,
}

/// The commands and flags that need a board newer than the first ones
/// that existed. A later milestone adds a row here for each route or
/// parameter it introduces, and passes the row to `Client::needs` in the
/// `map_err` of the request that uses it, for example
/// `.map_err(|failure| client.needs(&REQUIREMENTS[1], failure))`. A
/// `not_found` or `invalid_request` answer from a board below the row's
/// level then says the board is too old instead of blaming the input.
/// Everything not listed works with any board.
pub const REQUIREMENTS: &[Requirement] = &[
    // The level that introduced levels: an older board answers the health
    // check without them.
    Requirement {
        what: "showing the board's commit and API level in `openboard version`",
        min_api_level: 1,
    },
    Requirement {
        what: "`openboard tasks`, `wait --label` and `read <message-id>`",
        min_api_level: 2,
    },
    Requirement {
        what: "`openboard grant`, `grants`, `ungrant`, `explain` and `channels create`",
        min_api_level: 3,
    },
];

/// The row for the board's own build information.
pub const SERVER_BUILD_INFO: &Requirement = &REQUIREMENTS[0];
/// The row for tasks and what came with them.
pub const TASKS: &Requirement = &REQUIREMENTS[1];
/// Grants, `explain` and channel creation: API level 3.
pub const GRANTS: &Requirement = &REQUIREMENTS[2];

/// Whether `server` is too old for `requirement`.
pub fn too_old(server: &ServerInfo, requirement: &Requirement) -> bool {
    server.level() < requirement.min_api_level
}

/// What to do when the board is older than a command needs.
pub fn older_board_hint(server: &ServerInfo, requirement: &Requirement) -> String {
    format!(
        "the board is running an older version of Openboard than this command needs: it runs \
         {}, and {} needs API level {}. Ask the board's operator to upgrade the server (see \
         {UPGRADING_DOC} in the repository); `openboard version` shows both versions",
        server.describe(),
        requirement.what,
        requirement.min_api_level,
    )
}

/// Replaces the hint of a failure that an older board explains: the board
/// answered that it has no such route, or that it does not understand the
/// request, to a command it is too old for. A route the board's level does
/// not have can only be missing, and a parameter it does not have can only
/// be refused, so no other reading of the answer is likely.
pub fn explain(
    failure: Failure,
    server: Option<&ServerInfo>,
    requirement: &Requirement,
) -> Failure {
    let from_board = failure.status.is_some();
    let unknown_to_board = failure.code == openboard_api::codes::NOT_FOUND
        || failure.code == openboard_api::codes::INVALID_REQUEST;
    match server {
        Some(server) if from_board && unknown_to_board && too_old(server, requirement) => {
            let hint = older_board_hint(server, requirement);
            failure.next(hint)
        }
        _ => failure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn headers(version: Option<&'static str>, level: Option<&'static str>) -> HeaderMap {
        let mut map = HeaderMap::new();
        if let Some(version) = version {
            map.insert(VERSION_HEADER, HeaderValue::from_static(version));
        }
        if let Some(level) = level {
            map.insert(API_LEVEL_HEADER, HeaderValue::from_static(level));
        }
        map
    }

    const LATER: Requirement = Requirement {
        what: "`openboard frobnicate`",
        min_api_level: 7,
    };

    #[test]
    fn the_headers_are_read_and_a_board_without_them_is_level_zero() {
        let new = ServerInfo::from_headers(&headers(Some("0.3.0"), Some("4")));
        assert_eq!(new.version.as_deref(), Some("0.3.0"));
        assert_eq!(new.level(), 4);
        assert_eq!(new.describe(), "Openboard 0.3.0 (API level 4)");
        let old = ServerInfo::from_headers(&headers(None, None));
        assert_eq!(old, ServerInfo::default());
        assert_eq!(old.level(), 0);
        assert!(old.describe().contains("older than 0.1.0"));
        // A header that is not a number is no level, and a version is cut
        // down to what a version is made of.
        let odd = ServerInfo::from_headers(&headers(Some("1.0 <b>;"), Some("many")));
        assert_eq!(odd.version.as_deref(), Some("1.0b"));
        assert_eq!(odd.api_level, None);
    }

    #[test]
    fn the_notice_is_one_line_that_says_how_to_upgrade_and_how_to_silence_it() {
        let server = ServerInfo {
            version: Some("0.9.0".into()),
            api_level: Some(API_LEVEL + 1),
        };
        let notice = upgrade_notice(&server);
        assert_eq!(notice.lines().count(), 1, "{notice}");
        for needle in [
            "a newer CLI is available",
            "docs/upgrading.md",
            "`openboard version`",
            "OPENBOARD_NO_UPGRADE_NOTICE=1",
            "Openboard 0.9.0",
        ] {
            assert!(notice.contains(needle), "{needle}: {notice}");
        }
    }

    #[test]
    fn a_missing_route_on_an_older_board_is_explained() {
        let mut missing = Failure::new(openboard_api::codes::NOT_FOUND, "no such route")
            .next("see the route table in openboard-api");
        missing.status = Some(404);
        let old = ServerInfo::default();
        let explained = explain(missing.clone(), Some(&old), &LATER);
        let hint = explained.next.unwrap();
        assert!(hint.contains("older version of Openboard"), "{hint}");
        assert!(hint.contains("upgrade the server"), "{hint}");
        assert!(
            hint.contains("`openboard frobnicate` needs API level 7"),
            "{hint}"
        );
        assert!(hint.contains("older than 0.1.0"), "{hint}");

        // A board at a lower level says which.
        let lower = ServerInfo {
            version: Some("0.1.0".into()),
            api_level: Some(1),
        };
        let mut refused = Failure::new(openboard_api::codes::INVALID_REQUEST, "unknown field");
        refused.status = Some(400);
        let hint = explain(refused, Some(&lower), &LATER).next.unwrap();
        assert!(hint.contains("Openboard 0.1.0 (API level 1)"), "{hint}");

        // A board that is new enough keeps its own hint, and so does any
        // other failure, and one that never reached a board.
        let enough = ServerInfo {
            version: Some("1.0.0".into()),
            api_level: Some(7),
        };
        let kept = explain(missing.clone(), Some(&enough), &LATER);
        assert_eq!(kept.next, missing.next);
        let mut denied = Failure::new(openboard_api::codes::FORBIDDEN, "no").next("ask");
        denied.status = Some(403);
        assert_eq!(
            explain(denied, Some(&old), &LATER).next.as_deref(),
            Some("ask")
        );
        let offline = Failure::new(openboard_api::codes::NOT_FOUND, "made up here").next("x");
        assert_eq!(
            explain(offline, Some(&old), &LATER).next.as_deref(),
            Some("x")
        );
        assert_eq!(explain(missing.clone(), None, &LATER).next, missing.next);
    }

    #[test]
    fn every_requirement_names_what_needs_it() {
        for requirement in REQUIREMENTS {
            assert!(!requirement.what.is_empty());
            assert!(requirement.min_api_level >= 1);
            assert!(requirement.min_api_level <= API_LEVEL);
        }
        assert!(too_old(&ServerInfo::default(), SERVER_BUILD_INFO));
    }
}
