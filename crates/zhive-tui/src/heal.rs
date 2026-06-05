//! Streaming tail-healing for partial Markdown buffers.
//!
//! While a message streams token-by-token its tail often holds an unclosed
//! marker — `**bold` without its closing `**`, a lone `` ` ``, a half-typed
//! `[label](ur` — which the renderer would show as a literal symbol that then
//! "pops" into styled form one token later.  [`heal_tail`] rewrites just the
//! buffer tail so that mid-stream text renders in its eventual style without the
//! flicker, then hands the result to [`markdown::render`](crate::markdown).
//!
//! It runs only on the live streaming buffer (not finalized messages) and is
//! deliberately conservative: it targets the common trailing-marker cases plus
//! body-less code fences, not full `CommonMark` flanking analysis.  A missed edge
//! degrades to an occasional one-tick flicker, never a panic — every slice is
//! taken at a byte offset the scanner produced, so it is always a char boundary.

use std::borrow::Cow;

/// Heals an unclosed trailing marker in a partial streaming buffer.
///
/// Returns [`Cow::Borrowed`] unchanged when the tail is already well-formed.
pub(crate) fn heal_tail(src: &str) -> Cow<'_, str> {
    match fence_state(src) {
        // A body-less opening fence at the tail is held back until its first
        // body char arrives, so the renderer shows one divider, not a phantom
        // pair (pulldown-cmark synthesizes a close at EOF).
        FenceState::OpenBodyless(open_start) => {
            Cow::Owned(src[..open_start].trim_end_matches('\n').to_owned())
        }
        // Inside a fence with body: leave as-is; it renders to EOL correctly.
        FenceState::OpenWithBody => Cow::Borrowed(src),
        FenceState::Closed => {
            // A closed fence whose last physical line IS the delimiter must not
            // be inline-healed: heal_line would read the lone third backtick of
            // ``` as unclosed inline code and truncate the closer.
            let last_start = src.rfind('\n').map_or(0, |i| i + 1);
            if fence_marker(&src[last_start..]).is_some() {
                Cow::Borrowed(src)
            } else {
                heal_inline_tail(src)
            }
        }
    }
}

/// Whether the buffer ends inside a code fence, and if so with/without a body.
enum FenceState {
    Closed,
    OpenWithBody,
    /// Byte offset of the opening fence line that has no body yet.
    OpenBodyless(usize),
}

/// Classifies the buffer's trailing fence state by scanning fence-delimiter lines.
fn fence_state(src: &str) -> FenceState {
    let mut opener: Option<char> = None;
    let mut open_start = 0usize;
    let mut body_seen = false;
    let mut pos = 0usize;
    for line in src.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        match (opener, fence_marker(body)) {
            // Open a fence.
            (None, Some(ch)) => {
                opener = Some(ch);
                open_start = pos;
                body_seen = false;
            }
            // Close only with the same fence char (``` closes ```, ~~~ ~~~).
            (Some(open_ch), Some(ch)) if ch == open_ch => opener = None,
            // Any non-empty line inside the fence counts as a body.
            (Some(_), _) if !body.trim().is_empty() => body_seen = true,
            _ => {}
        }
        pos += line.len();
    }
    match opener {
        None => FenceState::Closed,
        Some(_) if body_seen => FenceState::OpenWithBody,
        Some(_) => FenceState::OpenBodyless(open_start),
    }
}

/// Returns the fence char (backtick or tilde) if `line` opens or closes a fence.
fn fence_marker(line: &str) -> Option<char> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some('`')
    } else if t.starts_with("~~~") {
        Some('~')
    } else {
        None
    }
}

/// Heals the last line of `src`; returns `src` borrowed when nothing changes.
fn heal_inline_tail(src: &str) -> Cow<'_, str> {
    let start = src.rfind('\n').map_or(0, |i| i + 1);
    match heal_line(&src[start..]) {
        None => Cow::Borrowed(src),
        Some(healed) => {
            let mut out = String::with_capacity(start + healed.len());
            out.push_str(&src[..start]);
            out.push_str(&healed);
            Cow::Owned(out)
        }
    }
}

