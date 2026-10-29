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

use adit_terminal::{Color as TermColor, TerminalCell, TerminalLine};

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
    // A cell is a whole styled *run*, not a character, and `TerminalCell`
    // documents the invariant that goes with it: every cell in a run shares one
    // hyperlink target, and a link boundary starts a new run. So a URL that is
    // only part of a longer run cannot have the target stamped onto that run —
    // the run has to be split at the URL's edges first.
    //
    // Getting this wrong is not subtle: ` * Documentation:   https://…` arrives
    // from a MOTD as one run, and stamping it made the whole line a link.
    let mut text = String::new();
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(line.cells.len());
    for cell in &line.cells {
        let start = text.len();
        text.push_str(&cell.text);
        spans.push((start, text.len()));
    }
    if !text.contains("://") {
        return;
    }
    let urls = find_urls(&text);
    if urls.is_empty() {
        return;
    }

    let mut out: Vec<TerminalCell> = Vec::with_capacity(line.cells.len() + urls.len() * 2);
    for (cell, (cell_start, cell_end)) in std::mem::take(&mut line.cells).into_iter().zip(spans) {
        // Two things this must not take from the server, because a link renders
        // in `hyperlink_color()` and would overwrite them: its own OSC 8 target
        // (a server that deliberately pointed this text elsewhere keeps it), its
        // own colour (the same restraint keyword highlighting uses, so a URL in a
        // syntax-highlighted buffer stays the colour vim gave it). Cursor color
        // remains controlled by the renderer, independently of link detection.
        let eligible = cell.hyperlink.is_none() && cell.fg == TermColor::Default;
        let overlapping: Vec<(usize, usize)> = if eligible {
            urls.iter()
                .map(|&(start, end)| (start.max(cell_start), end.min(cell_end)))
                .filter(|(start, end)| start < end)
                .collect()
        } else {
            Vec::new()
        };
        if overlapping.is_empty() {
            out.push(cell);
            continue;
        }

        let mut cut = cell_start;
        for (start, end) in overlapping {
            if start > cut {
                out.push(slice_of(&cell, &text[cut..start], None));
            }
            // The target is the WHOLE URL even when this piece is only part of
            // one: a URL split across runs has to open the same address from
            // either half.
            let whole = urls
                .iter()
                .find(|&&(url_start, url_end)| url_start <= start && end <= url_end)
                .map(|&(url_start, url_end)| text[url_start..url_end].to_string());
            out.push(slice_of(&cell, &text[start..end], whole));
            cut = end;
        }
        if cut < cell_end {
            out.push(slice_of(&cell, &text[cut..cell_end], None));
        }
    }
    line.cells = out;
}

/// One piece of a split run: same styling, different text, its own target.
fn slice_of(base: &TerminalCell, text: &str, hyperlink: Option<String>) -> TerminalCell {
    TerminalCell {
        text: text.to_string(),
        hyperlink,
        ..base.clone()
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
            // Exactly the character class `is_openable_http_url` accepts, which
            // is what makes "detected" imply "openable".
            //
            // Anything looser silently loses the link entirely: a URL at the end
            // of a Chinese sentence swallowed the 。, one non-ASCII character
            // made the whole thing fail that check, and the renderer drops a
            // hyperlink it cannot open — so the URL rendered as plain text with
            // no colour, no underline and no click. Enumerating CJK punctuation
            // would have fixed that one sentence and left the next script to
            // find. Deferring to the gate cannot drift from it.
            if !ch.is_ascii_graphic() {
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

    fn plain(text: &str) -> TerminalCell {
        TerminalCell::plain(text)
    }

    /// The bug this module shipped with: a MOTD line arrives as ONE run, and
    /// stamping the target onto that run made the entire line a link — visibly,
    /// once links grew an underline.
    #[test]
    fn only_the_url_part_of_a_run_becomes_a_link() {
        let mut line =
            TerminalLine::from_cells(vec![plain(" * Documentation:  https://docs.ubuntu.com")]);
        link_bare_urls(&mut line);

        let linked: String = line
            .cells
            .iter()
            .filter(|cell| cell.hyperlink.is_some())
            .map(|cell| cell.text.as_str())
            .collect();
        assert_eq!(linked, "https://docs.ubuntu.com");

        // Nothing may be lost or duplicated by the split.
        let rebuilt: String = line.cells.iter().map(|cell| cell.text.as_str()).collect();
        assert_eq!(rebuilt, " * Documentation:  https://docs.ubuntu.com");
        // …and the prefix must not have become clickable.
        let prefix = line.cells.first().expect("a leading run");
        assert!(
            prefix.hyperlink.is_none(),
            "the label is not part of the link"
        );
    }

    /// Styling survives the split, or a link would drop the run's bold/italic.
    #[test]
    fn split_pieces_keep_the_runs_styling() {
        let mut line = TerminalLine::from_cells(vec![TerminalCell {
            bold: true,
            ..TerminalCell::plain("see https://example.com ok")
        }]);
        link_bare_urls(&mut line);
        assert!(line.cells.len() >= 2, "the run must actually split");
        assert!(
            line.cells.iter().all(|cell| cell.bold),
            "bold is the run's, not the link's"
        );
    }

    #[test]
    fn a_server_declared_link_is_never_overwritten() {
        let mut line = TerminalLine::from_cells(vec![TerminalCell {
            hyperlink: Some(String::from("https://declared.example")),
            ..TerminalCell::plain("https://shown.example")
        }]);
        link_bare_urls(&mut line);
        assert_eq!(line.cells.len(), 1, "an already-linked run is left whole");
        assert_eq!(
            line.cells[0].hyperlink.as_deref(),
            Some("https://declared.example"),
        );
    }

    #[test]
    fn bare_url_remains_linked_under_the_cursor() {
        let mut line = TerminalLine::from_cells(vec![
            TerminalCell::plain("https://exa"),
            TerminalCell {
                cursor: true,
                ..TerminalCell::plain("m")
            },
            TerminalCell::plain("ple.com"),
        ]);
        link_bare_urls(&mut line);
        let cursor = line.cells.iter().find(|cell| cell.cursor).unwrap();
        assert_eq!(cursor.text, "m");
        assert_eq!(cursor.hyperlink.as_deref(), Some("https://example.com"));
    }

    /// A URL ending a CJK sentence. The full stop is not ASCII, and one non-ASCII
    /// character makes `is_openable_http_url` reject the whole target — at which
    /// point the renderer drops the link and the URL shows as plain text. So this
    /// is not cosmetic trimming: swallowing the 。 loses the link outright.
    #[test]
    fn stops_at_non_ascii_punctuation() {
        assert_eq!(
            urls("见 https://en.wikipedia.org/wiki/Foo_(bar)。"),
            ["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
        assert_eq!(
            urls("链接：https://example.com，然后"),
            ["https://example.com"]
        );
    }

    #[test]
    fn ignores_non_urls() {
        assert!(urls("no links here").is_empty());
        assert!(urls("xhttps://example.com").is_empty(), "must start a word");
        assert!(urls("https://").is_empty(), "scheme alone is not a link");
        assert!(
            urls("ftp://example.com").is_empty(),
            "only http(s) can open"
        );
    }
}
