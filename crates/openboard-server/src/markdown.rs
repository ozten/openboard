//! Markdown: rendering message text to sanitised HTML, and finding the
//! mentions in it.
//!
//! This is a security boundary (design P13). Message text is written by
//! agents and people the reader need not trust, and the HTML made here is
//! put into a page. So the HTML is never taken from a general-purpose
//! renderer and cleaned afterwards. [`render_html`] walks the events of the
//! `pulldown-cmark` parser and writes every tag itself, from a fixed list:
//!
//! - Elements: `p`, `br`, `em`, `strong`, `del`, `code`, `pre`,
//!   `blockquote`, `ul`, `ol`, `li`, `h1` to `h6`, `hr`, `a`, `span`,
//!   `table`, `thead`, `tr`, `th` and `td`.
//! - Attributes: `href` and a fixed `rel` on `a`, `class="mention"` on
//!   `span`, `start` on `ol` (digits), and `class="language-…"` on `code`
//!   (letters, digits, `+` and `-`).
//!
//! Every piece of text and every attribute value from the message is
//! escaped. Raw HTML in the message arrives as its own kind of event and is
//! written as escaped text. A link keeps its `href` only when the address
//! starts with `http://`, `https://` or `mailto:` and holds no whitespace
//! or control character; any other link is written as its text alone. An
//! image is never loaded: it becomes a link to its address, with the
//! alternative text as the link text.
//!
//! The emitter is a loop over a flat event stream with its own stack, so
//! deeply nested input cannot overflow the call stack, and elements nested
//! deeper than [`MAX_NESTING`] are written without their tags, so the output
//! stays proportional to the input.
//!
//! # Mentions
//!
//! A mention is `@handle` or `@handle/profile` in text outside code spans
//! and code blocks. The same walk finds them for both uses: when a message
//! is written, [`mention_candidates`] lists what the text addresses and the
//! store keeps those that name a principal; when it is rendered, a
//! candidate that the store kept is wrapped in `<span class="mention">`.

use openboard_api::Mention;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag};

/// Elements nested deeper than this are written without their tags.
const MAX_NESTING: usize = 32;
/// Longest language name kept from a fenced code block.
const MAX_LANGUAGE_LEN: usize = 32;
/// Most distinct mention candidates taken from one message.
pub(crate) const MAX_MENTION_CANDIDATES: usize = 500;
/// Longest profile a mention can name; the rule of `names::validate_profile`.
const MAX_PROFILE_LEN: usize = 32;

/// Something in a message that is shaped like a mention. Whether it names a
/// principal is for the store to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// The handle as written, without the `@`.
    pub handle: String,
    /// The profile as written, when one follows the handle.
    pub profile: Option<String>,
}

/// The distinct mention candidates in `text`, in order of first appearance,
/// at most [`MAX_MENTION_CANDIDATES`]. Handles and profiles are distinct
/// without regard to ASCII case.
pub(crate) fn mention_candidates(text: &str) -> Vec<Candidate> {
    let mut renderer = Renderer::new(None);
    renderer.run(text);
    renderer.candidates
}

/// Renders Markdown `text` to HTML through the allowlist described in the
/// module documentation. `mentions` are the message's resolved mentions.
pub(crate) fn render_html(text: &str, mentions: &[Mention]) -> String {
    let mut renderer = Renderer::new(Some(mentions));
    renderer.run(text);
    renderer.out
}

/// What closing an element has to undo.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plain,
    /// A code block: the text inside is code, so it holds no mentions.
    CodeBlock,
    /// A link or image whose `<a>` was written.
    Anchor,
    TableHead,
}

struct Open {
    /// Written when the element ends. Empty when its tags are left out.
    close: &'static str,
    kind: Kind,
}

struct Renderer<'m> {
    out: String,
    /// `None` when only the candidates are wanted.
    mentions: Option<&'m [Mention]>,
    candidates: Vec<Candidate>,
    stack: Vec<Open>,
    /// Adjacent text events, joined, so a mention split by the parser is
    /// still seen whole.
    pending: String,
    code_blocks: usize,
    anchors: usize,
    in_table_head: bool,
}

impl<'m> Renderer<'m> {
    fn new(mentions: Option<&'m [Mention]>) -> Self {
        Self {
            out: String::new(),
            mentions,
            candidates: Vec::new(),
            stack: Vec::new(),
            pending: String::new(),
            code_blocks: 0,
            anchors: 0,
            in_table_head: false,
        }
    }