/// An inline emphasis kind tracked while scanning for unclosed markers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Code,
    Bold,
    ItalicStar,
    ItalicUnder,
    Strike,
}

/// An open marker recorded during the scan: its kind and byte positions.
#[expect(
    clippy::struct_field_names,
    reason = "`open`/`content` name the marker's byte range; unambiguous in context"
)]
struct Open {
    kind: Kind,
    /// Byte offset of the marker's first char.
    open: usize,
    /// Byte offset just past the marker (start of its content).
    content: usize,
}

/// The closing token for a marker kind.
fn closer(kind: Kind) -> &'static str {
    match kind {
        Kind::Code => "`",
        Kind::Bold => "**",
        Kind::ItalicStar => "*",
        Kind::ItalicUnder => "_",
        Kind::Strike => "~~",
    }
}

/// Heals a single line, returning the rewritten line or `None` if unchanged.
fn heal_line(line: &str) -> Option<String> {
    let b = line.as_bytes();
    let mut stack: Vec<Open> = Vec::new();
    // An unclosed link is always at the tail (it runs to EOF); record its start
    // and the end of its label text so we can collapse it to plain label text.
    let mut link_cut: Option<(usize, usize)> = None;
    let mut i = 0;

    while i < b.len() {
        match b[i] {
            b'`' => {
                // Inline code disables other markers until its closing backtick.
                let mut j = i + 1;
                while j < b.len() && b[j] != b'`' {
                    j += 1;
                }
                if j < b.len() {
                    i = j + 1;
                } else {
                    stack.push(Open {
                        kind: Kind::Code,
                        open: i,
                        content: i + 1,
                    });
                    i = b.len();
                }
            }
            b'*' if i + 1 < b.len() && b[i + 1] == b'*' => {
                toggle(&mut stack, Kind::Bold, i, i + 2);
                i += 2;
            }
            b'~' if i + 1 < b.len() && b[i + 1] == b'~' => {
                toggle(&mut stack, Kind::Strike, i, i + 2);
                i += 2;
            }
            b'*' | b'_' => {
                let kind = if b[i] == b'*' {
                    Kind::ItalicStar
                } else {
                    Kind::ItalicUnder
                };
                let left_ws = i == 0 || b[i - 1].is_ascii_whitespace();
                let right_ws = i + 1 >= b.len() || b[i + 1].is_ascii_whitespace();
                let top_match = stack.last().is_some_and(|o| o.kind == kind);
                if top_match && !left_ws {
                    // Right-flanking: closes the matching open italic.
                    stack.pop();
                } else if !right_ws && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
                    // Left-flanking and not intra-word (rejects snake_case `_`).
                    stack.push(Open {
                        kind,
                        open: i,
                        content: i + 1,
                    });
                }
                i += 1;
            }
            b'[' => {
                if let Some(skip) = complete_link_len(&b[i..]) {
                    i += skip;
                } else {
                    let label_end = b[i + 1..]
                        .iter()
                        .position(|&x| x == b']')
                        .map_or(b.len(), |p| i + 1 + p);
                    link_cut = Some((i, label_end));
                    i = b.len();
                }
            }
            first => i += utf8_len(first),
        }
    }

    // An unclosed link sits at the tail: collapse `[label](ur` → `label`, then
    // close any inline marks that were still open before it.
    if let Some((cut, label_end)) = link_cut {
        let label = &line[cut + 1..label_end.min(line.len())];
        let mut out = String::with_capacity(line.len());
        out.push_str(&line[..cut]);
        out.push_str(label);
        for o in stack.iter().rev() {
            out.push_str(closer(o.kind));
        }
        return Some(out);
    }

    if stack.is_empty() {
        return None;
    }

    // Truncate the trailing run of *bare* markers (marker with no content yet),
    // then append closers for the remaining open marks that do have content.
    let mut cut = line.len();
    let mut close_upto = stack.len();
    for idx in (0..stack.len()).rev() {
        let o = &stack[idx];
        if line[o.content.min(line.len())..].trim().is_empty() {
            cut = o.open;
            close_upto = idx;
        } else {
            break;
        }
    }

    let mut out = String::with_capacity(line.len() + 4);
    out.push_str(&line[..cut]);
    for o in stack[..close_upto].iter().rev() {
        out.push_str(closer(o.kind));
    }
    if out == line { None } else { Some(out) }
}

