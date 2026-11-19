//! What this CLI is: `openboard version` and `openboard skill`.
//!
//! `version` asks the board for its own version as well, with the login
//! and URL any other command would use, and reports a board it cannot
//! reach instead of failing. `skill` prints or installs the agent skill
//! file built into this binary, so the instructions an agent reads always
//! match the CLI it runs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use openboard_api::{build, API_LEVEL};
use serde::Serialize;

use crate::cli::{Global, SkillArgs};
use crate::client::Client;
use crate::compat::{self, ServerInfo, SERVER_BUILD_INFO};
use crate::error::{codes, Failure};
use crate::logins::Store;
use crate::target;
use crate::{render, to_json, wanted, write_stdout};

/// The skill file as it is in the repository.
const SKILL_SOURCE: &str = include_str!("../../../skills/openboard/SKILL.md");

/// Longest time `version` waits for the board.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// This build, as `version --json` reports it.
#[derive(Debug, Serialize)]
struct ClientBuild {
    version: &'static str,
    commit: Option<&'static str>,
    build_date: Option<&'static str>,
    api_level: u32,
}

/// The board, as `version --json` reports it.
#[derive(Debug, Default, Serialize)]
struct BoardBuild {
    /// `None` when no board could even be chosen, for example with several
    /// stored logins and no `--login`.
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    /// `None` from a board older than API level 1, which reports none.
    #[serde(skip_serializing_if = "Option::is_none")]
    api_level: Option<u32>,
    /// What to do about the difference between the two, if anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    /// Why the board could not be asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorDetail>,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
}

impl From<Failure> for ErrorDetail {
    fn from(failure: Failure) -> Self {
        Self {
            code: failure.code,
            message: failure.message,
            next: failure.next,
        }
    }
}

/// Asks the board what it runs. Never fails: what went wrong is in the
/// answer.
async fn ask_board(global: &Global) -> BoardBuild {
    let target = match target::resolve(&wanted(global), Store::load) {
        Ok(target) => target,
        Err(failure) => {
            return BoardBuild {
                error: Some(failure.into()),
                ..BoardBuild::default()
            }
        }
    };
    let mut board = BoardBuild {
        url: Some(target.url.clone()),
        ..BoardBuild::default()
    };
    let client = match Client::new(&target, global.board(), global.profile()) {
        Ok(client) => client,
        Err(failure) => {
            board.error = Some(failure.into());
            return board;
        }
    };
    let health = match tokio::time::timeout(VERSION_TIMEOUT, client.health()).await {
        Ok(Ok(health)) => health,
        Ok(Err(failure)) => {
            board.error = Some(client.needs(SERVER_BUILD_INFO, failure).into());
            return board;
        }
        Err(_) => {
            board.error = Some(
                Failure::new(
                    codes::REQUEST_TIMEOUT,
                    format!("the board at {} did not answer in time", target.url),
                )
                .into(),
            );
            return board;
        }
    };
    let headers = client.server().unwrap_or_default();
    let info = ServerInfo {
        version: Some(render::safe(&health.version)),
        api_level: health.api_level.or(headers.api_level),
    };
    board.reachable = true;
    board.version.clone_from(&info.version);
    board.commit = health.commit.as_deref().map(render::safe);
    board.api_level = info.api_level;
    board.note = if compat::too_old(&info, SERVER_BUILD_INFO) {
        Some(compat::older_board_hint(&info, SERVER_BUILD_INFO))
    } else if info.level() > API_LEVEL {
        Some(compat::upgrade_notice(&info))
    } else {
        None
    };
    board
}

/// `openboard version`, and `openboard --version --json`.
pub async fn version(global: &Global) -> Result<(), Failure> {
    let client = ClientBuild {
        version: build::VERSION,
        commit: build::commit(),
        build_date: build::build_date(),
        api_level: API_LEVEL,
    };
    let board = ask_board(global).await;
    if global.json {
        #[derive(Serialize)]
        struct Versions {
            client: ClientBuild,
            board: BoardBuild,
        }
        return write_stdout(&to_json(&Versions { client, board })?);
    }
    let mut out = format!("openboard {}, API level {API_LEVEL}", build::DESCRIBE);
    let at = board
        .url
        .as_deref()
        .map(|url| format!(" at {}", render::safe(url)))
        .unwrap_or_default();
    match (&board.error, &board.version) {
        (Some(error), _) => {
            out.push_str(&format!(
                "\nboard{at}: not reachable ({}): {}",
                error.code, error.message
            ));
            if let Some(next) = &error.next {
                out.push_str(&format!("\nnext: {next}"));
            }
        }
        (None, Some(version)) => {
            let described = match &board.commit {
                Some(commit) => format!("{version} ({commit})"),
                None => version.clone(),
            };
            let level = match board.api_level {
                Some(level) => format!("API level {level}"),
                None => "no API level reported".to_owned(),
            };
            out.push_str(&format!("\nboard{at}: Openboard {described}, {level}"));
        }
        (None, None) => {}
    }
    if let Some(note) = &board.note {
        out.push_str(&format!("\nnote: {note}"));
    }
    write_stdout(&out)
}

