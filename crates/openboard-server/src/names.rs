//! Naming rules for boards, channels and profiles (design document, section 5).

/// Longest board name, in characters.
const MAX_BOARD_CHARS: usize = 64;
/// Longest channel path, in bytes. Paths are ASCII, so also in characters.
const MAX_CHANNEL_LEN: usize = 255;
/// Longest profile name.
const MAX_PROFILE_LEN: usize = 32;

/// A channel named in a query: one channel, or a channel and all below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChannelSel {
    Exact(String),
    /// The path without its trailing slash. Matches the path itself and
    /// every channel below it.
    Subtree(String),
}

/// Board names are 1 to 64 characters with no `/` and no control characters.
pub(crate) fn validate_board(name: &str) -> Result<(), &'static str> {
    let chars = name.chars().count();
    if chars == 0 || chars > MAX_BOARD_CHARS {
        return Err("a board name is 1 to 64 characters");
    }
    if name.contains('/') {
        return Err("a board name must not contain '/'");
    }
    if name.chars().any(char::is_control) {
        return Err("a board name must not contain control characters");
    }
    Ok(())
}

/// Boards are unique without regard to case. This is the comparison key.
pub(crate) fn board_key(name: &str) -> String {
    name.to_lowercase()
}

/// A channel path is segments of lowercase letters, digits, `_` and `-`
/// joined by `/`.
pub(crate) fn validate_channel(path: &str) -> Result<(), &'static str> {
    if path.is_empty() || path.len() > MAX_CHANNEL_LEN {
        return Err("a channel path is 1 to 255 characters");
    }
    let segment_ok = |segment: &str| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    };
    if path.split('/').all(segment_ok) {
        Ok(())
    } else {
        Err("a channel path is segments of lowercase letters, digits, '_' and '-' joined by '/'")
    }
}

/// Parses a channel as written in a query, where a trailing slash names a
/// subtree.
pub(crate) fn parse_channel_sel(raw: &str) -> Result<ChannelSel, &'static str> {
    match raw.strip_suffix('/') {
        Some(prefix) => validate_channel(prefix).map(|()| ChannelSel::Subtree(prefix.to_owned())),
        None => validate_channel(raw).map(|()| ChannelSel::Exact(raw.to_owned())),
    }
}

/// A profile starts with a letter and has up to 32 letters, digits and hyphens.
pub(crate) fn validate_profile(name: &str) -> Result<(), &'static str> {
    let starts_with_letter = name.bytes().next().is_some_and(|b| b.is_ascii_alphabetic());
    let rest_ok = name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if starts_with_letter && rest_ok && name.len() <= MAX_PROFILE_LEN {
        Ok(())
    } else {
        Err("a profile starts with a letter and has up to 32 letters, digits and hyphens")
    }
}

/// Handles no principal may take: the local principal's, and the name the
/// server uses for a caller it has not authenticated.
const RESERVED_HANDLES: [&str; 2] = ["local", "anonymous"];

/// A principal's handle follows the profile rule and is not a reserved name.
pub(crate) fn validate_handle(name: &str) -> Result<(), &'static str> {
    validate_profile(name)
        .map_err(|_| "a name starts with a letter and has up to 32 letters, digits and hyphens")?;
    if RESERVED_HANDLES.contains(&handle_key(name).as_str()) {
        return Err("that name is reserved");
    }
    Ok(())
}

/// Handles are unique without regard to case. This is the comparison key.
pub(crate) fn handle_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_names() {
        assert!(validate_board("general").is_ok());
        assert!(validate_board("Team Board 2").is_ok());
        assert!(validate_board(&"é".repeat(64)).is_ok());
        assert!(validate_board("").is_err());
        assert!(validate_board(&"x".repeat(65)).is_err());
        assert!(validate_board("a/b").is_err());
        assert!(validate_board("a\nb").is_err());
        assert_eq!(board_key("General"), board_key("gENERAL"));
    }

    #[test]
    fn channel_paths() {
        for ok in ["a", "research/web", "a_b/c-d/9"] {
            assert!(validate_channel(ok).is_ok(), "{ok}");
        }
        for bad in ["", "/", "a/", "/a", "a//b", "A", "a b", "a.b", "é"] {
            assert!(validate_channel(bad).is_err(), "{bad}");
        }
        assert!(validate_channel(&"a".repeat(256)).is_err());
    }

    #[test]
    fn channel_selectors() {
        assert_eq!(
            parse_channel_sel("a/b"),
            Ok(ChannelSel::Exact("a/b".into()))
        );
        assert_eq!(
            parse_channel_sel("a/b/"),
            Ok(ChannelSel::Subtree("a/b".into()))
        );
        assert!(parse_channel_sel("/").is_err());
        assert!(parse_channel_sel("a//").is_err());
    }

    #[test]
    fn profile_names() {
        for ok in ["a", "fx", "Worker-2", &"a".repeat(32)] {
            assert!(validate_profile(ok).is_ok(), "{ok}");
        }
        for bad in ["", "1a", "-a", "a_b", "a/b", "a b", &"a".repeat(33)] {
            assert!(validate_profile(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn handles() {
        for ok in ["a", "research-1", "Builder"] {
            assert!(validate_handle(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "1a",
            "a_b",
            "a/b",
            "local",
            "LOCAL",
            "Anonymous",
            &"a".repeat(33),
        ] {
            assert!(validate_handle(bad).is_err(), "{bad}");
        }
        assert_eq!(handle_key("Builder"), handle_key("bUILDER"));
    }
}