    fn run(&mut self, text: &str) {
        let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
        for event in Parser::new_ext(text, options) {
            match event {
                Event::Text(text) => self.pending.push_str(&text),
                other => {
                    self.flush_text();
                    self.event(other);
                }
            }
        }
        self.flush_text();
        // The parser balances its events. Should it ever not, the output
        // still closes what it opened.
        while let Some(open) = self.stack.pop() {
            self.out.push_str(open.close);
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(_) => self.end(),
            // Joined and written by `flush_text`.
            Event::Text(text) => escape_into(&mut self.out, &text),
            Event::Code(code) => {
                self.out.push_str("<code>");
                escape_into(&mut self.out, &code);
                self.out.push_str("</code>");
            }
            // Raw HTML is shown, never passed through.
            Event::Html(raw) | Event::InlineHtml(raw) => escape_into(&mut self.out, &raw),
            // Not enabled. Written as the text they came from, in case a
            // later parser version yields them anyway.
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                escape_into(&mut self.out, &math);
            }
            Event::FootnoteReference(label) => {
                self.out.push_str("[^");
                escape_into(&mut self.out, &label);
                self.out.push(']');
            }
            Event::TaskListMarker(done) => {
                self.out.push_str(if done { "[x] " } else { "[ ] " });
            }
            Event::SoftBreak => self.out.push('\n'),
            Event::HardBreak => self.out.push_str("<br>"),
            Event::Rule => self.out.push_str("<hr>"),
        }
    }

    /// Opens an element, or only records it when the nesting is too deep.
    fn open(&mut self, start: &str, close: &'static str, kind: Kind) {
        let written = self.stack.len() < MAX_NESTING;
        if written {
            self.out.push_str(start);
        }
        self.stack.push(Open {
            close: if written { close } else { "" },
            kind,
        });
    }

    /// Records an element that has no tags of its own.
    fn open_silent(&mut self, kind: Kind) {
        self.stack.push(Open { close: "", kind });
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.open("<p>", "</p>", Kind::Plain),
            Tag::Heading { level, .. } => {
                let (start, close) = match level {
                    HeadingLevel::H1 => ("<h1>", "</h1>"),
                    HeadingLevel::H2 => ("<h2>", "</h2>"),
                    HeadingLevel::H3 => ("<h3>", "</h3>"),
                    HeadingLevel::H4 => ("<h4>", "</h4>"),
                    HeadingLevel::H5 => ("<h5>", "</h5>"),
                    HeadingLevel::H6 => ("<h6>", "</h6>"),
                };
                self.open(start, close, Kind::Plain);
            }
            Tag::BlockQuote(_) => self.open("<blockquote>", "</blockquote>", Kind::Plain),
            Tag::CodeBlock(kind) => {
                self.code_blocks += 1;
                let language = match &kind {
                    CodeBlockKind::Fenced(info) => language_of(info),
                    CodeBlockKind::Indented => None,
                };
                let start = match language {
                    Some(language) => format!("<pre><code class=\"language-{language}\">"),
                    None => "<pre><code>".to_owned(),
                };
                self.open(&start, "</code></pre>", Kind::CodeBlock);
            }
            // The lines inside arrive as raw HTML events and are escaped.
            Tag::HtmlBlock => self.open("<pre>", "</pre>", Kind::Plain),
            Tag::List(Some(first)) if first != 1 => {
                self.open(&format!("<ol start=\"{first}\">"), "</ol>", Kind::Plain);
            }
            Tag::List(Some(_)) => self.open("<ol>", "</ol>", Kind::Plain),
            Tag::List(None) => self.open("<ul>", "</ul>", Kind::Plain),
            Tag::Item => self.open("<li>", "</li>", Kind::Plain),
            Tag::Table(_) => self.open("<table>", "</table>", Kind::Plain),
            Tag::TableHead => {
                self.in_table_head = true;
                self.open("<thead><tr>", "</tr></thead>", Kind::TableHead);
            }
            Tag::TableRow => self.open("<tr>", "</tr>", Kind::Plain),
            Tag::TableCell => {
                if self.in_table_head {
                    self.open("<th>", "</th>", Kind::Plain);
                } else {
                    self.open("<td>", "</td>", Kind::Plain);
                }
            }
            Tag::Emphasis => self.open("<em>", "</em>", Kind::Plain),
            Tag::Strong => self.open("<strong>", "</strong>", Kind::Plain),
            Tag::Strikethrough => self.open("<del>", "</del>", Kind::Plain),
            // An image is not loaded. It is a link to its address, and the
            // events inside it, its alternative text, become the link text.
            Tag::Link {
                link_type,
                dest_url,
                ..
            }
            | Tag::Image {
                link_type,
                dest_url,
                ..
            } => self.anchor(link_type, &dest_url),
            // Not enabled, and they have no element on the allowlist.
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => self.open_silent(Kind::Plain),
        }
    }

    fn anchor(&mut self, link_type: LinkType, address: &str) {
        // An `<a>` inside an `<a>` is not valid HTML, so a link or image
        // inside a link is written as its text.
        let href = if self.anchors == 0 && self.stack.len() < MAX_NESTING {
            safe_href(link_type, address)
        } else {
            None
        };
        match href {
            Some(href) => {
                self.anchors += 1;
                self.out.push_str("<a href=\"");
                escape_into(&mut self.out, &href);
                self.out
                    .push_str("\" rel=\"noopener noreferrer nofollow\">");
                self.stack.push(Open {
                    close: "</a>",
                    kind: Kind::Anchor,
                });
            }
            None => self.open_silent(Kind::Plain),
        }
    }

    fn end(&mut self) {
        let Some(open) = self.stack.pop() else {
            return;
        };
        self.out.push_str(open.close);
        match open.kind {
            Kind::Plain => {}
            Kind::CodeBlock => self.code_blocks = self.code_blocks.saturating_sub(1),
            Kind::Anchor => self.anchors = self.anchors.saturating_sub(1),
            Kind::TableHead => self.in_table_head = false,
        }
    }

    /// Writes the text gathered so far, escaped, with resolved mentions
    /// wrapped, and notes the mention candidates in it.
    fn flush_text(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending);
        if self.code_blocks > 0 {
            escape_into(&mut self.out, &text);
            return;
        }
        let mut written = 0;
        for found in scan_mentions(&text) {
            let handle = &text[found.handle.clone()];
            let profile = found.profile.clone().map(|range| &text[range]);
            self.note(handle, profile);
            let resolved = self.mentions.is_some_and(|mentions| {
                mentions.iter().any(|mention| {
                    mention.handle.eq_ignore_ascii_case(handle)
                        && match (&mention.profile, profile) {
                            (None, None) => true,
                            (Some(stored), Some(written)) => stored.eq_ignore_ascii_case(written),
                            _ => false,
                        }
                })
            });
            if resolved {
                escape_into(&mut self.out, &text[written..found.start]);
                self.out.push_str("<span class=\"mention\">");
                escape_into(&mut self.out, &text[found.start..found.end]);
                self.out.push_str("</span>");
                written = found.end;
            }
        }
        escape_into(&mut self.out, &text[written..]);
    }

    fn note(&mut self, handle: &str, profile: Option<&str>) {
        if self.candidates.len() >= MAX_MENTION_CANDIDATES {
            return;
        }
        let known = self.candidates.iter().any(|candidate| {
            candidate.handle.eq_ignore_ascii_case(handle)
                && match (&candidate.profile, profile) {
                    (None, None) => true,
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    _ => false,
                }
        });
        if !known {
            self.candidates.push(Candidate {
                handle: handle.to_owned(),
                profile: profile.map(str::to_owned),
            });
        }
    }
}

