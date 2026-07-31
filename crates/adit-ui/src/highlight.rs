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

/// How much of the line a match colours.
///
/// iTerm2 splits its highlight trigger into exactly these two actions, and the
/// distinction turns out to matter more than the colour does: a comment coloured
/// only where its `#` sits reads as a stray fragment, while the same comment
/// coloured end to end reads as one thing. No amount of tuning the palette
/// substitutes for picking the right one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    /// Colour the matched text only.
    Match,
    /// Colour the whole line the match appears on.
    Line,
}

/// One highlighting rule: a pattern, and the colour its matches take.
pub(super) struct HighlightRule {
    pattern: Regex,
    scope: Scope,
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

pub(super) struct RuleSpec {
    /// Stable key for persistence and for the settings UI. Deliberately not the
    /// index: reordering the list must not silently repoint someone's saved
    /// preference at a different rule.
    pub(super) id: &'static str,
    /// What the settings dialog calls it.
    pub(super) label: &'static str,
    pattern: &'static str,
    /// A normal ANSI slot (1–6), never a bright one (9–14): bright is what made
    /// the first cut of this feature shout over the text it was annotating.
    pub(super) ansi: u8,
    scope: Scope,
    /// On for everyone, or shipped and waiting to be switched on.
    pub(super) enabled: bool,
}

/// The shipped rules, for the settings dialog to list.
pub(super) fn rules() -> &'static [RuleSpec] {
    DEFAULT_RULES
}

/// The shipped rules, in precedence order — first match on a column wins.
///
/// The comment rules come first on purpose. In a comment line the comment colour
/// should carry the whole line, including any URL inside it; putting `url` first
/// would punch a differently coloured hole through the middle of it.
///
/// Four are on by default. A default is on for everyone, so a wrong one is a bug
/// shipped to every user, and everything enabled here is context-free and rarely
/// coincidental.
const DEFAULT_RULES: &[RuleSpec] = &[
    // Anchored to the start of the line, and that anchoring is what makes the
    // rule safe enough to ship at all. `docs/keyword-highlighting.md` rejected a
    // comment rule outright for misfiring on root prompts and diff markers — but
    // a prompt has `root@host:/path` before its `#`, and `+++`/`---` are not `#`
    // at all, so neither matches once the pattern demands line-start. The
    // objection was to an unanchored rule; it was never examined.
    //
    // Off by default all the same: a Markdown heading is a false positive, and a
    // whole-line colour is a loud way to be wrong.
    RuleSpec { id: "comment-hash", label: "# 注释整行", pattern: r"^\s*#", ansi: 2, scope: Scope::Line, enabled: false },
    RuleSpec { id: "comment-slash", label: "// 注释整行", pattern: r"^\s*//", ansi: 2, scope: Scope::Line, enabled: false },
    RuleSpec { id: "error", label: "错误关键字", pattern: r"\b(?:ERROR|FATAL|CRITICAL)\b", ansi: 1, scope: Scope::Match, enabled: true },
    RuleSpec { id: "warning", label: "警告关键字", pattern: r"\b(?:WARN|WARNING)\b", ansi: 3, scope: Scope::Match, enabled: true },
    RuleSpec { id: "ipv4", label: "IP 地址", pattern: r"\b\d{1,3}(?:\.\d{1,3}){3}\b", ansi: 6, scope: Scope::Match, enabled: true },
    RuleSpec { id: "url", label: "网址", pattern: r"\bhttps?://\S+", ansi: 4, scope: Scope::Match, enabled: true },
];

/// The compiled rule set. Built once and reused — compiling per frame would put
/// regex construction on the render path.
pub(super) struct Highlighter {
    rules: Vec<HighlightRule>,
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::with_overrides(&std::collections::BTreeMap::new())
    }
}

impl Highlighter {
    /// Build the rule set, with `overrides` flipping individual rules by id.
    ///
    /// Only deviations from the shipped defaults are stored, so changing a
    /// default later still reaches everyone who never touched that rule — which
    /// a saved copy of the whole set would quietly prevent.
    pub(super) fn with_overrides(overrides: &std::collections::BTreeMap<String, bool>) -> Self {
        Self {
            rules: DEFAULT_RULES
                .iter()
                .map(|spec| HighlightRule {
                    // Compile-time constants exercised by the tests below, so a
                    // failure is a build mistake rather than bad user input.
                    pattern: Regex::new(spec.pattern)
                        .expect("built-in highlight pattern must compile"),
                    scope: spec.scope,
                    color: TermColor::Indexed(spec.ansi),
                    enabled: overrides
                        .get(spec.id)
                        .copied()
                        .unwrap_or(spec.enabled),
                })
                .collect(),
        }
    }
}

/// The rule set the renderer consults.
///
/// A global for the same reason the font and palette are: the row renderer is a
/// free function with no path back to app state. Behind a lock rather than a
/// `OnceLock` because settings can change it while sessions are open.
fn active() -> &'static std::sync::RwLock<Highlighter> {
    static ACTIVE: std::sync::OnceLock<std::sync::RwLock<Highlighter>> =
        std::sync::OnceLock::new();
    ACTIVE.get_or_init(|| std::sync::RwLock::new(Highlighter::default()))
}