/// Toggles a paired marker: closes the matching open one, else opens a new one.
fn toggle(stack: &mut Vec<Open>, kind: Kind, open: usize, content: usize) {
    if let Some(p) = stack.iter().rposition(|o| o.kind == kind) {
        stack.truncate(p);
    } else {
        stack.push(Open {
            kind,
            open,
            content,
        });
    }
}

/// Returns the byte length of a complete `[label](url)` starting at `b[0] == '['`.
fn complete_link_len(b: &[u8]) -> Option<usize> {
    let close_bracket = b.iter().position(|&x| x == b']')?;
    if close_bracket + 1 >= b.len() || b[close_bracket + 1] != b'(' {
        return None;
    }
    let close_paren = b[close_bracket + 1..].iter().position(|&x| x == b')')?;
    Some(close_bracket + 1 + close_paren + 1)
}

/// Byte length of a UTF-8 char from its leading byte (defensive default 1).
fn utf8_len(first: u8) -> usize {
    match first {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heal(s: &str) -> String {
        heal_tail(s).into_owned()
    }

    #[test]
    fn closes_unclosed_bold() {
        assert_eq!(heal("**bold"), "**bold**");
    }

    #[test]
    fn closes_unclosed_code() {
        assert_eq!(heal("a `code"), "a `code`");
    }

    #[test]
    fn closes_unclosed_strike() {
        assert_eq!(heal("~~gone"), "~~gone~~");
    }

    #[test]
    fn closes_unclosed_italic_star_and_underscore() {
        assert_eq!(heal("*em"), "*em*");
        assert_eq!(heal("_em"), "_em_");
    }

    #[test]
    fn truncates_bare_trailing_marker() {
        assert_eq!(heal("text **"), "text ");
        assert_eq!(heal("text `"), "text ");
    }

    #[test]
    fn collapses_unclosed_link() {
        assert_eq!(heal("see [docs](http://exa"), "see docs");
        assert_eq!(heal("see [docs"), "see docs");
    }

    #[test]
    fn leaves_wellformed_unchanged() {
        for s in [
            "plain text",
            "**bold** done",
            "`code` and more",
            "[link](http://x)",
            "snake_case_name",
            "5 * 3 = 15",
            "~~done~~ ok",
        ] {
            assert!(
                matches!(heal_tail(s), Cow::Borrowed(_)),
                "should be unchanged: {s:?}"
            );
        }
    }

    #[test]
    fn backticks_disable_inner_markers() {
        // The `**` lives inside inline code, so nothing is unclosed.
        assert!(matches!(heal_tail("`a ** b`"), Cow::Borrowed(_)));
    }

    #[test]
    fn nested_markers_close_in_order() {
        assert_eq!(heal("**bold _and italic"), "**bold _and italic_**");
    }

    #[test]
    fn open_fence_with_body_is_unchanged() {
        assert!(matches!(heal_tail("```rust\nfn x() {}"), Cow::Borrowed(_)));
    }

    #[test]
    fn bodyless_opening_fence_is_held_back() {
        assert_eq!(heal("intro text\n\n```rust"), "intro text");
    }

    #[test]
    fn closed_fence_then_plain_tail() {
        assert!(matches!(
            heal_tail("```\ncode\n```\nafter"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn multibyte_tail_does_not_panic() {
        // Unclosed bold after CJK content must heal at a char boundary.
        assert_eq!(heal("你好 **世界"), "你好 **世界**");
    }

    #[test]
    fn closed_fence_as_last_line_is_untouched() {
        // The closing ``` must not lose a backtick to inline healing.
        assert!(matches!(heal_tail("```\ncode\n```"), Cow::Borrowed(_)));
        assert!(matches!(
            heal_tail("```rust\nfn x() {}\n```"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn tilde_fence_recognized_not_strikethrough() {
        assert!(matches!(heal_tail("~~~\ncode body"), Cow::Borrowed(_)));
        assert!(matches!(heal_tail("~~~\ncode\n~~~"), Cow::Borrowed(_)));
    }
}