/// The skill file with its version named in the front matter: a line
/// `openboard-version: ...` right after the opening `---`.
pub fn skill_text() -> String {
    let line = format!("openboard-version: {}", build::DESCRIBE);
    match SKILL_SOURCE.strip_prefix("---\n") {
        Some(rest) => format!("---\n{line}\n{rest}"),
        None => format!("---\n{line}\n---\n\n{SKILL_SOURCE}"),
    }
}

/// Where `--install <dir>` writes the skill.
fn install_path(dir: &Path) -> PathBuf {
    dir.join("openboard").join("SKILL.md")
}

/// `openboard skill [--install <dir> [--force]]`.
pub fn skill(json: bool, args: &SkillArgs) -> Result<(), Failure> {
    let text = skill_text();
    #[derive(Serialize)]
    struct Skill<'a> {
        version: &'static str,
        commit: Option<&'static str>,
        text: &'a str,
        /// Where `--install` put it.
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// Whether `--install` wrote the file, rather than finding it
        /// already the same.
        #[serde(skip_serializing_if = "Option::is_none")]
        written: Option<bool>,
    }
    let Some(dir) = &args.install else {
        if json {
            return write_stdout(&to_json(&Skill {
                version: build::VERSION,
                commit: build::commit(),
                text: &text,
                path: None,
                written: None,
            })?);
        }
        return write_stdout(text.trim_end_matches('\n'));
    };
    let path = install_path(dir);
    let shown = path.display().to_string();
    let io = |what: &str, error: std::io::Error| {
        Failure::new(codes::IO, format!("cannot {what} {shown}: {error}"))
            .next("check that the directory given to --install is writable")
    };
    let written = match std::fs::read(&path) {
        Ok(existing) if existing == text.as_bytes() => false,
        Ok(_) if !args.force => {
            return Err(Failure::new(
                codes::FILE_EXISTS,
                format!("{shown} already exists and differs from the skill of this openboard"),
            )
            .next(format!(
                "compare it with `openboard skill`, then add --force to replace it: `openboard \
                 skill --install {} --force`",
                crate::shell_quote(&dir.display().to_string())
            )));
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(io("read", error)),
    };
    if written {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| io("create the directory for", error))?;
        }
        std::fs::write(&path, &text).map_err(|error| io("write", error))?;
    }
    if json {
        return write_stdout(&to_json(&Skill {
            version: build::VERSION,
            commit: build::commit(),
            text: &text,
            path: Some(shown),
            written: Some(written),
        })?);
    }
    write_stdout(&if written {
        format!(
            "installed the skill for openboard {} in {shown}",
            build::DESCRIBE
        )
    } else {
        format!(
            "{shown} is already the skill for openboard {}; nothing changed",
            build::DESCRIBE
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skill_names_the_version_it_documents_in_its_front_matter() {
        let text = skill_text();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("---"));
        assert_eq!(
            lines.next().map(str::to_owned),
            Some(format!("openboard-version: {}", build::DESCRIBE))
        );
        assert_eq!(lines.next(), Some("name: openboard"));
        assert!(text.contains("\n---\n"));
    }

    #[test]
    fn install_refuses_to_overwrite_a_different_file_without_force() {
        let dir = tempfile::TempDir::new().unwrap();
        let args = |force| SkillArgs {
            install: Some(dir.path().to_owned()),
            force,
        };
        let path = install_path(dir.path());
        skill(true, &args(false)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), skill_text());
        // The same file again is fine.
        skill(true, &args(false)).unwrap();
        std::fs::write(&path, "my own notes").unwrap();
        let refused = skill(true, &args(false)).unwrap_err();
        assert_eq!(refused.code, codes::FILE_EXISTS);
        assert!(refused.next.unwrap().contains("--force"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "my own notes");
        skill(true, &args(true)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), skill_text());
    }
}