/// The language of a fenced code block, when its name is one the allowlist
/// can carry: letters, digits, `+` and `-`.
fn language_of(info: &str) -> Option<&str> {
    let name = info.split_whitespace().next()?;
    let allowed = name.len() <= MAX_LANGUAGE_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-');
    allowed.then_some(name)
}

/// The address to put in an `href`, or `None` when the link must not be
/// one. Only an address that starts, at its very first byte and whatever
/// the letter case, with `http://`, `https://` or `mailto:` passes, and
/// only when it holds no whitespace or control character anywhere:
/// browsers strip tabs and line breaks from an address before reading its
/// scheme.
fn safe_href(link_type: LinkType, address: &str) -> Option<String> {
    let starts_with = |prefix: &str| {
        address
            .get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    };
    let address = if link_type == LinkType::Email && !starts_with("mailto:") {
        format!("mailto:{address}")
    } else {
        address.to_owned()
    };
    if address.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let allowed = ["http://", "https://", "mailto:"].iter().any(|prefix| {
        address
            .get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    });
    allowed.then_some(address)
}

/// Appends `text` with the five characters that matter in HTML text and in
/// a quoted attribute value replaced by entities.
fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
}

/// One mention-shaped piece of a text run, as byte ranges into it.
struct Found {
    /// From the `@` to the end of the handle or profile.
    start: usize,
    end: usize,
    handle: std::ops::Range<usize>,
    profile: Option<std::ops::Range<usize>>,
}

/// A character a handle can hold: those of an agent's name and of an email
/// address.
fn is_handle_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-' | b'@')
}

