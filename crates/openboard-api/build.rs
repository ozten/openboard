//! Records which commit and date this build is from, for
//! `openboard_api::build`. Uses no crates: only `git`, when there is one.
//!
//! The commit is, in this order:
//!
//! 1. `OPENBOARD_BUILD_COMMIT` from the environment, as the release workflow
//!    sets it. A full hexadecimal hash is cut to 12 characters.
//! 2. `git rev-parse --short=12 HEAD`, with `-dirty` added when a tracked
//!    file has uncommitted changes. Only when this crate sits in the git
//!    work tree of its own workspace, so that a source tarball unpacked
//!    inside some other repository does not take that repository's commit.
//! 3. `unknown`: there is no git, or no repository.
//!
//! The date is `SOURCE_DATE_EPOCH` when set, else the commit's date, both
//! as a UTC calendar day, else `unknown`. It is never the wall clock, so two
//! builds of one commit carry the same text.
//!
//! The script runs again when the environment variables change and when
//! `HEAD` moves: a commit, a checkout, a reset. It does not watch the work
//! tree, so that editing a file does not rebuild every crate twice; the
//! `-dirty` mark therefore says how the tree was when `HEAD` last moved or
//! this crate was last rebuilt, not necessarily how it is now.

use std::path::{Path, PathBuf};
use std::process::Command;

const UNKNOWN: &str = "unknown";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=OPENBOARD_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let repository = own_repository(&manifest_dir);
    if repository {
        watch_head(&manifest_dir);
    }

    let commit = env_text("OPENBOARD_BUILD_COMMIT")
        .map(|given| tidy_commit(&given))
        .filter(|commit| !commit.is_empty())
        .or_else(|| repository.then(|| git_commit(&manifest_dir)).flatten())
        .unwrap_or_else(|| UNKNOWN.to_owned());

    let epoch = env_text("SOURCE_DATE_EPOCH")
        .and_then(|text| text.parse::<i64>().ok())
        .or_else(|| {
            repository
                .then(|| git(&manifest_dir, &["show", "-s", "--format=%ct", "HEAD"]))
                .flatten()
                .and_then(|text| text.parse::<i64>().ok())
        });
    let date = epoch.map_or_else(|| UNKNOWN.to_owned(), utc_day);

    // The one-line description, as `build::describe_parts` writes it, so
    // that it can be a constant (clap's `--version` wants one).
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let known = |part: &str| (part != UNKNOWN).then(|| part.to_owned());
    let describe = match (known(&commit), known(&date)) {
        (Some(commit), Some(date)) => format!("{version} ({commit} {date})"),
        (Some(only), None) | (None, Some(only)) => format!("{version} ({only})"),
        (None, None) => format!("{version} (unknown build)"),
    };

    println!("cargo:rustc-env=OPENBOARD_BUILT_COMMIT={commit}");
    println!("cargo:rustc-env=OPENBOARD_BUILT_DATE={date}");
    println!("cargo:rustc-env=OPENBOARD_BUILT_DESCRIBE={describe}");
}

fn env_text(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Runs git in `dir`. `None` when git is missing, fails or prints nothing.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Whether the workspace this crate belongs to is itself a git work tree.
fn own_repository(manifest_dir: &Path) -> bool {
    let Some(top) = git(manifest_dir, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    let workspace = manifest_dir.join("..").join("..");
    match (std::fs::canonicalize(top), std::fs::canonicalize(workspace)) {
        (Ok(top), Ok(workspace)) => top == workspace,
        _ => false,
    }
}

/// Asks cargo to run this script again when `HEAD` moves. Only files that
/// exist are named: cargo treats a missing one as always changed.
fn watch_head(manifest_dir: &Path) {
    let mut names = vec![
        "HEAD".to_owned(),
        "logs/HEAD".to_owned(),
        "packed-refs".to_owned(),
    ];
    if let Some(branch) = git(manifest_dir, &["symbolic-ref", "-q", "HEAD"]) {
        names.push(branch);
    }
    for name in names {
        let Some(path) = git(manifest_dir, &["rev-parse", "--git-path", &name]) else {
            continue;
        };
        let path = manifest_dir.join(path);
        if path.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn git_commit(manifest_dir: &Path) -> Option<String> {
    let commit = git(manifest_dir, &["rev-parse", "--short=12", "HEAD"])?;
    let dirty = Command::new("git")
        .arg("-C")
        .arg(manifest_dir)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .is_some_and(|output| output.status.success() && !output.stdout.is_empty());
    let commit = tidy_commit(&commit);
    Some(if dirty {
        format!("{commit}-dirty")
    } else {
        commit
    })
}

/// Keeps what a commit name is made of, and shortens a full hash.
fn tidy_commit(raw: &str) -> String {
    let kept: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        .take(64)
        .collect();
    if kept.len() > 12 && kept.chars().all(|c| c.is_ascii_hexdigit()) {
        kept[..12].to_owned()
    } else {
        kept
    }
}

/// The UTC calendar day of a Unix time, as `YYYY-MM-DD`.
fn utc_day(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    // Civil-from-days, after Howard Hinnant's public domain algorithm.
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}