/// Spans for one row. Taken under a read lock — tens of rows per frame, and the
/// write only happens when settings are applied.
pub(super) fn spans_for(line: &TerminalLine) -> Vec<(usize, usize, TermColor)> {
    active()
        .read()
        .map(|highlighter| highlighter.spans(line))
        .unwrap_or_default()
}

/// Rebuild the active rule set. Recompiles the patterns, which is why it is
/// called when settings are applied and never from the render path.
pub(super) fn apply_overrides(overrides: &std::collections::BTreeMap<String, bool>) {
    if let Ok(mut active) = active().write() {
        *active = Highlighter::with_overrides(overrides);
    }
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
            // A match can straddle coloured and uncoloured text — a URL printed
            // inside an already-red error line, say. Either way only the runs the
            // server left alone are emitted, rather than dropping the match whole.
            let ranges: Vec<(usize, usize)> = match rule.scope {
                Scope::Line => {
                    if rule.pattern.is_match(&map.text) {
                        map.paintable_runs(0, map.text.len())
                    } else {
                        Vec::new()
                    }
                }
                Scope::Match => rule
                    .pattern
                    .find_iter(&map.text)
                    .flat_map(|hit| map.paintable_runs(hit.start(), hit.end()))
                    .collect(),
            };
            for (start, end) in ranges {
                // First rule in list order wins. Predictable beats "most
                // specific", which nobody can predict from a settings list.
                if !spans.iter().any(|(s, e, _)| start < *e && *s < end) {
                    spans.push((start, end, rule.color));
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
        let on: Vec<_> = DEFAULT_RULES
            .iter()
            .filter(|spec| spec.enabled)
            .map(|spec| spec.id)
            .collect();
        assert_eq!(on, ["error", "warning", "ipv4", "url"]);
        // The comment rules ship switched off. Turning one on by default is a
        // decision about everyone's screen, not a tweak.
        let off: Vec<_> = DEFAULT_RULES
            .iter()
            .filter(|spec| !spec.enabled)
            .map(|spec| spec.id)
            .collect();
        assert_eq!(off, ["comment-hash", "comment-slash"]);
    }

    /// A rule set with the comment presets switched on, by the same path the
    /// settings dialog uses.
    fn with_comments() -> Highlighter {
        let overrides = DEFAULT_RULES
            .iter()
            .filter(|spec| spec.id.starts_with("comment-"))
            .map(|spec| (spec.id.to_string(), true))
            .collect();
        Highlighter::with_overrides(&overrides)
    }

    #[test]
    fn an_override_only_moves_the_rule_it_names() {
        let overrides = [(String::from("comment-hash"), true)].into_iter().collect();
        let highlighter = Highlighter::with_overrides(&overrides);
        let on: Vec<_> = highlighter
            .rules
            .iter()
            .zip(DEFAULT_RULES)
            .filter(|(rule, _)| rule.enabled)
            .map(|(_, spec)| spec.id)
            .collect();
        // comment-slash stays off; the four defaults stay on.
        assert_eq!(on, ["comment-hash", "error", "warning", "ipv4", "url"]);
    }

    #[test]
    fn a_line_scoped_rule_colours_the_whole_line() {
        // The point of Scope::Line: colouring only where the `#` sits is what
        // made this read as a stray fragment next to other clients.
        let text = "  # Passwords must be encoded using MD5";
        let line = TerminalLine::plain(text);
        let spans = with_comments().spans(&line);
        assert_eq!(spans.len(), 1);
        // Computed, not written out: a hand-counted column is a magic number
        // that is wrong the first time and silently right afterwards.
        assert_eq!((spans[0].0, spans[0].1), (0, text.chars().count()));
    }

    #[test]
    fn a_comment_carries_its_line_through_a_url() {
        // The comment rules precede `url` so a comment stays one colour end to
        // end. Ordered the other way this line would come back in three pieces.
        let line = TerminalLine::plain("# see https://example.com/x for more");
        let spans = with_comments().spans(&line);
        assert_eq!(spans.len(), 1, "expected one unbroken span, got {spans:?}");
    }

    #[test]
    fn a_prompt_is_not_a_comment() {
        // Anchoring to line-start is what makes the comment rule shippable: a
        // prompt's `#` has the user, host and path in front of it.
        let line = TerminalLine::plain("root@node72:/data/traefik# cat .env");
        assert!(with_comments().spans(&line).is_empty());
    }

    #[test]
    fn diff_markers_are_not_comments() {
        for text in ["+++ b/src/lib.rs", "--- a/src/lib.rs", "+added", "-removed"] {
            let line = TerminalLine::plain(text);
            assert!(
                with_comments().spans(&line).is_empty(),
                "{text} should not read as a comment"
            );
        }
    }

    #[test]
    fn ordinary_output_is_untouched() {
        let line = TerminalLine::plain("just some ordinary output");
        assert!(spans_of(&line).is_empty());
    }
}
