//! Reading text and selections out of a terminal snapshot.
//!
//! Pure functions over a snapshot — no app state, no rendering. Selections
//! arrive anchored in absolute scrollback rows, which is what lets one survive
//! scrolling; mapping into viewport space happens here, at the point of use.

use super::*;

pub(super) fn max_scroll_offset_for(snapshot: &TerminalSnapshot, rows: usize) -> usize {
    snapshot.total_rows.saturating_sub(rows.max(1))
}

pub(super) fn scroll_delta_to_rows(
    delta: mouse::ScrollDelta,
    metrics: TerminalMetrics,
) -> Option<i32> {
    let raw = match delta {
        mouse::ScrollDelta::Lines { y, .. } => y * 3.0,
        mouse::ScrollDelta::Pixels { y, .. } => y / metrics.cell_height,
    };

    if raw.abs() < f32::EPSILON {
        return None;
    }

    let rounded = raw.round();
    if rounded == 0.0 {
        Some(raw.signum() as i32)
    } else {
        Some(rounded as i32)
    }
}

pub(super) fn snapshot_to_text(snapshot: &TerminalSnapshot) -> String {
    // A wrapped row keeps its trailing blanks — they are the padding the next
    // row continues from, not the ragged end of a line. See `join_rows`.
    let rows: Vec<(String, bool)> = snapshot
        .lines
        .iter()
        .map(|line| {
            let text = if line.wrapped {
                raw_line_text(line)
            } else {
                line_to_text(line)
            };
            (text, line.wrapped)
        })
        .collect();
    join_rows(&rows).trim_end().to_string()
}

/// Stitch rows back into logical lines: a row marked `wrapped` runs straight
/// into the next one, any other ends a line.
///
/// This is the whole of the bug it was written for. The terminal breaks a long
/// line to fit the window, and copying it row by row hands back a newline the
/// remote never sent. Paste a container name that wrapped after `neos` and the
/// shell runs `docker logs -f neos`, then `tor-agent-chonburi` — two commands,
/// neither of them the one that was copied. It reads as "the terminal mangled
/// my clipboard" rather than as a wrapping question at all, because the text on
/// screen was right the whole time.
fn join_rows(rows: &[(String, bool)]) -> String {
    let mut out = String::new();
    for (index, (text, wrapped)) in rows.iter().enumerate() {
        out.push_str(text);
        if !wrapped && index + 1 < rows.len() {
            out.push('\n');
        }
    }
    out
}

pub(super) fn line_to_text(line: &TerminalLine) -> String {
    raw_line_text(line).trim_end().to_string()
}

pub(super) fn raw_line_text(line: &TerminalLine) -> String {
    line.cells
        .iter()
        .map(|cell| cell.text.as_str())
        .collect::<String>()
}

pub(super) fn selection_to_text(
    snapshot: &TerminalSnapshot,
    selection: TerminalSelection,
) -> String {
    let Some((start, end)) = normalized_selection(selection) else {
        return String::new();
    };

    let mut rows: Vec<(String, bool)> = Vec::new();
    for row_index in start.row..=end.row {
        // Selection rows are absolute scrollback rows; the snapshot holds a window
        // starting at `first_row`.
        let Some(line) = row_index
            .checked_sub(snapshot.first_row)
            .and_then(|i| snapshot.lines.get(i))
        else {
            continue;
        };

        // Only a row the selection runs *past* can be soft-wrapped into the next
        // one. The last row is where the selection stops, so whatever follows it
        // was not asked for and must not be joined on.
        let wrapped = line.wrapped && row_index < end.row;

        let text = raw_line_text(line);
        let start_col = if row_index == start.row { start.col } else { 0 };
        let end_col = if row_index == end.row {
            end.col
        } else {
            usize::MAX
        };
        rows.push((slice_columns(&text, start_col, end_col), wrapped));
    }

    join_rows(&rows).trim_end().to_string()
}

fn slice_columns(text: &str, start: usize, end: usize) -> String {
    let mut col = 0;
    adit_terminal::glyphs(text)
        .filter(|glyph| {
            let width = adit_terminal::glyph_width(glyph);
            let selected = col < end && col + width > start;
            col += width;
            selected
        })
        .collect()
}

