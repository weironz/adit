//! Reading text and selections out of a terminal snapshot.
//!
//! Pure functions over a snapshot — no app state, no rendering. Selections
//! arrive anchored in absolute scrollback rows, which is what lets one survive
//! scrolling; mapping into viewport space happens here, at the point of use.

use super::*;

pub(super) fn max_scroll_offset_for(snapshot: &TerminalSnapshot, rows: usize) -> usize {
    snapshot.total_rows.saturating_sub(rows.max(1))
}

pub(super) fn scroll_delta_to_rows(delta: mouse::ScrollDelta) -> Option<i32> {
    let raw = match delta {
        mouse::ScrollDelta::Lines { y, .. } => y * 3.0,
        mouse::ScrollDelta::Pixels { y, .. } => y / cell_height(),
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
    snapshot
        .lines
        .iter()
        .map(line_to_text)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
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

pub(super) fn selection_to_text(snapshot: &TerminalSnapshot, selection: TerminalSelection) -> String {
    let Some((start, end)) = normalized_selection(selection) else {
        return String::new();
    };

    let mut lines = Vec::new();
    for row_index in start.row..=end.row {
        // Selection rows are absolute scrollback rows; the snapshot holds a window
        // starting at `first_row`.
        let Some(line) = row_index
            .checked_sub(snapshot.first_row)
            .and_then(|i| snapshot.lines.get(i))
        else {
            continue;
        };

        let text = raw_line_text(line);
        let chars = text.chars().collect::<Vec<_>>();
        let start_col = if row_index == start.row { start.col } else { 0 };
        let end_col = if row_index == end.row {
            end.col
        } else {
            chars.len()
        };

        if start_col >= end_col || start_col >= chars.len() {
            lines.push(String::new());
            continue;
        }

        let end_col = end_col.min(chars.len());
        lines.push(chars[start_col..end_col].iter().collect::<String>());
    }

    lines.join("\n").trim_end().to_string()
}

pub(super) fn normalized_selection(selection: TerminalSelection) -> Option<(TerminalPoint, TerminalPoint)> {
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
    let chars: Vec<char> = line.chars().collect();
    if col >= chars.len() {
        return None;
    }
    let is_word =
        |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~' | ':' | '@' | '+');
    if !is_word(chars[col]) {
        // On whitespace or a separator: grab just that single cell.
        return Some((col, col + 1));
    }
    let mut start = col;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col + 1;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    Some((start, end))
}

pub(super) fn selection_range_for_row(selection: TerminalSelection, row: usize) -> Option<(usize, usize)> {
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
