//! Which build this is: the version, the commit and the date.
//!
//! The server and the CLI are built from one workspace and share this
//! module, so `openboard --version`, `obd --version`, [`crate::Health`] and
//! the [`crate::VERSION_HEADER`] response header all say the same thing
//! about one build. The values are fixed at compile time by this crate's
//! build script; see `build.rs` for where they come from.

/// The release version, `MAJOR.MINOR.PATCH`. One for the whole workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit the build was made from: 12 hexadecimal characters, with
/// `-dirty` added when the tree had uncommitted changes, or
/// [`UNKNOWN`] when the build had no git repository to ask.
pub const COMMIT: &str = env!("OPENBOARD_BUILT_COMMIT");

/// The UTC day of that commit, `YYYY-MM-DD`, or of `SOURCE_DATE_EPOCH` when
/// the build set it, or [`UNKNOWN`]. Never the time of the build itself.
pub const BUILD_DATE: &str = env!("OPENBOARD_BUILT_DATE");

/// What [`COMMIT`] and [`BUILD_DATE`] say when the build could not tell.
pub const UNKNOWN: &str = "unknown";

/// [`COMMIT`], unless it is [`UNKNOWN`].
pub fn commit() -> Option<&'static str> {
    (COMMIT != UNKNOWN).then_some(COMMIT)
}

/// [`BUILD_DATE`], unless it is [`UNKNOWN`].
pub fn build_date() -> Option<&'static str> {
    (BUILD_DATE != UNKNOWN).then_some(BUILD_DATE)
}

/// The build on one line, for example `0.1.0 (3ad55f3c0a1b 2026-11-19)`.
/// A build that knows neither commit nor date is `0.1.0 (unknown build)`.
/// The same text as [`describe`], as a constant.
pub const DESCRIBE: &str = env!("OPENBOARD_BUILT_DESCRIBE");

/// [`DESCRIBE`], as an owned string.
pub fn describe() -> String {
    DESCRIBE.to_owned()
}

/// [`describe`] for any build, such as the one a server reports about
/// itself. An empty or [`UNKNOWN`] part counts as absent.
pub fn describe_parts(version: &str, commit: Option<&str>, date: Option<&str>) -> String {
    fn known(part: Option<&str>) -> Option<&str> {
        part.filter(|text| !text.is_empty() && *text != UNKNOWN)
    }
    match (known(commit), known(date)) {
        (Some(commit), Some(date)) => format!("{version} ({commit} {date})"),
        (Some(only), None) | (None, Some(only)) => format!("{version} ({only})"),
        (None, None) => format!("{version} (unknown build)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_is_described_on_one_line() {
        assert_eq!(
            describe_parts("0.1.0", Some("3ad55f3c0a1b"), Some("2026-11-19")),
            "0.1.0 (3ad55f3c0a1b 2026-11-19)"
        );
        assert_eq!(
            describe_parts("0.1.0", Some("3ad55f3c0a1b-dirty"), None),
            "0.1.0 (3ad55f3c0a1b-dirty)"
        );
        assert_eq!(
            describe_parts("0.1.0", Some(UNKNOWN), Some("")),
            "0.1.0 (unknown build)"
        );
        let line = describe();
        assert_eq!(line, describe_parts(VERSION, commit(), build_date()));
        assert!(line.starts_with(VERSION), "{line}");
        assert!(!line.contains('\n'), "{line}");
    }

    #[test]
    fn the_build_script_recorded_a_commit_and_a_date() {
        // Either a real value or the word that says there is none.
        let (commit, date) = (String::from(COMMIT), String::from(BUILD_DATE));
        assert!(!commit.is_empty() && !date.is_empty());
        assert!(COMMIT
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')));
        if let Some(date) = build_date() {
            let shape: Vec<usize> = date.split('-').map(str::len).collect();
            assert_eq!(shape, [4, 2, 2], "{date}");
        }
    }
}