pub(super) fn normalized_selection(
    selection: TerminalSelection,
) -> Option<(TerminalPoint, TerminalPoint)> {
    let (start, end) =
        if (selection.start.row, selection.start.col) <= (selection.end.row, selection.end.col) {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };

    if start == end {
        None
    } else {
        Some((start, end))
    }
}

/// Text of an ABSOLUTE scrollback row, resolved against the snapshot's window.
pub(super) fn snapshot_line_text(snapshot: &TerminalSnapshot, row: usize) -> String {
    row.checked_sub(snapshot.first_row)
        .and_then(|i| snapshot.lines.get(i))
        .map(raw_line_text)
        .unwrap_or_default()
}

/// Word span `[start, end)` (char indices) around `col` for double-click select.
/// "Word" chars are alphanumerics plus the punctuation that commonly appears
/// mid-token in paths, URLs, and hostnames, so `/usr/local/bin` stays one word.
pub(super) fn word_bounds(line: &str, col: usize) -> Option<(usize, usize)> {
    let glyphs: Vec<&str> = adit_terminal::glyphs(line).collect();
    let mut offsets = vec![0];
    for glyph in &glyphs {
        offsets.push(offsets.last().copied().unwrap_or(0) + adit_terminal::glyph_width(glyph));
    }
    let index = offsets
        .windows(2)
        .position(|span| col >= span[0] && col < span[1])?;
    let is_word =
        |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~' | ':' | '@' | '+');
    let word = |index: usize| {
        glyphs[index]
            .chars()
            .find(|c| UnicodeWidthChar::width(*c).unwrap_or(0) > 0)
            .is_some_and(is_word)
    };
    if !word(index) {
        // On whitespace or a separator: grab just that single cell.
        return Some((offsets[index], offsets[index + 1]));
    }
    let mut start = index;
    while start > 0 && word(start - 1) {
        start -= 1;
    }
    let mut end = index + 1;
    while end < glyphs.len() && word(end) {
        end += 1;
    }
    Some((offsets[start], offsets[end]))
}

pub(super) fn selection_range_for_row(
    selection: TerminalSelection,
    row: usize,
) -> Option<(usize, usize)> {
    let (start, end) = normalized_selection(selection)?;
    if row < start.row || row > end.row {
        return None;
    }

    let start_col = if row == start.row { start.col } else { 0 };
    let end_col = if row == end.row { end.col } else { usize::MAX };

    (start_col < end_col).then_some((start_col, end_col))
}

/// Map an absolute-row selection into viewport row space for rendering, clipping
/// to the visible window. `None` when the selection is entirely off-screen. A row
/// clipped off the top starts at column 0; one clipped off the bottom runs to EOL
/// (`usize::MAX`), matching `selection_range_for_row`'s convention.
pub(super) fn selection_for_viewport(
    selection: TerminalSelection,
    first_row: usize,
    rows: usize,
) -> Option<TerminalSelection> {
    let (start, end) = normalized_selection(selection)?;
    let last_row = first_row + rows.saturating_sub(1);
    if end.row < first_row || start.row > last_row {
        return None;
    }

    let start = if start.row < first_row {
        TerminalPoint { row: 0, col: 0 }
    } else {
        TerminalPoint {
            row: start.row - first_row,
            col: start.col,
        }
    };
    let end = if end.row > last_row {
        TerminalPoint {
            row: rows.saturating_sub(1),
            col: usize::MAX,
        }
    } else {
        TerminalPoint {
            row: end.row - first_row,
            col: end.col,
        }
    };
    Some(TerminalSelection { start, end })
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    #[test]
    fn column_selection_keeps_combining_and_wide_glyphs_whole() {
        assert_eq!(slice_columns("e\u{301}\u{754c}x", 0, 1), "e\u{301}");
        assert_eq!(slice_columns("e\u{301}\u{754c}x", 1, 3), "\u{754c}");
        assert_eq!(slice_columns("e\u{301}\u{754c}x", 3, 4), "x");
        assert_eq!(
            slice_columns("\u{1f469}\u{200d}\u{1f4bb}!", 0, 2),
            "\u{1f469}\u{200d}\u{1f4bb}"
        );
    }

    #[test]
    fn word_selection_returns_grid_columns() {
        assert_eq!(word_bounds("e\u{301}clair ok", 1), Some((0, 6)));
        assert_eq!(word_bounds("\u{754c} x", 1), Some((0, 2)));
        assert_eq!(word_bounds("\u{754c} x", 2), Some((2, 3)));
    }
}
