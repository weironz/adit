//! Making bare `http(s)://…` in terminal output clickable.
//!
//! A server that emits OSC 8 gets its links for free — the escape carries the
//! target and the cell already holds it. Almost nothing does: `curl`, `git`,
//! `npm`, a CI log and every `echo` print a plain URL, and those are the ones
//! anyone actually wants to click.
//!
//! This annotates cells that have no hyperlink of their own, which keeps two
//! properties that matter. A real OSC 8 link is never overwritten, so a server
//! that deliberately points its text somewhere else still wins — and the *text*
//! is what becomes the target here, so what is shown is what is opened. Both
//! feed the same confirm-before-open dialog as OSC 8 (`Message::OpenHyperlink`),
//! which is the actual defence: terminal output is remote-controlled, and the
//! destination is shown before anything launches.

use adit_terminal::{Color as TermColor, TerminalLine};

/// Schemes worth linking. Deliberately short: the click path only opens
/// http(s) anyway (`is_openable_http_url`), so linking `ftp://` would render as
/// clickable and then refuse.
const SCHEMES: [&str; 2] = ["https://", "http://"];

/// Trailing characters a URL almost never really ends with.
///
/// Prose puts URLs in sentences (`see https://example.com.`) and in brackets
/// (`(https://example.com)`), and taking the punctuation along is how an
/// autolinker produces targets that 404. Closing brackets are trimmed only when
/// unbalanced, so a genuine `…/Foo_(bar)` survives.
const TRAILING: [char; 10] = ['.', ',', ';', ':', '!', '?', '\'', '"', '>', '`'];

/// Give every bare URL on this line a hyperlink, in place.
///
/// Cheap-exits on lines with no `://` at all, which is nearly all of them — this
/// runs per visible row on every frame.
pub(crate) fn link_bare_urls(line: &mut TerminalLine) {
    // One flat string plus a map back to cell indices. A cell is not one char:
    // it holds a whole styled run, so the map is per byte of the flattened text.
    let mut text = String::new();
    let mut owner: Vec<usize> = Vec::new();
    for (index, cell) in line.cells.iter().enumerate() {
        for ch in cell.text.chars() {
            text.push(ch);
            owner.extend(std::iter::repeat_n(index, ch.len_utf8()));
        }
    }
    if !text.contains("://") {
        return;
    }

    for (start, end) in find_urls(&text) {
        // Two things this must not take from the server, both because a link is
        // rendered in `hyperlink_color()` and would overwrite them:
        //
        // its own OSC 8 target — a server that deliberately pointed this text
        // somewhere else keeps that — and its own colour. The same restraint
        // keyword highlighting uses (`ColumnMap::of`): a URL inside a syntax-
        // highlighted buffer stays the colour vim gave it rather than turning
        // link-blue mid-line.
        let claimed = owner[start..end].iter().any(|&cell| {
            let cell = &line.cells[cell];
            cell.hyperlink.is_some() || cell.fg != TermColor::Default
        });
        if claimed {
            continue;
        }
        let url = text[start..end].to_string();
        for &cell in &owner[start..end] {
            line.cells[cell].hyperlink = Some(url.clone());
        }
    }
}

/// Byte ranges of the bare URLs in `text`.
fn find_urls(text: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut cursor = 0_usize;

    while cursor < text.len() {
        let Some((start, scheme)) = SCHEMES
            .iter()
            .filter_map(|scheme| text[cursor..].find(scheme).map(|at| (cursor + at, *scheme)))
            .min_by_key(|(at, _)| *at)
        else {
            break;
        };

        // The scheme must start a word: `xhttps://…` is not a URL, and neither
        // is the tail of one already matched.
        let boundary_ok = text[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_alphanumeric() && ch != '.' && ch != '/');
        let mut end = start + scheme.len();
        while let Some(ch) = text[end..].chars().next() {
            // Whitespace ends a URL; so do the control characters a terminal can
            // hold. Everything else is allowed through and trimmed below, which
            // is more forgiving than a whitelist for the query strings and
            // encoded paths that turn up in real output.
            if ch.is_whitespace() || ch.is_control() {
                break;
            }
            end += ch.len_utf8();
        }
        let trimmed = trim_trailing(&text[start..end]);
        // A scheme with nothing after it is not a link.
        if boundary_ok && trimmed > scheme.len() {
            found.push((start, start + trimmed));
        }
        cursor = end.max(start + scheme.len());
    }

    found
}

/// Length of `candidate` with sentence punctuation trimmed off the end.
fn trim_trailing(candidate: &str) -> usize {
    let mut end = candidate.len();
    loop {
        let Some(ch) = candidate[..end].chars().next_back() else {
            return end;
        };
        let unbalanced_close = |open: char, close: char| {
            ch == close
                && candidate[..end].matches(open).count() < candidate[..end].matches(close).count()
        };
        if TRAILING.contains(&ch) || unbalanced_close('(', ')') || unbalanced_close('[', ']') {
            end -= ch.len_utf8();
        } else {
            return end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(text: &str) -> Vec<&str> {
        find_urls(text)
            .into_iter()
            .map(|(start, end)| &text[start..end])
            .collect()
    }

    #[test]
    fn finds_plain_urls() {
        assert_eq!(urls("see https://example.com now"), ["https://example.com"]);
        assert_eq!(urls("http://a.b/c?d=1&e=2"), ["http://a.b/c?d=1&e=2"]);
        assert_eq!(
            urls("two https://a.example and http://b.example"),
            ["https://a.example", "http://b.example"]
        );
    }

    /// Prose punctuation is not part of the address — taking it along is how an
    /// autolinker hands you a 404.
    #[test]
    fn trims_sentence_punctuation() {
        assert_eq!(urls("go to https://example.com."), ["https://example.com"]);
        assert_eq!(urls("(https://example.com)"), ["https://example.com"]);
        assert_eq!(urls("<https://example.com>"), ["https://example.com"]);
    }

    /// …but a bracket that belongs to the path stays, or Wikipedia links break.
    #[test]
    fn keeps_balanced_brackets_inside_the_path() {
        assert_eq!(
            urls("https://en.wikipedia.org/wiki/Foo_(bar)"),
            ["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
    }

    #[test]
    fn ignores_non_urls() {
        assert!(urls("no links here").is_empty());
        assert!(urls("xhttps://example.com").is_empty(), "must start a word");
        assert!(urls("https://").is_empty(), "scheme alone is not a link");
        assert!(urls("ftp://example.com").is_empty(), "only http(s) can open");
    }
}