/// Finds what is shaped like a mention in one run of text.
///
/// The `@` must not follow a letter, a digit or one of `._+-@/`, so the
/// middle of an email address or a path is not a mention. The handle is
/// the longest run of handle characters after it, less any trailing
/// punctuation. When the run is directly followed by `/` and a profile
/// name, that is the profile.
fn scan_mentions(text: &str) -> Vec<Found> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(offset) = bytes[at..].iter().position(|b| *b == b'@') {
        let start = at + offset;
        let run_start = start + 1;
        let mut run_end = run_start;
        while run_end < bytes.len() && is_handle_byte(bytes[run_end]) {
            run_end += 1;
        }
        // Whatever happens, the scan continues after this run.
        at = run_end.max(run_start);
        let attached = text[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '+' | '-' | '@' | '/'));
        if attached || run_end == run_start {
            continue;
        }

        // `@handle/profile`: the handle is the whole run.
        if bytes[run_end - 1].is_ascii_alphanumeric()
            && bytes.get(run_end) == Some(&b'/')
            && bytes
                .get(run_end + 1)
                .is_some_and(|b| b.is_ascii_alphabetic())
        {
            let profile_start = run_end + 1;
            let mut profile_end = profile_start;
            while profile_end < bytes.len()
                && (bytes[profile_end].is_ascii_alphanumeric() || bytes[profile_end] == b'-')
            {
                profile_end += 1;
            }
            while bytes[profile_end - 1] == b'-' {
                profile_end -= 1;
            }
            if profile_end - profile_start <= MAX_PROFILE_LEN {
                found.push(Found {
                    start,
                    end: profile_end,
                    handle: run_start..run_end,
                    profile: Some(profile_start..profile_end),
                });
                at = profile_end;
                continue;
            }
        }

        // `@handle`: trailing sentence punctuation is not part of it.
        let mut handle_end = run_end;
        while handle_end > run_start && !bytes[handle_end - 1].is_ascii_alphanumeric() {
            handle_end -= 1;
        }
        if handle_end > run_start {
            found.push(Found {
                start,
                end: handle_end,
                handle: run_start..handle_end,
                profile: None,
            });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use openboard_api::ActorKind;

    use super::*;

    fn html(text: &str) -> String {
        render_html(text, &[])
    }

    fn mention(handle: &str, profile: Option<&str>) -> Mention {
        Mention {
            handle: handle.to_owned(),
            kind: ActorKind::Agent,
            profile: profile.map(str::to_owned),
        }
    }

    fn candidates(text: &str) -> Vec<(String, Option<String>)> {
        mention_candidates(text)
            .into_iter()
            .map(|candidate| (candidate.handle, candidate.profile))
            .collect()
    }

    /// Every tag in `html`, as its name with a leading `/` for a closing
    /// tag, together with its raw attribute text.
    fn tags(html: &str) -> Vec<(String, String)> {
        let mut tags = Vec::new();
        let mut rest = html;
        while let Some(open) = rest.find('<') {
            let after = &rest[open + 1..];
            let close = after
                .find('>')
                .expect("every `<` in the output opens a tag");
            let inner = &after[..close];
            let (name, attributes) = match inner.split_once(' ') {
                Some((name, attributes)) => (name, attributes),
                None => (inner, ""),
            };
            tags.push((name.to_owned(), attributes.to_owned()));
            rest = &after[close + 1..];
        }
        tags
    }

    const ELEMENTS: [&str; 26] = [
        "p",
        "br",
        "em",
        "strong",
        "del",
        "code",
        "pre",
        "blockquote",
        "ul",
        "ol",
        "li",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "a",
        "span",
        "table",
        "thead",
        "tbody",
        "tr",
        "th",
        "td",
    ];

    /// Checks the output against the allowlist, structurally: every `<`
    /// starts an allowed tag with allowed attributes, tags are balanced,
    /// and every `href` has an allowed scheme. Because all text is escaped,
    /// a `<` can only come from the emitter.
    fn assert_allowlisted(input: &str, html: &str) {
        let mut open: Vec<String> = Vec::new();
        for (name, attributes) in tags(html) {
            if let Some(closing) = name.strip_prefix('/') {
                assert_eq!(
                    open.pop().as_deref(),
                    Some(closing),
                    "unbalanced {closing} for input {input:?}: {html}"
                );
                assert!(attributes.is_empty());
                continue;
            }
            assert!(
                ELEMENTS.contains(&name.as_str()),
                "element {name:?} is not allowed; input {input:?}: {html}"
            );
            match name.as_str() {
                "a" => {
                    let rest = attributes
                        .strip_prefix("href=\"")
                        .unwrap_or_else(|| panic!("an <a> without href: {html}"));
                    let (href, rest) = rest.split_once('"').unwrap();
                    assert_eq!(rest, " rel=\"noopener noreferrer nofollow\"", "{html}");
                    let lower = href.to_ascii_lowercase();
                    assert!(
                        ["http://", "https://", "mailto:"]
                            .iter()
                            .any(|scheme| lower.starts_with(scheme)),
                        "href {href:?} for input {input:?}"
                    );
                    assert!(!href.chars().any(|c| c.is_control() || c.is_whitespace()));
                }
                "span" => assert_eq!(attributes, "class=\"mention\""),
                "ol" if !attributes.is_empty() => {
                    let number = attributes
                        .strip_prefix("start=\"")
                        .and_then(|rest| rest.strip_suffix('"'))
                        .unwrap_or_else(|| panic!("ol attributes {attributes:?}"));
                    assert!(number.bytes().all(|b| b.is_ascii_digit()), "{number}");
                }
                "code" if !attributes.is_empty() => {
                    let language = attributes
                        .strip_prefix("class=\"language-")
                        .and_then(|rest| rest.strip_suffix('"'))
                        .unwrap_or_else(|| panic!("code attributes {attributes:?}"));
                    assert!(!language.is_empty());
                    assert!(language
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-'));
                }
                _ => assert!(
                    attributes.is_empty(),
                    "attributes {attributes:?} on {name}; input {input:?}"
                ),
            }
            if !matches!(name.as_str(), "br" | "hr") {
                open.push(name);
            }
        }
        assert!(open.is_empty(), "unclosed {open:?} for input {input:?}");
        // Outside tags, quotes are entities, so no attribute can be forged.
        let mut outside = String::new();
        let mut rest = html;
        while let Some(start) = rest.find('<') {
            outside.push_str(&rest[..start]);
            rest = &rest[start + rest[start..].find('>').unwrap() + 1..];
        }
        outside.push_str(rest);
        assert!(
            !outside.contains(['"', '\'', '>']),
            "unescaped text for input {input:?}: {html}"
        );
    }

    #[test]
    fn common_markdown_renders() {
        for (input, expected) in [
            ("hello", "<p>hello</p>"),
            ("a *b* **c** ~~d~~ `e`", "<p>a <em>b</em> <strong>c</strong> <del>d</del> <code>e</code></p>"),
            ("# One\n\n###### Six", "<h1>One</h1><h6>Six</h6>"),
            ("> quoted", "<blockquote><p>quoted</p></blockquote>"),
            ("- a\n- b", "<ul><li>a</li><li>b</li></ul>"),
            ("1. a\n2. b", "<ol><li>a</li><li>b</li></ol>"),
            ("7. a\n8. b", "<ol start=\"7\"><li>a</li><li>b</li></ol>"),
            ("a\nb", "<p>a\nb</p>"),
            ("a  \nb", "<p>a<br>b</p>"),
            ("a\n\n---\n\nb", "<p>a</p><hr><p>b</p>"),
            ("```rust\nlet a = 1 < 2;\n```", "<pre><code class=\"language-rust\">let a = 1 &lt; 2;\n</code></pre>"),
            ("```c++\nx\n```", "<pre><code class=\"language-c++\">x\n</code></pre>"),
            ("```\nx\n```", "<pre><code>x\n</code></pre>"),
            ("    indented", "<pre><code>indented</code></pre>"),
            (
                "| a | b |\n|---|:-:|\n| 1 | 2 |",
                "<table><thead><tr><th>a</th><th>b</th></tr></thead><tr><td>1</td><td>2</td></tr></table>",
            ),
            (
                "[text](https://example.com/a?b=1&c=2)",
                "<p><a href=\"https://example.com/a?b=1&amp;c=2\" rel=\"noopener noreferrer nofollow\">text</a></p>",
            ),
            (
                "<https://example.com>",
                "<p><a href=\"https://example.com\" rel=\"noopener noreferrer nofollow\">https://example.com</a></p>",
            ),
            (
                "<ada@example.com>",
                "<p><a href=\"mailto:ada@example.com\" rel=\"noopener noreferrer nofollow\">ada@example.com</a></p>",
            ),
            (
                "[mail](MAILTO:ada@example.com)",
                "<p><a href=\"MAILTO:ada@example.com\" rel=\"noopener noreferrer nofollow\">mail</a></p>",
            ),
            ("", ""),
        ] {
            assert_eq!(html(input), expected, "input {input:?}");
            assert_allowlisted(input, &html(input));
        }
    }

    #[test]
    fn a_code_language_outside_the_allowlist_is_dropped() {
        for info in [
            "c#",
            "\"onmouseover=\"alert(1)",
            "x\"y",
            "a.b",
            "rust,ignore",
            "<script>",
            &"a".repeat(33),
        ] {
            let input = format!("```{info}\ncode\n```");
            assert_eq!(
                html(&input),
                "<pre><code>code\n</code></pre>",
                "info {info:?}"
            );
        }
        assert_eq!(
            html("```rust ignore\"x\ncode\n```"),
            "<pre><code class=\"language-rust\">code\n</code></pre>"
        );
    }

    #[test]
    fn raw_html_is_shown_as_text() {
        for (input, expected) in [
            (
                "<script>alert(1)</script>",
                "<pre>&lt;script&gt;alert(1)&lt;/script&gt;</pre>",
            ),
            (
                "hi <b onclick=\"x()\">bold</b>",
                "<p>hi &lt;b onclick=&quot;x()&quot;&gt;bold&lt;/b&gt;</p>",
            ),
            (
                "a <img src=x onerror=alert(1)> b",
                "<p>a &lt;img src=x onerror=alert(1)&gt; b</p>",
            ),
            ("<!-- c -->", "<pre>&lt;!-- c --&gt;</pre>"),
            ("a < b && c > d", "<p>a &lt; b &amp;&amp; c &gt; d</p>"),
            ("it's \"quoted\"", "<p>it&#39;s &quot;quoted&quot;</p>"),
            // Entities are decoded by the parser and escaped again.
            ("&lt;script&gt;", "<p>&lt;script&gt;</p>"),
            (
                "&#60;script&#62; &#x3C;b&#x3E;",
                "<p>&lt;script&gt; &lt;b&gt;</p>",
            ),
            ("&amp;lt;", "<p>&amp;lt;</p>"),
            ("`<script>`", "<p><code>&lt;script&gt;</code></p>"),
        ] {
            assert_eq!(html(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn nothing_executable_survives() {
        let inputs = [
            "<script>alert(1)</script>",
            "<SCRIPT SRC=//evil.test/x.js></SCRIPT>",
            "<scr<script>ipt>alert(1)</scr</script>ipt>",
            "<img src=x onerror=alert(1)>",
            "<svg/onload=alert(1)>",
            "<iframe src=\"javascript:alert(1)\"></iframe>",
            "<a href=\"javascript:alert(1)\">x</a>",
            "<div style=\"background:url(javascript:alert(1))\">",
            "<body onload=alert(1)>",
            "<details open ontoggle=alert(1)>",
            "<math><mi xlink:href=\"data:x,<script>alert(1)</script>\">",
            "<style>*{background:url(//evil.test)}</style>",
            "<link rel=stylesheet href=//evil.test/x.css>",
            "<meta http-equiv=refresh content=\"0;url=//evil.test\">",
            "<base href=//evil.test/>",
            "<form action=//evil.test><input name=q></form>",
            "<object data=//evil.test></object><embed src=//evil.test>",
            "text <script>alert(1)</script> text",
            "> <script>alert(1)</script>",
            "- <img src=x onerror=alert(1)>",
            "| a |\n|---|\n| <script>x</script> |",
            "# <script>alert(1)</script>",
            "*<script>alert(1)</script>*",
            "[<script>alert(1)</script>](https://example.com)",
            "[x](https://example.com \"<script>alert(1)</script>\")",
            "[x](https://example.com \"t\\\" onmouseover=\\\"alert(1)\")",
            "[x](https://example.com\"onmouseover=\"alert(1))",
            "[x](<https://example.com\" onmouseover=\"alert(1)>)",
            "![x\" onerror=\"alert(1)](https://example.com/a.png)",
            "![<script>alert(1)</script>](https://example.com/a.png)",
            "<https://example.com/\"onmouseover=\"alert(1)>",
            "```\"><script>alert(1)</script>\ncode\n```",
            "``` \"onmouseover=\"alert(1)\ncode\n```",
            "<![CDATA[<script>alert(1)</script>]]>",
            "<?php echo 1 ?>",
            "<!DOCTYPE html><html>",
            "&lt;script&gt;alert(1)&lt;/script&gt;",
            "&#x3C;script&#x3E;alert(1)&#x3C;/script&#x3E;",
            "\\<script>alert(1)\\</script>",
            "<script\n>alert(1)</script\n>",
            "<<script>script>alert(1)",
            "</p><script>alert(1)</script>",
            "1. a\n\n   <script>alert(1)</script>",
            "@agent <script>alert(1)</script>",
            "[ref]\n\n[ref]: javascript:alert(1)",
            "[ref]: https://example.com \"<script>\"\n\n[ref]",
            "\u{0}<script>alert(1)</script>",
        ];
        let mentions = [mention("agent", None)];
        for input in inputs {
            let html = render_html(input, &mentions);
            assert_allowlisted(input, &html);
            let lower = html.to_ascii_lowercase();
            for element in [
                "<script", "<img", "<svg", "<iframe", "<style", "<link", "<meta", "<base", "<form",
                "<object", "<embed", "<input", "<body", "<div", "<math", "<details", "<b ",
                "<html",
            ] {
                assert!(
                    !lower.contains(element),
                    "{element} in {html} for {input:?}"
                );
            }
        }
    }

    #[test]
    fn only_http_https_and_mailto_links_keep_their_address() {
        let refused = [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "JAVASCRIPT:alert(1)",
            "jAvAsCrIpT:alert(1)",
            " javascript:alert(1)",
            "javascript&#58;alert(1)",
            "javascript&colon;alert(1)",
            "&#106;avascript:alert(1)",
            "&#x6A;avascript:alert(1)",
            "java&#9;script:alert(1)",
            "java&#10;script:alert(1)",
            "java&#13;script:alert(1)",
            "&#1;javascript:alert(1)",
            "&#x1f;javascript:alert(1)",
            "javascript://example.com/%0aalert(1)",
            "javascript:alert(1)//http://example.com",
            "data:text/html,<script>alert(1)</script>",
            "data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==",
            "DATA:text/html,x",
            "vbscript:msgbox(1)",
            "VbScript:msgbox(1)",
            "file:///etc/passwd",
            "ftp://example.com/x",
            "blob:https://example.com/x",
            "about:blank",
            "ws://example.com",
            "tel:+15550100",
            "//evil.test/x",
            "\\\\evil.test\\x",
            "/\\evil.test",
            "/relative",
            "relative/path",
            "#fragment",
            "?query",
            "http:/example.com",
            "http:example.com",
            "https:example.com",
            "http//example.com",
            "x-http://example.com",
            "httpx://example.com",
            "mailto",
            "feed:javascript:alert(1)",
            "view-source:javascript:alert(1)",
            "&#x09;https://example.com",
            "https://exa&#10;mple.com",
            "https://example.com/&#9;x",
            "https://example.com/&#x00A0;x",
            "\u{feff}javascript:alert(1)",
            "\u{200b}javascript:alert(1)",
        ];
        for address in refused {
            for input in [
                format!("[link text]({address})"),
                format!("[link text](<{address}>)"),
                format!("![link text]({address})"),
                format!("[link text][r]\n\n[r]: {address}"),
                format!("[link text][r]\n\n[r]: <{address}>"),
            ] {
                let html = html(&input);
                assert_allowlisted(&input, &html);
                assert!(
                    !html.contains("<a"),
                    "address {address:?} kept in {html} for {input:?}"
                );
            }
        }
        // A refused link is still shown, as its text.
        assert_eq!(
            html("[click *me*](javascript:alert(1))"),
            "<p>click <em>me</em></p>"
        );
        for address in [
            "http://example.com",
            "https://example.com/a/b?c=d#e",
            "HTTPS://EXAMPLE.COM",
            "hTtP://example.com",
            "mailto:ada@example.com",
            "https://example.com/javascript:alert(1)",
        ] {
            let input = format!("[link text]({address})");
            let html = html(&input);
            assert_allowlisted(&input, &html);
            assert!(html.contains(&format!("<a href=\"{address}\"")), "{html}");
        }
    }

    #[test]
    fn autolinks_follow_the_same_rule() {
        for input in [
            "<javascript:alert(1)>",
            "<JaVaScRiPt:alert(1)>",
            "<data:text/html,x>",
            "<vbscript:x>",
            "<ftp://example.com>",
            "<file:///etc/passwd>",
        ] {
            let html = html(input);
            assert_allowlisted(input, &html);
            assert!(!html.contains("<a"), "{html} for {input:?}");
        }
    }

    #[test]
    fn an_image_becomes_a_link_and_is_never_loaded() {
        assert_eq!(
            html("![a *cat*](https://example.com/cat.png \"title\")"),
            "<p><a href=\"https://example.com/cat.png\" rel=\"noopener noreferrer nofollow\">a <em>cat</em></a></p>"
        );
        assert_eq!(html("![tracker](//evil.test/pixel.gif)"), "<p>tracker</p>");
        assert_eq!(
            html("![x](data:image/png;base64,AAAA)"),
            "<p>x</p>",
            "a data: image is not linked either"
        );
        // An image inside a link does not nest anchors.
        assert_eq!(
            html("[![alt](https://example.com/i.png)](https://example.com/page)"),
            "<p><a href=\"https://example.com/page\" rel=\"noopener noreferrer nofollow\">alt</a></p>"
        );
        for input in [
            "![a](https://example.com/a.png)",
            "![](https://example.com/a.png)",
            "![a][r]\n\n[r]: https://example.com/a.png",
        ] {
            assert!(!html(input).contains("<img"));
            assert_allowlisted(input, &html(input));
        }
    }

    #[test]
    fn attribute_values_are_escaped() {
        let page = html("[x](https://example.com/?a=\"b\"&c='d'<e>)");
        assert_allowlisted("quotes in an address", &page);
        assert!(!page.contains("\"b\""), "{page}");
        let page = html("[x](<https://example.com/a\"b>)");
        assert_eq!(
            page,
            "<p><a href=\"https://example.com/a&quot;b\" rel=\"noopener noreferrer nofollow\">x</a></p>"
        );
        // A title is not on the allowlist, so it is not written at all.
        let page = html("[x](https://example.com \"a title\")");
        assert!(!page.contains("title"), "{page}");
    }

    #[test]
    fn mentions_are_found_outside_code_only() {
        assert_eq!(
            candidates("hi @agent-c, meet @Ada@Example.com. And @agent-c/reviewer!"),
            [
                ("agent-c".to_owned(), None),
                ("Ada@Example.com".to_owned(), None),
                ("agent-c".to_owned(), Some("reviewer".to_owned())),
            ]
        );
        // Code spans, code blocks and raw HTML hold no mentions.
        assert!(candidates("`@agent-c`").is_empty());
        assert!(candidates("``` \n@agent-c\n```").is_empty());
        assert!(candidates("    @agent-c in an indented block").is_empty());
        assert!(candidates("~~~\n@agent-c\n~~~\n").is_empty());
        assert!(candidates("<b title=\"@agent-c\">").is_empty());
        assert_eq!(
            candidates("`@a` then @b\n\n```\n@c\n```\n\n> @d"),
            [("b".to_owned(), None), ("d".to_owned(), None)]
        );
        // Emphasis, links, headings, lists and tables do.
        for input in [
            "*@agent-c*",
            "**@agent-c**",
            "[@agent-c](https://example.com)",
            "# @agent-c",
            "- @agent-c",
            "| @agent-c |\n|---|",
            "(@agent-c)",
            "\"@agent-c\"",
            "@agent-c's",
            "@agent-c...",
            "@agent-c?!",
            "line\n@agent-c",
        ] {
            assert_eq!(
                candidates(input),
                [("agent-c".to_owned(), None)],
                "input {input:?}"
            );
        }
        // An address or a path is not a mention.
        for input in [
            "ada@example.com",
            "see a/@b",
            "x@agent",
            "a.@b",
            "@",
            "@ agent",
            "@-",
            "@@",
            "é@agent",
        ] {
            assert!(candidates(input).is_empty(), "input {input:?}");
        }
        // The same candidate is listed once, whatever its case.
        assert_eq!(candidates("@a @A @a/p @A/P @a").len(), 2);
        // A profile that is too long is not one.
        assert_eq!(
            candidates(&format!("@a/{}", "p".repeat(33))),
            [("a".to_owned(), None)]
        );
        assert_eq!(
            candidates("@a/b-c-, @a/1x @a/"),
            [
                ("a".to_owned(), Some("b-c".to_owned())),
                ("a".to_owned(), None)
            ]
        );
    }

    #[test]
    fn a_mention_split_by_the_parser_is_still_one() {
        // The parser yields `_` and `&amp;` as separate text events.
        assert_eq!(
            candidates("@first_last@example.com and x &amp; @b"),
            [
                ("first_last@example.com".to_owned(), None),
                ("b".to_owned(), None)
            ]
        );
        assert!(candidates("snake_case@agent").is_empty());
    }

    #[test]
    fn candidates_are_capped() {
        let text: String = (0..MAX_MENTION_CANDIDATES + 50)
            .map(|n| format!("@a{n} "))
            .collect();
        assert_eq!(mention_candidates(&text).len(), MAX_MENTION_CANDIDATES);
    }

    #[test]
    fn resolved_mentions_are_wrapped() {
        let mentions = [
            mention("agent-c", None),
            mention("agent-c", Some("reviewer")),
            mention("ada@example.com", None),
        ];
        assert_eq!(
            render_html(
                "hi @Agent-C, @agent-c/reviewer and @agent-c/other. @ada@example.com? @nobody `@agent-c`",
                &mentions
            ),
            "<p>hi <span class=\"mention\">@Agent-C</span>, \
             <span class=\"mention\">@agent-c/reviewer</span> and @agent-c/other. \
             <span class=\"mention\">@ada@example.com</span>? @nobody <code>@agent-c</code></p>"
        );
        assert_eq!(
            render_html("```\n@agent-c\n```", &mentions),
            "<pre><code>@agent-c\n</code></pre>"
        );
        // Without resolved mentions nothing is wrapped.
        assert_eq!(html("@agent-c"), "<p>@agent-c</p>");
        // The wrapped text is escaped like any other.
        let odd = [mention("a", None)];
        assert_eq!(
            render_html("<@a> & @a", &odd),
            "<p>&lt;<span class=\"mention\">@a</span>&gt; &amp; <span class=\"mention\">@a</span></p>"
        );
    }

    #[test]
    fn malformed_markdown_stays_balanced() {
        for input in [
            "*a **b* c**",
            "**a *b** c*",
            "[a](",
            "[a](https://example.com",
            "[a][",
            "![",
            "](",
            "`a",
            "```",
            "```\nunclosed",
            "~~~~\n```\n~~~",
            "> - > - a\n\n- > b",
            "| a |\n|---",
            "| a | b |\n|---|\n| 1 | 2 | 3 |",
            "|\n|-|\n|",
            "1. a\n   - b\n     1. c\n\n       > d",
            "<",
            ">",
            "&",
            "&#",
            "&#x;",
            "&#99999999;",
            "\\",
            "***",
            "___\n***\n---",
            "#######",
            "# \n## \n",
            "\r\n\r\n",
            "\t\t-\t-\t-",
            "[a]: \n[a]",
            "[[[[[[a]]]]]]",
            "[a](https://example.com)[b](https://example.com)",
            "[a [b](https://example.com/b) c](https://example.com/a)",
            "*[a*](https://example.com)",
            "~~a ~~b~~ c~~",
            "a\u{0}b",
            "\u{feff}# a",
            "a\u{2028}b",
            "-\n-\n-",
            "- \n\n\n-",
            "1)\n2)",
            "999999999. a",
            "0. a",
        ] {
            assert_allowlisted(input, &html(input));
        }
    }

    #[test]
    fn deep_nesting_is_capped() {
        for unit in ["> ", "- ", "1. ", "* "] {
            let input = format!("{}deep", unit.repeat(3000));
            let started = Instant::now();
            let html = html(&input);
            assert!(started.elapsed() < Duration::from_secs(5), "{unit:?}");
            assert_allowlisted(unit, &html);
            let opened = tags(&html)
                .iter()
                .filter(|(name, _)| !name.starts_with('/'))
                .count();
            assert!(opened <= MAX_NESTING, "{opened} elements for {unit:?}");
            assert!(html.contains("deep"));
        }
        // Inline nesting too.
        let input = format!("{}x{}", "*_".repeat(2000), "_*".repeat(2000));
        assert_allowlisted("emphasis", &html(&input));
        let input = format!(
            "{}x{}",
            "[".repeat(5000),
            "](https://example.com)".repeat(5000)
        );
        assert_allowlisted("links", &html(&input));
    }

    #[test]
    fn hostile_input_renders_in_bounded_time_and_size() {
        let n = 20_000;
        let inputs = [
            "*".repeat(n),
            "_".repeat(n),
            "`".repeat(n),
            "[".repeat(n),
            "]".repeat(n),
            "(".repeat(n),
            "<".repeat(n),
            ">".repeat(n),
            "&".repeat(n),
            "@".repeat(n),
            "|".repeat(n),
            "~".repeat(n),
            "#".repeat(n),
            "\\".repeat(n),
            "\n".repeat(n),
            "*a ".repeat(n / 3),
            "*a_ ".repeat(n / 4),
            "[a](".repeat(n / 4),
            "![a](".repeat(n / 5),
            "[a][b]".repeat(n / 6),
            "<a ".repeat(n / 3),
            "<!--".repeat(n / 4),
            "<![CDATA[".repeat(n / 9),
            "<?".repeat(n / 2),
            "&#".repeat(n / 2),
            "`a``".repeat(n / 4),
            "``` ".repeat(n / 4),
            "|a".repeat(n / 2) + "\n" + &"|-".repeat(n / 2) + "\n" + &"|b".repeat(n / 2),
            "- a\n".repeat(n / 4),
            "> a\n".repeat(n / 4),
            "@a ".repeat(n / 3),
            "@a/b ".repeat(n / 5),
            (0..n / 8).map(|i| format!("@a{i} ")).collect(),
            "a@".repeat(n / 2),
            "[x]: https://example.com\n".repeat(n / 25) + &"[x]".repeat(n / 3),
            "* * * * ".repeat(n / 8),
            "1. ".repeat(n / 3),
            "x\n\n".repeat(n / 3),
            "<script>".repeat(n / 8),
            "[a](javascript:b)".repeat(n / 17),
        ];
        let mentions = [mention("a", None), mention("a", Some("b"))];
        for input in &inputs {
            let label: String = input.chars().take(12).collect();
            let started = Instant::now();
            let html = render_html(input, &mentions);
            let found = mention_candidates(input);
            let took = started.elapsed();
            assert!(took < Duration::from_secs(5), "{label:?} took {took:?}");
            assert!(
                html.len() <= input.len() * 40 + 64,
                "{label:?}: {} bytes of HTML from {} of input",
                html.len(),
                input.len()
            );
            assert!(found.len() <= MAX_MENTION_CANDIDATES);
            assert_allowlisted(&label, &html);
        }
    }

    #[test]
    fn every_short_string_over_a_hostile_alphabet_stays_inside_the_allowlist() {
        // An exhaustive walk over short strings built from the pieces that
        // matter to Markdown and HTML. It is a cheap stand-in for a fuzzer.
        let pieces = [
            "<",
            ">",
            "\"",
            "'",
            "&",
            "[",
            "]",
            "(",
            ")",
            "!",
            "*",
            "`",
            "\n",
            " ",
            "a",
            ":",
            "/",
            "@",
            "javascript:",
            "https://e.x",
            "<script>",
            "|",
            "-",
            "#",
            "\\",
        ];
        let mentions = [mention("a", None)];
        let mut checked = 0u32;
        for a in pieces {
            for b in pieces {
                for c in pieces {
                    for d in pieces {
                        let input = format!("{a}{b}{c}{d}");
                        assert_allowlisted(&input, &render_html(&input, &mentions));
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, 25 * 25 * 25 * 25);
        // Link-shaped strings with every piece as the address and the text.
        for address in pieces {
            for extra in pieces {
                for text in pieces {
                    for input in [
                        format!("[{text}]({address}{extra})"),
                        format!("![{text}]({address}{extra})"),
                        format!("[{text}](<{address}{extra}>)"),
                        format!("<{address}{extra}{text}>"),
                        format!("[{text}][r]\n\n[r]: {address}{extra}"),
                        format!("```{address}{extra}\n{text}\n```"),
                    ] {
                        assert_allowlisted(&input, &render_html(&input, &mentions));
                    }
                }
            }
        }
    }
}
