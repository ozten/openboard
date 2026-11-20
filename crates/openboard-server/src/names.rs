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

/// Longest label key, in bytes. Keys are ASCII, so also in characters.
const MAX_LABEL_KEY_LEN: usize = 64;
/// Longest label value, in bytes.
const MAX_LABEL_VALUE_LEN: usize = 256;
/// Longest reaction name.
const MAX_REACTION_LEN: usize = 32;
/// Label keys under this prefix are reserved for the board itself.
const RESERVED_LABEL_PREFIX: &str = "board.";

/// A label key is 1 to 64 characters from lowercase letters, digits, `.`,
/// `_` and `-`, and does not start with `board.`.
pub(crate) fn validate_label_key(key: &str) -> Result<(), &'static str> {
    let shaped = !key.is_empty()
        && key.len() <= MAX_LABEL_KEY_LEN
        && key.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        });
    if !shaped {
        return Err(
            "a label key is 1 to 64 characters from lowercase letters, digits, '.', '_' and '-'",
        );
    }
    if key.starts_with(RESERVED_LABEL_PREFIX) {
        return Err("label keys that start with 'board.' are reserved");
    }
    Ok(())
}

/// A label key in a filter: a label key, reserved or not.
pub(crate) fn validate_label_filter_key(key: &str) -> Result<(), &'static str> {
    match key.strip_prefix("board.") {
        Some(rest) => validate_label_key(rest),
        None => validate_label_key(key),
    }
}

/// A label value is 1 to 256 bytes with no control characters.
pub(crate) fn validate_label_value(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.len() > MAX_LABEL_VALUE_LEN || value.chars().any(char::is_control)
    {
        Err("a label value is 1 to 256 bytes with no control characters")
    } else {
        Ok(())
    }
}

/// A reaction name is 1 to 32 characters from lowercase letters, digits,
/// `_`, `+` and `-`.
pub(crate) fn validate_reaction(name: &str) -> Result<(), &'static str> {
    let shaped = !name.is_empty()
        && name.len() <= MAX_REACTION_LEN
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'+' | b'-')
        });
    if shaped {
        Ok(())
    } else {
        Err("a reaction name is 1 to 32 characters from lowercase letters, digits, '_', '+' and '-'")
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

/// Longest email address, in bytes.
const MAX_EMAIL_LEN: usize = 254;

/// Checks a person's email address and returns it lowercased, which is the
/// person's handle.
///
/// The check is loose about what an address is and strict about what a
/// handle may hold. The address is at most 254 bytes of ASCII letters,
/// digits and `.`, `_`, `+`, `-`, with exactly one `@`, and every character
/// that is not a letter or digit stands between two that are. So the local
/// part and the domain are not empty, there is no whitespace or control
/// character, and `@` followed by the address is always read as one mention
/// of it: the mention grammar matches every character, nothing is trimmed
/// from its end, and no run of punctuation can be taken for Markdown.
pub(crate) fn validate_email(address: &str) -> Result<String, &'static str> {
    const SHAPE: &str = "an email address is letters, digits and single '.', '_', '+' or '-' \
                         between them, around one '@'";
    if address.is_empty() || address.len() > MAX_EMAIL_LEN {
        return Err("an email address is 1 to 254 characters");
    }
    let bytes = address.as_bytes();
    if !bytes.iter().all(|b| crate::markdown::is_handle_byte(*b)) {
        return Err(SHAPE);
    }
    let alphanumeric_at = |at: Option<usize>| {
        at.and_then(|at| bytes.get(at))
            .is_some_and(u8::is_ascii_alphanumeric)
    };
    let separators_are_enclosed = bytes.iter().enumerate().all(|(at, b)| {
        b.is_ascii_alphanumeric()
            || (alphanumeric_at(at.checked_sub(1)) && alphanumeric_at(Some(at + 1)))
    });
    if !separators_are_enclosed || bytes.iter().filter(|b| **b == b'@').count() != 1 {
        return Err(SHAPE);
    }
    Ok(address.to_ascii_lowercase())
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
    fn label_keys_and_values() {
        for ok in [
            "a",
            "status",
            "task.state",
            "a_b-c.9",
            &"k".repeat(64),
            "boards",
            "board",
        ] {
            assert!(validate_label_key(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "A",
            "a b",
            "a=b",
            "a|b",
            "é",
            "board.x",
            "board.",
            &"k".repeat(65),
        ] {
            assert!(validate_label_key(bad).is_err(), "{bad}");
        }
        for ok in ["x", "In Progress", "a=b|c", "é", &"v".repeat(256)] {
            assert!(validate_label_value(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "a\nb",
            "a\tb",
            "a\u{0}b",
            "a\u{7f}",
            "a\u{85}",
            &"v".repeat(257),
            &"é".repeat(129),
        ] {
            assert!(validate_label_value(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn reaction_names() {
        for ok in ["thumbsup", "+1", "-1", "a_b", "100", &"a".repeat(32)] {
            assert!(validate_reaction(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "Thumbsup",
            "a b",
            "👍",
            "a.b",
            "a/b",
            ":a:",
            &"a".repeat(33),
        ] {
            assert!(validate_reaction(bad).is_err(), "{bad}");
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

    #[test]
    fn email_addresses() {
        let longest = format!("{}@example.com", "a".repeat(254 - "@example.com".len()));
        for ok in [
            "ada@example.com",
            "Ada.Lovelace+board@Mail.Example.COM",
            "a@b",
            "first_last-2@sub-domain.example.org",
            "7@9",
            longest.as_str(),
        ] {
            let handle = validate_email(ok).unwrap_or_else(|reason| panic!("{ok}: {reason}"));
            assert_eq!(handle, ok.to_ascii_lowercase());
            // No agent can hold it, and it is its own comparison key.
            assert!(validate_handle(&handle).is_err());
            assert_eq!(handle_key(ok), handle);
            // `@` and the address is one mention of exactly the address,
            // also at the end of a sentence.
            for text in [format!("@{ok}"), format!("ask @{ok}, please. @{ok}.")] {
                let found = crate::markdown::mention_candidates(&text);
                assert_eq!(found.len(), 1, "{text}");
                assert_eq!(found[0].handle, ok, "{text}");
                assert_eq!(found[0].profile, None, "{text}");
            }
        }
        let too_long = format!("{}@example.com", "a".repeat(255 - "@example.com".len()));
        for bad in [
            "",
            "ada",
            "@example.com",
            "ada@",
            "@",
            "ada@@example.com",
            "ada@example@com",
            "ada @example.com",
            "ada@exam ple.com",
            " ada@example.com",
            "ada@example.com\n",
            "ada\t@example.com",
            "ada\u{0}@example.com",
            "ada@example.com.",
            ".ada@example.com",
            "ada.@example.com",
            "ada@.example.com",
            "a..b@example.com",
            "_ada_@example.com",
            "a__b@example.com",
            "ada@example.com/reviewer",
            "ada*@example.com",
            "<ada@example.com>",
            "ada@[127.0.0.1]",
            "\"ada\"@example.com",
            "ada@exämple.com",
            "ädä@example.com",
            too_long.as_str(),
        ] {
            assert!(validate_email(bad).is_err(), "{bad:?}");
        }
    }
}
