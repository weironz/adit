//! Local keyword highlighting: colour output the server sent *uncoloured*.
//!
//! Design and scope live in `docs/keyword-highlighting.md`; the two rules that
//! shape this module are worth repeating where they are enforced.
//!
//! **Never repaint a cell the server coloured.** Server colour classifies things
//! we cannot reconstruct — blue is a directory, red an archive, green and red the
//! two sides of a diff. Painting over it destroys information and reads as a
//! rendering bug. `Color::Default` is exactly "the pen was at SGR 39 here", so it
//! is the whole test, and [`Highlighter::spans`] never emits a span outside it.
//!
//! **Spans are columns, not byte offsets.** The renderer walks a line by display
//! column so wide glyphs stay aligned, so matches are translated out of byte
//! space before they leave this module.

use adit_terminal::{Color as TermColor, TerminalLine};
use regex::Regex;
use unicode_width::UnicodeWidthChar;

/// One highlighting rule: a pattern, and the colour its matches take.
pub(super) struct HighlightRule {
    pattern: Regex,
    /// An ANSI palette index, not an RGB value.
    ///
    /// Hardcoded RGB was the first cut and it looked wrong beside real output:
    /// every other colour on screen comes from the scheme's 16-colour palette,
    /// so four off-palette hues read as garish intrusions rather than as part of
    /// the terminal. Going through the palette also makes a highlight follow
    /// whatever scheme the user picked instead of fighting it.
    color: TermColor,
    enabled: bool,
}

/// The shipped defaults, as `(id, pattern, rgb)`.
///
/// Deliberately four. A default is on for everyone, so a wrong one is a bug
/// shipped to every user — and the tempting patterns are the dangerous ones: a
/// `#`-comment rule misfires on diff markers and root prompts, and a `$`-prefix
/// rule shreds a bcrypt hash into differently coloured fragments. Those ship
/// later as presets that are off by default. Everything here is context-free and
/// rarely coincidental.
///
/// The ids exist so a rule can be named without quoting its pattern; nothing
/// reads them at runtime yet, which is why they live here rather than on
/// [`HighlightRule`].
/// The normal ANSI slots (1–6), never the bright ones (9–14): bright is what
/// made the first cut of this feature shout over the text it was annotating.
const DEFAULT_RULES: &[(&str, &str, u8)] = &[
    ("error", r"\b(?:ERROR|FATAL|CRITICAL)\b", 1),
    ("warning", r"\b(?:WARN|WARNING)\b", 3),
    ("ipv4", r"\b\d{1,3}(?:\.\d{1,3}){3}\b", 6),
    ("url", r"\bhttps?://\S+", 4),
];

/// The compiled rule set. Built once and reused — compiling per frame would put
/// regex construction on the render path.
pub(super) struct Highlighter {
    rules: Vec<HighlightRule>,
}

impl Default for Highlighter {
    fn default() -> Self {
        Self {
            rules: DEFAULT_RULES
                .iter()
                .map(|(_, pattern, ansi)| HighlightRule {
                    // Compile-time constants exercised by the tests below, so a
                    // failure is a build mistake rather than bad user input.
                    pattern: Regex::new(pattern)
                        .expect("built-in highlight pattern must compile"),
                    color: TermColor::Indexed(*ansi),
                    enabled: true,
                })
                .collect(),
        }
    }
}

/// The process-wide rule set.
///
/// A global while the rules are hardcoded, which they are for as long as there
/// is no settings surface to change them from. Step 2 of the design moves them
/// into the profile store, at which point this becomes app state and the call
/// site passes a reference instead.
pub(super) fn highlighter() -> &'static Highlighter {
    static HIGHLIGHTER: std::sync::OnceLock<Highlighter> = std::sync::OnceLock::new();
    HIGHLIGHTER.get_or_init(Highlighter::default)
}

impl Highlighter {
    /// Column spans to recolour on `line`, as `(start_col, end_col, colour)`.
    ///
    /// Empty for a line the server coloured throughout, which is the common case
    /// on exactly the output this feature exists to leave alone.
    pub(super) fn spans(&self, line: &TerminalLine) -> Vec<(usize, usize, TermColor)> {
        let map = ColumnMap::of(line);
        if map.text.is_empty() || !map.any_paintable {
            return Vec::new();
        }

        let mut spans: Vec<(usize, usize, TermColor)> = Vec::new();
        for rule in self.rules.iter().filter(|rule| rule.enabled) {
            for hit in rule.pattern.find_iter(&map.text) {
                // A match can straddle coloured and uncoloured text — a URL
                // printed inside an already-red error line, say. Emit the runs
                // the server left alone rather than dropping the match whole.
                for (start, end) in map.paintable_runs(hit.start(), hit.end()) {
                    // First rule in list order wins. Predictable beats "most
                    // specific", which nobody can predict from a settings list.
                    if !spans.iter().any(|(s, e, _)| start < *e && *s < end) {
                        spans.push((start, end, rule.color));
                    }
                }
            }
        }
        spans
    }
}

/// A line flattened to text, with each char's display column and whether the
/// server left it uncoloured.
struct ColumnMap {
    text: String,
    /// One entry per char of `text`, in order: `(column, paintable)`.
    chars: Vec<(usize, bool)>,
    any_paintable: bool,
}

impl ColumnMap {
    fn of(line: &TerminalLine) -> Self {
        let mut text = String::new();
        let mut chars = Vec::new();
        let mut column = 0_usize;
        let mut any_paintable = false;

        for cell in &line.cells {
            // The cursor cell is painted reverse-video by the renderer, and a
            // hyperlink already carries its own colour; neither is ours to take.
            let paintable =
                cell.fg == TermColor::Default && !cell.cursor && cell.hyperlink.is_none();
            any_paintable |= paintable;
            for ch in cell.text.chars() {
                text.push(ch);
                chars.push((column, paintable));
                // Mirrors the renderer's own advance, so a wide glyph does not
                // slide every later span one column to the left.
                column += UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
            }
        }

        Self {
            text,
            chars,
            any_paintable,
        }
    }

    /// Maximal `(start_col, end_col)` runs of paintable columns within the byte
    /// range `[start, end)`.
    fn paintable_runs(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        let mut runs = Vec::new();
        let mut open: Option<(usize, usize)> = None;

        for (index, (byte, ch)) in self.text.char_indices().enumerate() {
            if byte < start {
                continue;
            }
            if byte >= end {
                break;
            }
            let Some(&(column, paintable)) = self.chars.get(index) else {
                break;
            };
            let width = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
            match (paintable, open.as_mut()) {
                (true, Some(run)) => run.1 = column + width,
                (true, None) => open = Some((column, column + width)),
                (false, _) => {
                    if let Some(run) = open.take() {
                        runs.push(run);
                    }
                }
            }
        }
        runs.extend(open);
        runs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adit_terminal::TerminalCell;

    fn coloured(text: &str) -> TerminalCell {
        TerminalCell {
            fg: TermColor::Indexed(2),
            ..TerminalCell::plain(text)
        }
    }

    fn spans_of(line: &TerminalLine) -> Vec<(usize, usize)> {
        Highlighter::default()
            .spans(line)
            .into_iter()
            .map(|(start, end, _)| (start, end))
            .collect()
    }

    #[test]
    fn matches_uncoloured_text() {
        let line = TerminalLine::plain("boot: ERROR disk full");
        assert_eq!(spans_of(&line), vec![(6, 11)]);
    }

    #[test]
    fn never_repaints_what_the_server_coloured() {
        // The constraint the whole feature rests on: `ls --color` red means
        // "archive", and a rule firing inside it would destroy that meaning.
        let line = TerminalLine::from_cells([coloured("ERROR and 10.0.0.1")]);
        assert!(
            spans_of(&line).is_empty(),
            "a server-coloured line must come back untouched"
        );
    }

    #[test]
    fn a_match_straddling_colour_keeps_only_the_uncoloured_part() {
        let line = TerminalLine::from_cells([TerminalCell::plain("WA"), coloured("RN done")]);
        assert_eq!(spans_of(&line), vec![(0, 2)]);
    }

    #[test]
    fn columns_account_for_wide_glyphs() {
        // Two CJK glyphs plus a space occupy five columns, so the match starts
        // at 5 — byte offsets would have put it at 7 and shifted the paint left.
        let line = TerminalLine::plain("测试 ERROR");
        assert_eq!(spans_of(&line), vec![(5, 10)]);
    }

    #[test]
    fn overlapping_matches_do_not_double_paint() {
        // The URL rule spans the address the ipv4 rule also matches.
        let line = TerminalLine::plain("see http://10.0.0.1/x now");
        let spans = spans_of(&line);
        assert_eq!(spans.len(), 1, "expected one span, got {spans:?}");
    }

    #[test]
    fn the_cursor_cell_is_left_alone() {
        // The renderer paints it reverse-video; recolouring would fight that.
        let line = TerminalLine::from_cells([TerminalCell {
            cursor: true,
            ..TerminalCell::plain("ERROR")
        }]);
        assert!(spans_of(&line).is_empty());
    }

    #[test]
    fn a_hyperlink_keeps_its_own_colour() {
        let line = TerminalLine::from_cells([TerminalCell {
            hyperlink: Some(String::from("https://example.com")),
            ..TerminalCell::plain("https://example.com")
        }]);
        assert!(spans_of(&line).is_empty());
    }

    #[test]
    fn the_default_set_stays_small() {
        // Defaults are on for everyone, so a wrong one is a bug shipped to every
        // user. Adding to this list is a decision, not a tweak — the tempting
        // patterns are exactly the dangerous ones (a `#` rule misfires on diff
        // markers and root prompts; a `$` rule shreds a bcrypt hash). Anything
        // new belongs in the off-by-default presets until it has earned better.
        let ids: Vec<_> = DEFAULT_RULES.iter().map(|(id, ..)| *id).collect();
        assert_eq!(ids, ["error", "warning", "ipv4", "url"]);
        assert_eq!(Highlighter::default().rules.len(), DEFAULT_RULES.len());
    }

    #[test]
    fn ordinary_output_is_untouched() {
        let line = TerminalLine::plain("just some ordinary output");
        assert!(spans_of(&line).is_empty());
    }
}
