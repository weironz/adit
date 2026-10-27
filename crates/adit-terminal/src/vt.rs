//! A self-contained VT/ANSI terminal grid.
//!
//! [`VtTerminal`] owns a screen grid, scrollback, cursor, and SGR pen, and is
//! driven by the [`vte`] escape-sequence parser via the [`Perform`] trait. It
//! implements enough of the xterm control set to render real shell output:
//! SGR colors/attributes, cursor motion, erase/scroll, scroll regions,
//! insert/delete, the alternate screen, and device-status replies.

use crate::{
    Color, LogicalAnchor, TerminalCell, TerminalChangeSet, TerminalCore, TerminalLine,
    TerminalSize, TerminalSnapshot, Viewport,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

const DEFAULT_SCROLLBACK: usize = 5000;
const TAB_WIDTH: usize = 8;
const MAX_GLYPH_BYTES: usize = 128;
const MAX_GRAPHIC_TILES: usize = 16_384;

#[derive(Debug, Clone, Copy, Default)]
enum Charset {
    #[default]
    Ascii,
    Uk,
    DecSpecial,
}

impl Charset {
    fn map(self, c: char) -> char {
        match self {
            Self::Uk if c == '#' => '\u{00a3}',
            Self::DecSpecial if ('_'..='~').contains(&c) => {
                const DEC: [char; 32] = [
                    ' ', '\u{25c6}', '\u{2592}', '\u{2409}', '\u{240c}', '\u{240d}', '\u{240a}',
                    '\u{00b0}', '\u{00b1}', '\u{2424}', '\u{240b}', '\u{2518}', '\u{2510}',
                    '\u{250c}', '\u{2514}', '\u{253c}', '\u{23ba}', '\u{23bb}', '\u{2500}',
                    '\u{23bc}', '\u{23bd}', '\u{251c}', '\u{2524}', '\u{2534}', '\u{252c}',
                    '\u{2502}', '\u{2264}', '\u{2265}', '\u{03c0}', '\u{2260}', '\u{00a3}',
                    '\u{00b7}',
                ];
                DEC[c as usize - '_' as usize]
            }
            _ => c,
        }
    }
}

enum Dcs {
    Sixel {
        bytes: Vec<u8>,
        transparent: bool,
        aspect: u16,
    },
    Status(Vec<u8>),
}

/// Max scrollback rows kept per terminal. A global user preference (like a
/// theme) so it applies to every session without threading it through each
/// terminal; clamped to a sane floor.
static SCROLLBACK_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_SCROLLBACK);

/// Set the global scrollback line limit (clamped to at least 200).
pub fn set_scrollback_limit(lines: usize) {
    SCROLLBACK_LIMIT.store(lines.max(200), Ordering::Relaxed);
}

fn scrollback_limit() -> usize {
    SCROLLBACK_LIMIT.load(Ordering::Relaxed)
}

// Cell attribute bit flags.
const BOLD: u16 = 1 << 0;
const DIM: u16 = 1 << 1;
const ITALIC: u16 = 1 << 2;
const UNDERLINE: u16 = 1 << 3;
const REVERSE: u16 = 1 << 4;
const HIDDEN: u16 = 1 << 5;
const STRIKE: u16 = 1 << 6;

/// The current SGR drawing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: u16,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            flags: 0,
        }
    }
}

/// One grid cell. `spacer` marks the right half of a double-width glyph; that
/// column is owned by the wide character immediately to its left. `link` is an
/// OSC 8 hyperlink id (0 ⇒ none) into the state's interning table — kept OFF the
/// [`Pen`] so an SGR reset mid-link doesn't drop the hyperlink.
#[derive(Debug, Clone)]
struct Cell {
    ch: char,
    pen: Pen,
    spacer: bool,
    link: u32,
    /// Only complex glyphs allocate. Cloning for reflow shares their bytes.
    glyph: Option<Arc<str>>,
    image: Option<Arc<Vec<u8>>>,
    /// Reflow drops wide spacers. Carry their graphics on the lead cell until
    /// the new spacer is created (also across a temporary one-column resize).
    right_image: Option<Arc<Vec<u8>>>,
}

impl Cell {
    fn blank(bg: Color) -> Self {
        Self {
            ch: ' ',
            pen: Pen {
                fg: Color::Default,
                bg,
                flags: 0,
            },
            spacer: false,
            link: 0,
            glyph: None,
            image: None,
            right_image: None,
        }
    }

    fn text(&self) -> String {
        self.glyph
            .as_deref()
            .map_or_else(|| self.ch.to_string(), str::to_owned)
    }

    fn width(&self) -> usize {
        self.glyph
            .as_deref()
            .map_or_else(|| self.ch.width().unwrap_or(1).max(1), crate::glyph_width)
    }
}

/// One grid row. `wrapped` means the text ran off the right margin and continues
/// on the next row — a *soft* wrap, as opposed to a real newline.
///
/// Nothing else can recover that distinction after the fact, and without it
/// reflow is impossible: a 120-character echo on an 80-column screen and two
/// separate 60-character echoes both look like two rows, and only the first may
/// be rejoined when the width changes.
///
/// `Deref` to the cell slice keeps every `grid[row][col]`, `.len()`, and slice
/// site working unchanged.
#[derive(Debug, Clone)]
struct Row {
    cells: Vec<Cell>,
    wrapped: bool,
    dirty: bool,
}

impl Row {
    fn blank(cols: usize, bg: Color) -> Self {
        Self {
            cells: vec![Cell::blank(bg); cols],
            wrapped: false,
            dirty: true,
        }
    }
}

impl std::ops::Deref for Row {
    type Target = [Cell];

    fn deref(&self) -> &[Cell] {
        &self.cells
    }
}

impl std::ops::DerefMut for Row {
    fn deref_mut(&mut self) -> &mut [Cell] {
        self.dirty = true;
        &mut self.cells
    }
}

#[derive(Debug, Clone, Copy)]
struct SavedCursor {
    row: usize,
    col: usize,
    pen: Pen,
    pending_wrap: bool,
    charsets: [Charset; 4],
    active_charset: usize,
}

/// The primary-screen state stashed while the alternate screen is active.
#[derive(Debug, Clone)]
struct AltSaved {
    grid: Vec<Row>,
    cursor_row: usize,
    cursor_col: usize,
    pen: Pen,
    saved_cursor: Option<SavedCursor>,
    scroll_top: usize,
    scroll_bottom: usize,
    autowrap: bool,
    current_link: u32,
    charsets: [Charset; 4],
    active_charset: usize,
}

/// The mutable terminal model. Kept separate from the [`Parser`] so a single
/// `feed` can borrow the parser and this state as disjoint fields.
struct TermState {
    cols: usize,
    rows: usize,
    grid: Vec<Row>,
    scrollback: VecDeque<Row>,
    cursor_row: usize,
    cursor_col: usize,
    pen: Pen,
    scroll_top: usize,
    scroll_bottom: usize,
    pending_wrap: bool,
    autowrap: bool,
    cursor_visible: bool,
    bracketed_paste: bool,
    mouse_mode: crate::MouseMode,
    mouse_sgr: bool,
    saved_cursor: Option<SavedCursor>,
    alt: Option<AltSaved>,
    title: String,
    bell: bool,
    responses: Vec<u8>,
    /// OSC 8 hyperlink targets, interned; a cell's `link` id is `index + 1`
    /// (0 ⇒ no link). `current_link` is the id applied to newly-printed cells.
    links: Vec<String>,
    current_link: u32,
    charsets: [Charset; 4],
    active_charset: usize,
    single_shift: Option<usize>,
    tabs: Vec<bool>,
    dcs: Option<Dcs>,
    pending_dcs: Option<Dcs>,
    sixel_scrolling: bool,
    force_dirty: bool,
    /// Only consecutive printing may extend a cluster. Cursor/erase controls
    /// break the association, including a control split across feed calls.
    last_printed: Option<(usize, usize)>,
}

impl TermState {
    fn new(cols: usize, rows: usize, title: String) -> Self {
        Self {
            cols,
            rows,
            grid: blank_grid(cols, rows),
            scrollback: VecDeque::new(),
            cursor_row: 0,
            cursor_col: 0,
            pen: Pen::default(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            pending_wrap: false,
            autowrap: true,
            cursor_visible: true,
            bracketed_paste: false,
            mouse_mode: crate::MouseMode::Off,
            mouse_sgr: false,
            saved_cursor: None,
            alt: None,
            title,
            bell: false,
            responses: Vec::new(),
            links: Vec::new(),
            current_link: 0,
            charsets: [Charset::Ascii; 4],
            active_charset: 0,
            single_shift: None,
            tabs: (0..cols)
                .map(|col| col > 0 && col % TAB_WIDTH == 0)
                .collect(),
            dcs: None,
            pending_dcs: None,
            sixel_scrolling: true,
            force_dirty: false,
            last_printed: None,
        }
    }

    // --- character output ---------------------------------------------------

    /// Intern an OSC 8 URI, returning its 1-based id (`0` for an empty/rejected
    /// URI). De-dupes, and caps both URL length and table size so a hostile
    /// stream can't exhaust memory.
    fn intern_link(&mut self, uri: &str) -> u32 {
        const MAX_URL_LEN: usize = 4096;
        const MAX_LINKS: usize = 4096;
        let uri = uri.trim();
        if uri.is_empty() || uri.len() > MAX_URL_LEN {
            return 0;
        }
        if let Some(pos) = self.links.iter().position(|u| u == uri) {
            return (pos + 1) as u32;
        }
        if self.links.len() >= MAX_LINKS {
            return 0;
        }
        self.links.push(uri.to_string());
        self.links.len() as u32
    }

    fn put_char(&mut self, c: char) {
        let charset = self.single_shift.take().unwrap_or(self.active_charset);
        let c = self.charsets[charset].map(c);
        let width = c.width().unwrap_or(0);
        let continuation = !c.is_ascii() && self.last_printed.is_some_and(|(row, col)| {
            use unicode_segmentation::UnicodeSegmentation;
            let mut previous = self.grid[row][col].text();
            if previous.len() + c.len_utf8() > MAX_GLYPH_BYTES {
                return false;
            }
            previous.push(c);
            previous.graphemes(true).count() == 1
        });
        if width == 0 || continuation {
            self.extend_glyph(c);
            return;
        }

        if self.pending_wrap {
            self.pending_wrap = false;
            if self.autowrap {
                // Mark the row we are leaving as soft-wrapped BEFORE feeding:
                // `line_feed` may scroll, handing this row to scrollback, and the
                // flag has to travel with it. These two blocks are the only soft
                // wraps in the emulator — a real LF must never set the flag, or
                // reflow would glue unrelated lines together.
                self.mark_wrapped();
                self.cursor_col = 0;
                self.line_feed();
            }
        }

        if width == 2 && self.cols > 1 && self.cursor_col + 1 >= self.cols {
            // A wide glyph cannot straddle the right margin.
            if self.autowrap {
                self.mark_wrapped();
                self.cursor_col = 0;
                self.line_feed();
            } else {
                self.cursor_col = self.cols.saturating_sub(2);
            }
        }

        let row = self.cursor_row;
        let col = self.cursor_col;
        self.clear_wide_artifacts(row, col);

        let pen = self.pen;
        let link = self.current_link;
        self.grid[row][col] = Cell {
            ch: c,
            pen,
            spacer: false,
            link,
            glyph: None,
            image: None,
            right_image: None,
        };

        if width == 2 && col + 1 < self.cols {
            self.clear_wide_artifacts(row, col + 1);
            self.grid[row][col + 1] = Cell {
                ch: ' ',
                pen,
                spacer: true,
                link,
                glyph: None,
                image: None,
                right_image: None,
            };
            self.advance_cursor(2);
        } else {
            self.advance_cursor(1);
        }
        self.last_printed = Some((row, col));
    }

    fn extend_glyph(&mut self, c: char) {
        let Some((row, col)) = self.last_printed else {
            // A leading mark has no base; retain it on a space without moving
            // the cursor. The next spacing character replaces it normally.
            let row = self.cursor_row;
            let col = self.cursor_col;
            if self.grid[row][col].spacer {
                return;
            }
            self.last_printed = Some((row, col));
            self.extend_glyph(c);
            return;
        };
        let old_width = self.grid[row][col].width().min(self.cols);
        let mut text = self.grid[row][col].text();
        if text.len() + c.len_utf8() > MAX_GLYPH_BYTES {
            return;
        }
        text.push(c);
        let new_width = crate::glyph_width(&text).min(self.cols);
        // A late VS16 cannot claim a column outside the grid. Retain the
        // suffix; a later reflow can restore its natural width.
        let actual_width = new_width.min(self.cols - col);
        if actual_width > old_width {
            self.clear_wide_artifacts(row, col + 1);
            let mut spacer = Cell::blank(self.pen.bg);
            spacer.spacer = true;
            self.grid[row][col + 1] = spacer;
        } else if actual_width < old_width && col + 1 < self.cols {
            self.grid[row][col + 1] = Cell::blank(self.pen.bg);
        }
        self.grid[row][col].glyph = Some(Arc::from(text));
        if old_width != actual_width {
            self.cursor_row = row;
            self.cursor_col = col;
            self.pending_wrap = false;
            self.advance_cursor(actual_width);
        }
    }

    /// Blank the dangling half of a wide glyph that a write is about to break.
    fn clear_wide_artifacts(&mut self, row: usize, col: usize) {
        if self.grid[row][col].spacer && col > 0 {
            self.grid[row][col - 1] = Cell::blank(self.pen.bg);
        }
        if col + 1 < self.cols
            && !self.grid[row][col].spacer
            && self.grid[row][col].width() == 2
            && self.grid[row][col + 1].spacer
        {
            self.grid[row][col + 1] = Cell::blank(self.pen.bg);
        }
    }

    fn advance_cursor(&mut self, n: usize) {
        let next = self.cursor_col + n;
        if next >= self.cols {
            self.cursor_col = self.cols - 1;
            self.pending_wrap = true;
        } else {
            self.cursor_col = next;
        }
    }

    // --- C0 controls --------------------------------------------------------

    fn backspace(&mut self) {
        self.cursor_col = self.cursor_col.saturating_sub(1);
        self.pending_wrap = false;
    }

    fn tab(&mut self) {
        self.cursor_col = ((self.cursor_col + 1)..self.cols)
            .find(|&col| self.tabs[col])
            .unwrap_or(self.cols - 1);
        self.pending_wrap = false;
    }

    fn back_tab(&mut self) {
        self.cursor_col = (0..self.cursor_col)
            .rev()
            .find(|&col| self.tabs[col])
            .unwrap_or(0);
        self.pending_wrap = false;
    }

    fn carriage_return(&mut self) {
        self.cursor_col = 0;
        self.pending_wrap = false;
    }

    fn line_feed(&mut self) {
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
        }
        self.pending_wrap = false;
    }

    fn reverse_index(&mut self) {
        if self.cursor_row == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
        }
        self.pending_wrap = false;
    }

    // --- scrolling ----------------------------------------------------------

    fn scroll_up(&mut self, n: usize) {
        for _ in 0..n.min(self.scroll_bottom - self.scroll_top + 1) {
            let removed = self.grid.remove(self.scroll_top);
            if self.scroll_top == 0 && self.alt.is_none() {
                self.push_scrollback(removed);
            }
            self.grid
                .insert(self.scroll_bottom, blank_row(self.cols, self.pen.bg));
        }
        self.damage_scroll_region();
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n.min(self.scroll_bottom - self.scroll_top + 1) {
            self.grid.remove(self.scroll_bottom);
            self.grid
                .insert(self.scroll_top, blank_row(self.cols, self.pen.bg));
        }
        self.damage_scroll_region();
    }

    fn damage_scroll_region(&mut self) {
        for row in &mut self.grid[self.scroll_top..=self.scroll_bottom] {
            row.dirty = true;
        }
    }

    /// Record that the cursor's row continues onto the next one.
    fn mark_wrapped(&mut self) {
        if let Some(row) = self.grid.get_mut(self.cursor_row) {
            row.wrapped = true;
            row.dirty = true;
        }
    }

    /// Rebuild scrollback + screen for a new column count by re-wrapping every
    /// logical line.
    ///
    /// Rows keep whatever width they were written at, so scrollback captured on
    /// a 200-column screen still re-wraps correctly after several resizes — the
    /// history is never clipped down to the narrowest width it has ever seen.
    fn reflow(&mut self, cols: usize, rows: usize) {
        let (lines, cursor) = self.logical_lines();
        let (mut all, (cursor_row, cursor_col)) = rewrap(&lines, cols, cursor);

        // The screen is the tail; everything above becomes scrollback. The
        // cursor is clamped into that window rather than the window moved to
        // chase it: after a re-wrap the cursor is at the end of the output
        // anyway, and following it would scroll away what the user was reading.
        let screen_start = all.len().saturating_sub(rows);
        let mut grid = all.split_off(screen_start);
        while grid.len() < rows {
            grid.push(Row::blank(cols, Color::Default));
        }

        self.grid = grid;
        self.scrollback = VecDeque::from(all);
        // Re-wrapping narrower turns one row into several, so the history can
        // cross the limit even though no new output arrived.
        let limit = scrollback_limit();
        while self.scrollback.len() > limit {
            self.scrollback.pop_front();
        }

        self.cursor_row = cursor_row.saturating_sub(screen_start).min(rows - 1);
        self.cursor_col = cursor_col.min(cols - 1);
    }

    /// Join scrollback + screen into logical lines, and locate the cursor inside
    /// them as `(line index, cell offset)`.
    ///
    /// Wide-glyph spacers are dropped: they are re-derived when the line is
    /// re-wrapped, and carrying them would double-count columns. Trailing blanks
    /// are trimmed from a row that ended in a real newline, but never from a
    /// soft-wrapped one — that row is full by definition, and trimming it would
    /// pull the continuation up against the wrong column.
    fn logical_lines(&self) -> (Vec<Vec<Cell>>, (usize, usize)) {
        let mut lines: Vec<Vec<Cell>> = Vec::new();
        let mut current: Vec<Cell> = Vec::new();

        // The cursor is located by the same walk every other anchor uses. Doing
        // it here by hand instead would let the two drift apart, and then a
        // selection anchored before a reflow would come back on a different line
        // from the cursor that reflow placed.
        let cursor = self.logical_anchor(self.scrollback.len() + self.cursor_row, self.cursor_col);

        for abs in 0..self.total_rows() {
            let row = self.row_at(abs);
            for (col, cell) in row.cells[..logical_keep(row)].iter().enumerate() {
                if cell.spacer {
                    continue;
                }
                let mut cell = cell.clone();
                if row.cells.get(col + 1).is_some_and(|next| next.spacer) {
                    cell.right_image = row.cells[col + 1].image.clone();
                }
                current.push(cell);
            }

            if !row.wrapped {
                lines.push(std::mem::take(&mut current));
            }
        }
        // A trailing soft-wrapped row leaves an unclosed line; an empty terminal
        // still has one (blank) line.
        if !current.is_empty() || lines.is_empty() {
            lines.push(current);
        }
        (lines, (cursor.line, cursor.offset))
    }

    /// Rows in the buffer: scrollback followed by the screen.
    fn total_rows(&self) -> usize {
        self.scrollback.len() + self.grid.len()
    }

    /// The row at an absolute (scrollback + screen) index.
    ///
    /// # Panics
    /// If `abs >= self.total_rows()`.
    fn row_at(&self, abs: usize) -> &Row {
        let split = self.scrollback.len();
        if abs < split {
            &self.scrollback[abs]
        } else {
            &self.grid[abs - split]
        }
    }

    /// Anchor an absolute `(row, col)` to the logical line it falls in.
    ///
    /// The walk must stay identical to [`Self::logical_lines`]' — an anchor is
    /// only worth anything if it names the same line the reflow will rebuild.
    fn logical_anchor(&self, row: usize, col: usize) -> LogicalAnchor {
        let total = self.total_rows();
        let target = row.min(total.saturating_sub(1));
        let mut line = 0usize;
        let mut base = 0usize;

        for abs in 0..total {
            let current = self.row_at(abs);
            if abs == target {
                // Columns strictly before `col`, so an exclusive endpoint (which
                // is how a selection's end column is read) stays exclusive.
                let upto = col.min(current.cells.len());
                let before = current.cells[..upto].iter().filter(|c| !c.spacer).count();
                return LogicalAnchor {
                    line,
                    offset: base + before,
                };
            }
            base += logical_len(current);
            if !current.wrapped {
                line += 1;
                base = 0;
            }
        }

        LogicalAnchor { line, offset: base }
    }

    /// Resolve a [`LogicalAnchor`] back to an absolute `(row, col)` at the
    /// current width.
    ///
    /// Clamps instead of failing. An offset past the end of its line lands just
    /// after the last cell; a line index that no longer exists — re-wrapping
    /// narrower can push the oldest lines past the scrollback limit, which
    /// renumbers everything that survives — lands on the last row.
    fn resolve_anchor(&self, anchor: LogicalAnchor) -> (usize, usize) {
        let total = self.total_rows();
        if total == 0 {
            return (0, 0);
        }
        let mut line = 0usize;
        let mut base = 0usize;

        for abs in 0..total {
            let current = self.row_at(abs);
            let len = logical_len(current);
            if line == anchor.line {
                let wanted = anchor.offset.saturating_sub(base);
                if wanted < len {
                    let col = current
                        .cells
                        .iter()
                        .enumerate()
                        .filter(|(_, cell)| !cell.spacer)
                        .nth(wanted)
                        .map_or(0, |(col, _)| col);
                    return (abs, col);
                }
                if !current.wrapped {
                    // Last row of the line: the anchor sits at or past its end.
                    return (abs, content_len(&current.cells).min(current.cells.len()));
                }
            }
            base += len;
            if !current.wrapped {
                line += 1;
                base = 0;
            }
        }

        (total - 1, 0)
    }

    fn push_scrollback(&mut self, row: Row) {
        while self.scrollback.len() >= scrollback_limit() {
            self.scrollback.pop_front();
            self.force_dirty = true;
        }
        self.scrollback.push_back(row);
    }

    // --- cursor motion ------------------------------------------------------

    fn cursor_up(&mut self, n: usize) {
        let floor = if self.cursor_row >= self.scroll_top {
            self.scroll_top
        } else {
            0
        };
        self.cursor_row = self.cursor_row.saturating_sub(n).max(floor);
        self.pending_wrap = false;
    }

    fn cursor_down(&mut self, n: usize) {
        let ceil = if self.cursor_row <= self.scroll_bottom {
            self.scroll_bottom
        } else {
            self.rows - 1
        };
        self.cursor_row = (self.cursor_row + n).min(ceil);
        self.pending_wrap = false;
    }

    fn cursor_left(&mut self, n: usize) {
        self.cursor_col = self.cursor_col.saturating_sub(n);
        self.pending_wrap = false;
    }

    fn cursor_right(&mut self, n: usize) {
        self.cursor_col = (self.cursor_col + n).min(self.cols - 1);
        self.pending_wrap = false;
    }

    fn move_to(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(self.rows - 1);
        self.cursor_col = col.min(self.cols - 1);
        self.pending_wrap = false;
    }

    fn set_col(&mut self, col: usize) {
        self.cursor_col = col.min(self.cols - 1);
        self.pending_wrap = false;
    }

    fn set_row(&mut self, row: usize) {
        self.cursor_row = row.min(self.rows - 1);
        self.pending_wrap = false;
    }

    // --- erase / insert / delete -------------------------------------------

    fn erase_in_display(&mut self, mode: u16) {
        let bg = self.pen.bg;
        match mode {
            0 => {
                self.erase_line_from_cursor(0);
                for row in (self.cursor_row + 1)..self.rows {
                    self.grid[row] = blank_row(self.cols, bg);
                }
            }
            1 => {
                for row in 0..self.cursor_row {
                    self.grid[row] = blank_row(self.cols, bg);
                }
                self.erase_line_from_cursor(1);
            }
            2 => {
                for row in 0..self.rows {
                    self.grid[row] = blank_row(self.cols, bg);
                }
            }
            3 => {
                for row in 0..self.rows {
                    self.grid[row] = blank_row(self.cols, bg);
                }
                self.scrollback.clear();
                self.force_dirty = true;
            }
            _ => {}
        }
        self.pending_wrap = false;
    }

    fn erase_in_line(&mut self, mode: u16) {
        self.erase_line_from_cursor(mode);
        self.pending_wrap = false;
    }

    fn erase_line_from_cursor(&mut self, mode: u16) {
        let bg = self.pen.bg;
        let row = self.cursor_row;
        let (start, end) = match mode {
            0 => (self.cursor_col, self.cols),
            1 => (0, self.cursor_col + 1),
            2 => (0, self.cols),
            _ => return,
        };
        for col in start..end.min(self.cols) {
            self.clear_wide_artifacts(row, col);
            self.grid[row][col] = Cell::blank(bg);
        }
    }

    fn erase_chars(&mut self, n: usize) {
        let bg = self.pen.bg;
        let row = self.cursor_row;
        let end = (self.cursor_col + n).min(self.cols);
        for col in self.cursor_col..end {
            self.clear_wide_artifacts(row, col);
            self.grid[row][col] = Cell::blank(bg);
        }
        self.pending_wrap = false;
    }

    fn insert_chars(&mut self, n: usize) {
        let bg = self.pen.bg;
        let row = self.cursor_row;
        let col = self.cursor_col;
        let n = n.min(self.cols - col);
        if self.grid[row][col].spacer {
            self.clear_wide_artifacts(row, col);
        }
        self.grid[row].dirty = true;
        let line = &mut self.grid[row].cells;
        for _ in 0..n {
            line.pop();
            line.insert(col, Cell::blank(bg));
        }
        repair_wide(&mut self.grid[row]);
        self.pending_wrap = false;
    }

    fn delete_chars(&mut self, n: usize) {
        let bg = self.pen.bg;
        let row = self.cursor_row;
        let col = self.cursor_col;
        let n = n.min(self.cols - col);
        if self.grid[row][col].spacer {
            self.clear_wide_artifacts(row, col);
        }
        if col + n < self.cols && self.grid[row][col + n].spacer {
            self.clear_wide_artifacts(row, col + n);
        }
        self.grid[row].dirty = true;
        let line = &mut self.grid[row].cells;
        for _ in 0..n {
            line.remove(col);
            line.push(Cell::blank(bg));
        }
        repair_wide(&mut self.grid[row]);
        self.pending_wrap = false;
    }

    fn insert_lines(&mut self, n: usize) {
        if self.cursor_row < self.scroll_top || self.cursor_row > self.scroll_bottom {
            return;
        }
        let bg = self.pen.bg;
        for _ in 0..n.min(self.scroll_bottom - self.cursor_row + 1) {
            self.grid.remove(self.scroll_bottom);
            self.grid.insert(self.cursor_row, blank_row(self.cols, bg));
        }
        self.damage_scroll_region();
        self.pending_wrap = false;
    }

    fn delete_lines(&mut self, n: usize) {
        if self.cursor_row < self.scroll_top || self.cursor_row > self.scroll_bottom {
            return;
        }
        let bg = self.pen.bg;
        for _ in 0..n.min(self.scroll_bottom - self.cursor_row + 1) {
            self.grid.remove(self.cursor_row);
            self.grid
                .insert(self.scroll_bottom, blank_row(self.cols, bg));
        }
        self.damage_scroll_region();
        self.pending_wrap = false;
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let top = top.min(self.rows - 1);
        let bottom = bottom.min(self.rows - 1);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
        }
        self.move_to(0, 0);
    }

    // --- cursor save / restore ---------------------------------------------

    fn save_cursor(&mut self) {
        self.saved_cursor = Some(SavedCursor {
            row: self.cursor_row,
            col: self.cursor_col,
            pen: self.pen,
            pending_wrap: self.pending_wrap,
            charsets: self.charsets,
            active_charset: self.active_charset,
        });
    }

    fn restore_cursor(&mut self) {
        if let Some(saved) = self.saved_cursor {
            self.cursor_row = saved.row.min(self.rows - 1);
            self.cursor_col = saved.col.min(self.cols - 1);
            self.pen = saved.pen;
            self.pending_wrap = saved.pending_wrap;
            self.charsets = saved.charsets;
            self.active_charset = saved.active_charset;
        } else {
            self.move_to(0, 0);
        }
    }

    // --- alternate screen ---------------------------------------------------

    fn enter_alt_screen(&mut self) {
        self.force_dirty = true;
        self.single_shift = None;
        if self.alt.is_some() {
            self.grid = blank_grid(self.cols, self.rows);
            self.move_to(0, 0);
            return;
        }
        let saved = AltSaved {
            grid: std::mem::replace(&mut self.grid, blank_grid(self.cols, self.rows)),
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            pen: self.pen,
            saved_cursor: self.saved_cursor.take(),
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
            autowrap: self.autowrap,
            current_link: self.current_link,
            charsets: self.charsets,
            active_charset: self.active_charset,
        };
        self.alt = Some(saved);
        self.move_to(0, 0);
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        // A hyperlink open on the primary screen must not bleed into alt output.
        self.current_link = 0;
    }

    fn leave_alt_screen(&mut self) {
        if let Some(saved) = self.alt.take() {
            self.force_dirty = true;
            self.single_shift = None;
            self.grid = saved.grid;
            self.cursor_row = saved.cursor_row.min(self.rows - 1);
            self.cursor_col = saved.cursor_col.min(self.cols - 1);
            self.pen = saved.pen;
            self.saved_cursor = saved.saved_cursor;
            self.scroll_top = saved.scroll_top.min(self.rows - 1);
            self.scroll_bottom = saved.scroll_bottom.min(self.rows - 1);
            self.autowrap = saved.autowrap;
            self.pending_wrap = false;
            self.current_link = saved.current_link;
            self.charsets = saved.charsets;
            self.active_charset = saved.active_charset;
        }
    }

    fn full_reset(&mut self) {
        self.grid = blank_grid(self.cols, self.rows);
        self.scrollback.clear();
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.pen = Pen::default();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.pending_wrap = false;
        self.autowrap = true;
        self.cursor_visible = true;
        self.saved_cursor = None;
        self.alt = None;
        self.title = String::from("terminal");
        // RIS closes any open hyperlink and reclaims the interned URLs (safe:
        // the scrollback that referenced them was just cleared).
        self.current_link = 0;
        self.links.clear();
        self.charsets = [Charset::Ascii; 4];
        self.active_charset = 0;
        self.single_shift = None;
        self.tabs = (0..self.cols)
            .map(|col| col > 0 && col % TAB_WIDTH == 0)
            .collect();
        self.dcs = None;
        self.pending_dcs = None;
        self.sixel_scrolling = true;
        self.bracketed_paste = false;
        self.mouse_mode = crate::MouseMode::Off;
        self.mouse_sgr = false;
        self.force_dirty = true;
        self.last_printed = None;
    }

    // --- DEC private / ANSI modes ------------------------------------------

    fn set_dec_mode(&mut self, mode: u16, enable: bool) {
        match mode {
            7 => self.autowrap = enable,
            25 => self.cursor_visible = enable,
            80 => self.sixel_scrolling = enable,
            47 | 1047 => {
                if enable {
                    self.enter_alt_screen();
                } else {
                    self.leave_alt_screen();
                }
            }
            1048 => {
                if enable {
                    self.save_cursor();
                } else {
                    self.restore_cursor();
                }
            }
            1049 => {
                if enable {
                    self.enter_alt_screen();
                } else {
                    self.leave_alt_screen();
                }
            }
            1000 => {
                self.mouse_mode = if enable {
                    crate::MouseMode::Normal
                } else {
                    crate::MouseMode::Off
                }
            }
            1002 => {
                self.mouse_mode = if enable {
                    crate::MouseMode::ButtonEvent
                } else {
                    crate::MouseMode::Off
                }
            }
            1003 => {
                self.mouse_mode = if enable {
                    crate::MouseMode::AnyMotion
                } else {
                    crate::MouseMode::Off
                }
            }
            1006 => self.mouse_sgr = enable,
            2004 => self.bracketed_paste = enable,
            _ => {}
        }
    }

    // --- device reports -----------------------------------------------------

    fn device_status(&mut self, mode: u16) {
        match mode {
            5 => self.responses.extend_from_slice(b"\x1b[0n"),
            6 => {
                let report = format!("\x1b[{};{}R", self.cursor_row + 1, self.cursor_col + 1);
                self.responses.extend_from_slice(report.as_bytes());
            }
            _ => {}
        }
    }

    fn device_attributes(&mut self) {
        // VT220 with ANSI color and Sixel graphics.
        self.responses.extend_from_slice(b"\x1b[?62;4;22c");
    }

    fn finish_dcs(&mut self) {
        match self.pending_dcs.take() {
            Some(Dcs::Sixel {
                bytes,
                transparent,
                aspect,
            }) => {
                if let Some(raster) = crate::sixel::decode(&bytes, aspect) {
                    self.paint_sixel(raster, transparent);
                }
            }
            Some(Dcs::Status(query)) => {
                let status = match query.as_slice() {
                    b"m" => Some(self.sgr_status()),
                    b"r" => Some(format!(
                        "{};{}r",
                        self.scroll_top + 1,
                        self.scroll_bottom + 1
                    )),
                    _ => None,
                };
                if let Some(status) = status {
                    self.responses
                        .extend_from_slice(format!("\x1bP1$r{status}\x1b\\").as_bytes());
                } else {
                    self.responses.extend_from_slice(b"\x1bP0$r\x1b\\");
                }
            }
            None => {}
        }
    }

    fn sgr_status(&self) -> String {
        let mut params = vec!["0".to_string()];
        for (flag, code) in [
            (BOLD, 1),
            (DIM, 2),
            (ITALIC, 3),
            (UNDERLINE, 4),
            (REVERSE, 7),
            (HIDDEN, 8),
            (STRIKE, 9),
        ] {
            if self.pen.flags & flag != 0 {
                params.push(code.to_string());
            }
        }
        for (prefix, color) in [(38, self.pen.fg), (48, self.pen.bg)] {
            match color {
                Color::Default => {}
                Color::Indexed(index) => params.push(format!("{prefix};5;{index}")),
                Color::Rgb(r, g, b) => params.push(format!("{prefix};2;{r};{g};{b}")),
            }
        }
        format!("{}m", params.join(";"))
    }

    fn window_report(&mut self, kind: u16) {
        // Graphics and geometry replies share virtual coordinates; the UI
        // scales the same cell to its font metrics, preserving image size.
        let report = match kind {
            14 => format!(
                "\x1b[4;{};{}t",
                self.rows * crate::SIXEL_CELL_HEIGHT,
                self.cols * crate::SIXEL_CELL_WIDTH
            ),
            16 => format!(
                "\x1b[6;{};{}t",
                crate::SIXEL_CELL_HEIGHT,
                crate::SIXEL_CELL_WIDTH
            ),
            18 => format!("\x1b[8;{};{}t", self.rows, self.cols),
            _ => return,
        };
        self.responses.extend_from_slice(report.as_bytes());
    }

    fn paint_sixel(&mut self, raster: crate::sixel::Raster, transparent: bool) {
        use crate::{SIXEL_CELL_HEIGHT as H, SIXEL_CELL_WIDTH as W};
        let (start_row, start_col) = if self.sixel_scrolling {
            (self.cursor_row, self.cursor_col)
        } else {
            (0, 0)
        };
        let columns = raster.width.div_ceil(W).min(self.cols - start_col);
        let rows = raster.height.div_ceil(H);
        let tile_count = rows * columns;
        if tile_count > MAX_GRAPHIC_TILES {
            return;
        }
        // Bound retained graphics across BOTH screens and history. Evict the
        // oldest tiles first, before allocation; grid cell count alone is not
        // a useful bound on attacker-controlled scrollback graphics.
        let retained: usize = self
            .scrollback
            .iter()
            .chain(self.grid.iter())
            .chain(self.alt.iter().flat_map(|alt| alt.grid.iter()))
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| {
                        usize::from(cell.image.is_some()) + usize::from(cell.right_image.is_some())
                    })
                    .sum::<usize>()
            })
            .sum();
        let mut evict = (retained + tile_count).saturating_sub(MAX_GRAPHIC_TILES);
        if evict > 0 {
            for row in self
                .scrollback
                .iter_mut()
                .chain(self.grid.iter_mut())
                .chain(self.alt.iter_mut().flat_map(|alt| alt.grid.iter_mut()))
            {
                for cell in &mut row.cells {
                    if evict > 0 && cell.image.take().is_some() {
                        evict -= 1;
                        row.dirty = true;
                    }
                    if evict > 0 && cell.right_image.take().is_some() {
                        evict -= 1;
                        row.dirty = true;
                    }
                }
            }
            self.force_dirty = true;
        }
        self.pending_wrap = false;
        for tile_y in 0..rows {
            let row = if self.sixel_scrolling {
                self.cursor_row
            } else {
                start_row + tile_y
            };
            if row >= self.rows {
                break;
            }
            for tile_x in 0..columns {
                let col = start_col + tile_x;
                let mut pixels = if transparent {
                    self.grid[row][col]
                        .image
                        .as_deref()
                        .cloned()
                        .unwrap_or_else(|| vec![0; W * H * 4])
                } else {
                    vec![0; W * H * 4]
                };
                for y in 0..H.min(raster.height - tile_y * H) {
                    for x in 0..W.min(raster.width - tile_x * W) {
                        let src = ((tile_y * H + y) * raster.width + tile_x * W + x) * 4;
                        let dst = (y * W + x) * 4;
                        if raster.pixels[src + 3] != 0 {
                            pixels[dst..dst + 4].copy_from_slice(&raster.pixels[src..src + 4]);
                        }
                    }
                }
                if !transparent {
                    self.clear_wide_artifacts(row, col);
                    self.grid[row][col] = Cell::blank(self.pen.bg);
                }
                self.grid[row][col].image = Some(Arc::new(pixels));
            }
            if self.sixel_scrolling {
                self.line_feed();
            }
        }
        if self.sixel_scrolling {
            self.cursor_col = start_col;
        }
        self.last_printed = None;
    }

    // --- SGR ----------------------------------------------------------------

    fn apply_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.pen = Pen::default();
            return;
        }

        let mut iter = params.iter();
        while let Some(part) = iter.next() {
            if part.len() > 1 {
                self.apply_sgr_colon(part);
                continue;
            }
            let code = part.first().copied().unwrap_or(0);
            match code {
                0 => self.pen = Pen::default(),
                1 => self.pen.flags |= BOLD,
                2 => self.pen.flags |= DIM,
                3 => self.pen.flags |= ITALIC,
                4 => self.pen.flags |= UNDERLINE,
                7 => self.pen.flags |= REVERSE,
                8 => self.pen.flags |= HIDDEN,
                9 => self.pen.flags |= STRIKE,
                21 | 22 => self.pen.flags &= !(BOLD | DIM),
                23 => self.pen.flags &= !ITALIC,
                24 => self.pen.flags &= !UNDERLINE,
                27 => self.pen.flags &= !REVERSE,
                28 => self.pen.flags &= !HIDDEN,
                29 => self.pen.flags &= !STRIKE,
                30..=37 => self.pen.fg = Color::Indexed((code - 30) as u8),
                38 => {
                    if let Some(color) = parse_extended_color(&mut iter) {
                        self.pen.fg = color;
                    }
                }
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed((code - 40) as u8),
                48 => {
                    if let Some(color) = parse_extended_color(&mut iter) {
                        self.pen.bg = color;
                    }
                }
                49 => self.pen.bg = Color::Default,
                90..=97 => self.pen.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.pen.bg = Color::Indexed((code - 100 + 8) as u8),
                _ => {}
            }
        }
    }

    fn apply_sgr_colon(&mut self, part: &[u16]) {
        match part.first().copied().unwrap_or(0) {
            0 => self.pen = Pen::default(),
            1 => self.pen.flags |= BOLD,
            4 => self.pen.flags |= UNDERLINE,
            38 => {
                if let Some(color) = colon_color(part) {
                    self.pen.fg = color;
                }
            }
            48 => {
                if let Some(color) = colon_color(part) {
                    self.pen.bg = color;
                }
            }
            _ => {}
        }
    }

    // --- snapshot -----------------------------------------------------------

    fn snapshot(&self, viewport: Viewport) -> TerminalSnapshot {
        let total = self.scrollback.len() + self.rows;
        let height = viewport.height.max(1);
        let first = if viewport.first_row == usize::MAX {
            total.saturating_sub(height)
        } else {
            viewport.first_row.min(total)
        };
        let end = (first + height).min(total);
        let cursor_abs = self.scrollback.len() + self.cursor_row;

        let mut lines = Vec::with_capacity(end - first);
        for abs in first..end {
            let row = self.row_at(abs);
            let cursor = if self.cursor_visible && abs == cursor_abs {
                Some(self.cursor_col)
            } else {
                None
            };
            lines.push(render_row(row, cursor, &self.links, row.wrapped));
        }

        TerminalSnapshot {
            title: self.title.clone(),
            size: TerminalSize::new(self.cols as u16, self.rows as u16),
            first_row: first,
            total_rows: total,
            lines,
            cursor_row: cursor_abs.saturating_sub(first),
            cursor_col: self.cursor_col,
            cursor_visible: self.cursor_visible,
            // `alt` holds the stashed primary screen while the alternate one is
            // in front, so its presence *is* the flag.
            alt_screen: self.alt.is_some(),
        }
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }

        // The alternate screen never reflows and its rows must never reach
        // scrollback: it belongs to a full-screen app that redraws itself at the
        // new size, and joining its rows would splice together unrelated parts
        // of a TUI. The primary screen STASHED behind it is the opposite case —
        // ordinary shell output that owns this terminal's scrollback — so it
        // takes exactly the same lossless path the live primary does. Running it
        // through `resize_grid` instead is how shrinking the window inside vim
        // used to eat the primary rows underneath.
        if let Some(mut alt) = self.alt.take() {
            let alt_src_start = shrink_start(self.cursor_row, self.rows, rows);
            let alt_cursor = (self.cursor_row, self.cursor_col);

            // Swap the stashed primary in, resize it as a primary screen, swap back.
            let alt_grid = std::mem::replace(&mut self.grid, std::mem::take(&mut alt.grid));
            self.cursor_row = alt.cursor_row;
            self.cursor_col = alt.cursor_col;
            self.resize_primary(cols, rows);
            alt.grid = std::mem::replace(&mut self.grid, alt_grid);
            alt.cursor_row = self.cursor_row;
            alt.cursor_col = self.cursor_col;
            alt.scroll_top = alt.scroll_top.min(rows - 1);
            alt.scroll_bottom = rows - 1;

            self.grid = resize_grid(&self.grid, cols, rows, alt_src_start);
            self.cursor_row = alt_cursor.0.saturating_sub(alt_src_start).min(rows - 1);
            self.cursor_col = alt_cursor.1.min(cols - 1);
            self.alt = Some(alt);
        } else {
            self.resize_primary(cols, rows);
        }

        self.cols = cols;
        self.rows = rows;
        let old_tabs = self.tabs.len();
        self.tabs.resize(cols, false);
        for (col, stop) in self.tabs.iter_mut().enumerate().skip(old_tabs) {
            *stop = col > 0 && col % TAB_WIDTH == 0;
        }
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.pending_wrap = false;
        self.last_printed = None;
        self.force_dirty = true;
    }

    /// Resize the primary screen — `self.grid` plus `self.scrollback` — without
    /// losing content: a width change re-wraps every logical line, and a height
    /// shrink moves the rows it pushes off the top into scrollback.
    ///
    /// Reads `self.cols`/`self.rows` as the OLD size and deliberately does not
    /// update them: the alternate-screen path above runs this over the stashed
    /// primary grid and still has its own screen to resize at the old size after.
    fn resize_primary(&mut self, cols: usize, rows: usize) {
        // A width change re-wraps every logical line, so the history has to be
        // rebuilt. A height-only change wraps nothing, so it takes the cheaper
        // path that just moves rows between screen and scrollback.
        if cols != self.cols {
            self.reflow(cols, rows);
            return;
        }

        let src_start = shrink_start(self.cursor_row, self.rows, rows);
        // Rows a shrink pushes off the top are real output — move them into
        // scrollback instead of dropping them, which is what this did before and
        // is how narrowing the window silently ate history. Collect before
        // pushing: `drain` borrows the grid, `push_scrollback` borrows all of
        // `self`. Carried rows keep their old width; `resize_grid` would clip
        // them to the new one, losing the very columns a reflow needs later.
        let carried: Vec<Row> = self.grid.drain(..src_start.min(self.grid.len())).collect();
        for row in carried {
            self.push_scrollback(row);
        }
        // The carried rows are gone from the front, so the copy starts at 0.
        self.grid = resize_grid(&self.grid, cols, rows, 0);
        self.cursor_row = self.cursor_row.saturating_sub(src_start).min(rows - 1);
        self.cursor_col = self.cursor_col.min(cols - 1);
    }
}

impl Perform for TermState {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        self.last_printed = None;
        match byte {
            0x07 => self.bell = true,
            0x08 => self.backspace(),
            0x09 => self.tab(),
            0x0A..=0x0C => self.line_feed(),
            0x0D => self.carriage_return(),
            0x0E => self.active_charset = 1,
            0x0F => self.active_charset = 0,
            0x18 | 0x1A => {
                self.dcs = None;
                self.pending_dcs = None;
            }
            _ => {}
        }
    }

    fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.last_printed = None;
        self.dcs = if ignore {
            None
        } else if intermediates.is_empty() && action == 'q' {
            Some(Dcs::Sixel {
                bytes: Vec::new(),
                transparent: params.iter().nth(1).and_then(|p| p.first()).copied() == Some(1),
                aspect: params
                    .iter()
                    .next()
                    .and_then(|p| p.first())
                    .copied()
                    .unwrap_or(0),
            })
        } else if intermediates == b"$" && action == 'q' {
            Some(Dcs::Status(Vec::new()))
        } else {
            None
        };
    }

    fn put(&mut self, byte: u8) {
        let (bytes, limit) = match &mut self.dcs {
            Some(Dcs::Sixel { bytes, .. }) => (bytes, crate::sixel::MAX_BYTES),
            Some(Dcs::Status(bytes)) => (bytes, 128),
            None => return,
        };
        if bytes.len() < limit {
            bytes.push(byte);
        } else {
            self.dcs = None;
        }
    }

    fn unhook(&mut self) {
        // vte calls unhook for ESC, ST AND CAN/SUB. Publication is delayed
        // until feed sees the terminator, so cancelled images never flash.
        self.pending_dcs = self.dcs.take();
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if action != 'm' {
            self.last_printed = None;
        }
        if ignore || (!intermediates.is_empty() && intermediates != b"?") {
            return;
        }
        let private = intermediates.first() == Some(&b'?');
        if private && !matches!(action, 'h' | 'l') {
            return;
        }
        let ps: Vec<u16> = params
            .iter()
            .map(|p| p.first().copied().unwrap_or(0))
            .collect();
        let count = |idx: usize| -> usize {
            match ps.get(idx) {
                Some(&0) | None => 1,
                Some(&v) => v as usize,
            }
        };
        let raw = |idx: usize| -> u16 { ps.get(idx).copied().unwrap_or(0) };

        match action {
            'A' => self.cursor_up(count(0)),
            'B' | 'e' => self.cursor_down(count(0)),
            'C' | 'a' => self.cursor_right(count(0)),
            'D' => self.cursor_left(count(0)),
            'I' if !private => {
                for _ in 0..count(0).min(self.cols) {
                    self.tab();
                }
            }
            'Z' if !private => {
                for _ in 0..count(0).min(self.cols) {
                    self.back_tab();
                }
            }
            'g' if !private => match raw(0) {
                0 => self.tabs[self.cursor_col] = false,
                3 => self.tabs.fill(false),
                _ => {}
            },
            'E' => {
                self.carriage_return();
                self.cursor_down(count(0));
            }
            'F' => {
                self.carriage_return();
                self.cursor_up(count(0));
            }
            'G' | '`' => self.set_col(count(0) - 1),
            'd' => self.set_row(count(0) - 1),
            'H' | 'f' => self.move_to(count(0) - 1, count(1) - 1),
            'J' => self.erase_in_display(raw(0)),
            'K' => self.erase_in_line(raw(0)),
            'L' => self.insert_lines(count(0)),
            'M' => self.delete_lines(count(0)),
            'P' => self.delete_chars(count(0)),
            'X' => self.erase_chars(count(0)),
            '@' => self.insert_chars(count(0)),
            'S' => self.scroll_up(count(0)),
            'T' => self.scroll_down(count(0)),
            'r' => {
                let bottom = if raw(1) == 0 {
                    self.rows
                } else {
                    raw(1) as usize
                };
                self.set_scroll_region(count(0) - 1, bottom - 1);
            }
            'm' => self.apply_sgr(params),
            'h' if private => {
                for mode in &ps {
                    self.set_dec_mode(*mode, true);
                }
            }
            'l' if private => {
                for mode in &ps {
                    self.set_dec_mode(*mode, false);
                }
            }
            'n' if !private => self.device_status(raw(0)),
            'c' if !private => self.device_attributes(),
            't' if !private => self.window_report(raw(0)),
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.last_printed = None;
        if ignore {
            return;
        }
        if let [slot @ (b'(' | b')' | b'*' | b'+')] = intermediates {
            let charset = match byte {
                b'B' => Charset::Ascii,
                b'A' => Charset::Uk,
                b'0' => Charset::DecSpecial,
                _ => return,
            };
            let index = match slot {
                b'(' => 0,
                b')' => 1,
                b'*' => 2,
                _ => 3,
            };
            self.charsets[index] = charset;
            return;
        }
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'M' => self.reverse_index(),
            b'D' => self.line_feed(),
            b'H' => self.tabs[self.cursor_col] = true,
            b'N' => self.single_shift = Some(2),
            b'O' => self.single_shift = Some(3),
            b'n' => self.active_charset = 2,
            b'o' => self.active_charset = 3,
            b'E' => {
                self.carriage_return();
                self.line_feed();
            }
            b'c' => self.full_reset(),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(kind) = params.first() else {
            return;
        };
        if (kind == b"0" || kind == b"2") && params.len() > 1 {
            self.title = String::from_utf8_lossy(params[1]).into_owned();
        } else if kind == b"8" {
            // OSC 8 ; params ; URI  — `params` (id=…) is ignored; the URI may itself
            // contain ';', so rejoin fields 2.. before interning. An empty URI (or
            // a missing one) closes the current hyperlink.
            let uri = if params.len() > 2 {
                params[2..]
                    .iter()
                    .map(|p| String::from_utf8_lossy(p))
                    .collect::<Vec<_>>()
                    .join(";")
            } else {
                String::new()
            };
            self.current_link = self.intern_link(&uri);
        }
    }
}

/// Adit's VT terminal: a [`vte`] parser plus an owned grid.
pub struct VtTerminal {
    parser: Parser,
    state: TermState,
}

impl VtTerminal {
    #[must_use]
    pub fn new(size: TerminalSize) -> Self {
        Self::with_title(size, "terminal")
    }

    #[must_use]
    pub fn with_title(size: TerminalSize, title: impl Into<String>) -> Self {
        let cols = size.cols.max(1) as usize;
        let rows = size.rows.max(1) as usize;
        Self {
            parser: Parser::new(),
            state: TermState::new(cols, rows, title.into()),
        }
    }

    /// Feed UTF-8 text (convenience over [`TerminalCore::feed`]).
    pub fn feed_str(&mut self, text: &str) {
        let _ = self.feed(text.as_bytes());
    }

    /// Append an Adit status annotation on its own line without disturbing a
    /// partially written line.
    pub fn append_status(&mut self, message: impl AsRef<str>) {
        let prefix = if self.state.cursor_col == 0 {
            ""
        } else {
            "\r\n"
        };
        let line = format!("{prefix}\x1b[90m[status]\x1b[0m {}\r\n", message.as_ref());
        self.feed_str(&line);
    }

    /// Clear the visible screen and scrollback and home the cursor.
    pub fn clear(&mut self) {
        self.feed_str("\x1b[3J\x1b[2J\x1b[H");
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.state.title = title.into();
    }

    /// Drain bytes the terminal wants written back to the PTY (DSR/DA replies).
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.state.responses)
    }

    /// Consume a pending bell, returning whether one occurred since last call.
    pub fn take_bell(&mut self) -> bool {
        std::mem::replace(&mut self.state.bell, false)
    }

    /// Whether the app has enabled bracketed paste (DEC private mode 2004), so
    /// the UI should wrap pasted text in `ESC[200~` … `ESC[201~`.
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.state.bracketed_paste
    }

    /// The active mouse-reporting mode (DEC 1000/1002/1003).
    #[must_use]
    pub fn mouse_mode(&self) -> crate::MouseMode {
        self.state.mouse_mode
    }

    /// Whether SGR mouse encoding (DEC 1006) is enabled.
    #[must_use]
    pub fn mouse_sgr(&self) -> bool {
        self.state.mouse_sgr
    }

    /// Anchor an absolute `(row, col)` to the logical line it falls in, so it
    /// can be found again after a resize re-wraps the buffer. See
    /// [`LogicalAnchor`].
    #[must_use]
    pub fn logical_anchor(&self, row: usize, col: usize) -> LogicalAnchor {
        self.state.logical_anchor(row, col)
    }

    /// Resolve a [`LogicalAnchor`] back to an absolute `(row, col)` at the
    /// current width.
    #[must_use]
    pub fn resolve_anchor(&self, anchor: LogicalAnchor) -> (usize, usize) {
        self.state.resolve_anchor(anchor)
    }
}

impl TerminalCore for VtTerminal {
    fn resize(&mut self, size: TerminalSize) -> TerminalChangeSet {
        if usize::from(size.cols.max(1)) == self.state.cols
            && usize::from(size.rows.max(1)) == self.state.rows
        {
            return TerminalChangeSet {
                dirty_rows: Vec::new(),
            };
        }
        self.state
            .resize(size.cols.max(1) as usize, size.rows.max(1) as usize);
        TerminalChangeSet::all(self.state.rows as u16)
    }

    fn feed(&mut self, bytes: &[u8]) -> TerminalChangeSet {
        let before = (
            self.state.cursor_row,
            self.state.cursor_col,
            self.state.cursor_visible,
        );
        let history = self.state.scrollback.len();
        self.state.force_dirty = false;
        for row in &mut self.state.grid {
            row.dirty = false;
        }
        // A DCS can end halfway through a network packet or ESC/ST can span
        // packets. Inspect the terminating bytes rather than trusting unhook,
        // which also fires for cancellation and arbitrary ESC sequences.
        let mut offset = 0;
        while offset < bytes.len() {
            let byte = bytes[offset];
            if self.state.pending_dcs.is_some() {
                if byte == b'\\' {
                    self.state.finish_dcs();
                } else {
                    self.state.pending_dcs = None;
                }
            }
            let end = bytes[offset..]
                .iter()
                .position(|byte| matches!(byte, 0x1b | 0x9c | 0x18 | 0x1a))
                .map_or(bytes.len(), |n| offset + n + 1);
            self.parser.advance(&mut self.state, &bytes[offset..end]);
            if bytes[end - 1] == 0x9c && self.state.pending_dcs.is_some() {
                self.state.finish_dcs();
            }
            offset = end;
        }
        if self.state.force_dirty || history != self.state.scrollback.len() {
            return TerminalChangeSet::all(self.state.rows as u16);
        }
        let after = (
            self.state.cursor_row,
            self.state.cursor_col,
            self.state.cursor_visible,
        );
        let dirty_rows = self
            .state
            .grid
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                (row.dirty
                    || (before != after
                        && ((before.2 && index == before.0) || (after.2 && index == after.0))))
                    .then_some(index)
            })
            .collect();
        TerminalChangeSet { dirty_rows }
    }

    fn snapshot(&self, viewport: Viewport) -> TerminalSnapshot {
        self.state.snapshot(viewport)
    }
}

// --- free helpers -----------------------------------------------------------

/// Length of `cells` ignoring trailing blanks. A blank carrying a background
/// colour counts as content, so a coloured run is not silently trimmed away.
fn content_len(cells: &[Cell]) -> usize {
    cells
        .iter()
        .rposition(|cell| !is_default_blank(cell))
        .map_or(0, |index| index + 1)
}

/// How much of a row belongs to its logical line. Trailing blanks are trimmed
/// from a row that ended in a real newline, but never from a soft-wrapped one —
/// that row is full by definition, and trimming it would pull the continuation
/// up against the wrong column.
fn logical_keep(row: &Row) -> usize {
    if row.wrapped {
        row.cells.len()
    } else {
        content_len(&row.cells)
    }
}

/// How many cells a row contributes to its logical line. Wide-glyph spacers are
/// dropped: they are re-derived when the line is re-wrapped, and carrying them
/// would double-count columns.
fn logical_len(row: &Row) -> usize {
    row.cells[..logical_keep(row)]
        .iter()
        .filter(|cell| !cell.spacer)
        .count()
}

/// Re-wrap logical lines to `cols`, returning the rows and the cursor's new
/// `(row, col)` within them.
fn rewrap(lines: &[Vec<Cell>], cols: usize, cursor: (usize, usize)) -> (Vec<Row>, (usize, usize)) {
    let mut rows: Vec<Row> = Vec::new();
    let mut cursor_out = (0usize, 0usize);

    for (index, line) in lines.iter().enumerate() {
        let mut row = Row::blank(cols, Color::Default);
        let mut col = 0usize;

        for (offset, cell) in line.iter().enumerate() {
            // A wide glyph claims two columns — except on a one-column screen,
            // where no pair can fit; draw it narrow rather than panicking on the
            // spacer write or dropping the character outright.
            let width = if cell.width() == 2 && cols >= 2 { 2 } else { 1 };
            if col + width > cols {
                row.wrapped = true;
                rows.push(std::mem::replace(
                    &mut row,
                    Row::blank(cols, Color::Default),
                ));
                col = 0;
            }
            if index == cursor.0 && offset == cursor.1 {
                cursor_out = (rows.len(), col);
            }
            row.cells[col] = cell.clone();
            if width == 2 {
                row.cells[col].right_image = None;
                let mut spacer = cell.clone();
                spacer.ch = ' ';
                spacer.spacer = true;
                spacer.glyph = None;
                spacer.image = cell.right_image.clone();
                spacer.right_image = None;
                row.cells[col + 1] = spacer;
            }
            col += width;
        }

        // The cursor legitimately sits one past the last cell, at end of line.
        if index == cursor.0 && cursor.1 >= line.len() {
            cursor_out = (rows.len(), col.min(cols.saturating_sub(1)));
        }
        rows.push(row);
    }
    (rows, cursor_out)
}

/// The first row a height change keeps. Shrinking keeps the window of `rows`
/// rows ending at (and including) the cursor, so the prompt and recent output
/// stay on screen; growing keeps content anchored at the top (`0`) and pads
/// blank rows at the bottom.
fn shrink_start(cursor_row: usize, old_rows: usize, rows: usize) -> usize {
    if rows >= old_rows {
        0
    } else {
        (cursor_row + 1)
            .min(old_rows)
            .saturating_sub(rows)
            .min(old_rows - rows)
    }
}

fn blank_row(cols: usize, bg: Color) -> Row {
    Row::blank(cols, bg)
}

fn blank_grid(cols: usize, rows: usize) -> Vec<Row> {
    vec![blank_row(cols, Color::Default); rows]
}

/// Copy `min(old_rows, rows)` rows from `old` (starting at `src_start`) into a
/// fresh `cols`x`rows` grid anchored at the top, clamping each row to `cols`.
fn resize_grid(old: &[Row], cols: usize, rows: usize, src_start: usize) -> Vec<Row> {
    let copy = old.len().min(rows);
    let mut grid = blank_grid(cols, rows);
    for i in 0..copy {
        let src = &old[src_start + i];
        let dst = &mut grid[i];
        let width = cols.min(src.len());
        dst.cells[..width].clone_from_slice(&src.cells[..width]);
        // A row clipped at the new width no longer continues anywhere, so the
        // soft-wrap flag would be a lie. Only a row that survived intact keeps it.
        dst.wrapped = src.wrapped && width == src.len();
        repair_wide(dst);
    }
    grid
}

/// Insert/delete and a clipped alternate-screen resize can bisect a wide
/// glyph. Never leave an orphan spacer or a glyph owning an unrelated column.
fn repair_wide(row: &mut Row) {
    for col in 0..row.len() {
        let invalid = if row[col].spacer {
            col == 0 || row[col - 1].spacer || row[col - 1].width() != 2
        } else {
            row[col].width() == 2 && row.len() > 1 && (col + 1 == row.len() || !row[col + 1].spacer)
        };
        if invalid {
            row.cells[col] = Cell::blank(row[col].pen.bg);
        }
    }
}

/// Parse the `;`-separated tail of an SGR 38/48 extended-color sequence.
fn parse_extended_color(iter: &mut vte::ParamsIter<'_>) -> Option<Color> {
    let kind = iter.next()?.first().copied()?;
    match kind {
        5 => Some(Color::Indexed(iter.next()?.first().copied()? as u8)),
        2 => {
            let r = iter.next()?.first().copied()? as u8;
            let g = iter.next()?.first().copied()? as u8;
            let b = iter.next()?.first().copied()? as u8;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

/// Parse a single `:`-separated SGR color subparameter (e.g. `38:2::r:g:b`).
fn colon_color(part: &[u16]) -> Option<Color> {
    match part.get(1).copied()? {
        5 => part.get(2).map(|i| Color::Indexed(*i as u8)),
        2 => {
            // Either `38:2:r:g:b` (len 5) or `38:2:cs:r:g:b` (len 6).
            let off = if part.len() >= 6 { 3 } else { 2 };
            Some(Color::Rgb(
                *part.get(off)? as u8,
                *part.get(off + 1)? as u8,
                *part.get(off + 2)? as u8,
            ))
        }
        _ => None,
    }
}

fn is_default_blank(cell: &Cell) -> bool {
    !cell.spacer
        && cell.ch == ' '
        && cell.glyph.is_none()
        && cell.image.is_none()
        && cell.right_image.is_none()
        && cell.pen.bg == Color::Default
        && cell.pen.flags & (UNDERLINE | REVERSE) == 0
}

/// Render-ready glyph attributes (beyond fg/bg).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct RunAttrs {
    bold: bool,
    underline: bool,
    italic: bool,
    dim: bool,
    strike: bool,
}

/// Collapse a cell's pen into render-ready attributes: bold brightens the dim
/// ANSI colors, reverse swaps fg/bg, and hidden paints fg with bg.
fn resolve(cell: &Cell) -> (Color, Color, RunAttrs) {
    let pen = cell.pen;
    let bold = pen.flags & BOLD != 0;
    let attrs = RunAttrs {
        bold,
        underline: pen.flags & UNDERLINE != 0,
        italic: pen.flags & ITALIC != 0,
        dim: pen.flags & DIM != 0,
        strike: pen.flags & STRIKE != 0,
    };
    let reverse = pen.flags & REVERSE != 0;
    let hidden = pen.flags & HIDDEN != 0;

    let mut fg = pen.fg;
    let mut bg = pen.bg;
    if bold {
        if let Color::Indexed(i) = fg {
            if i < 8 {
                fg = Color::Indexed(i + 8);
            }
        }
    }
    if reverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    if hidden {
        fg = bg;
    }
    (fg, bg, attrs)
}

/// The cell under the text cursor, as its own run.
///
/// It keeps the cell's real colours and attributes and is only *flagged*; the
/// renderer paints the cursor. Baking a fixed light-on-dark pair in here (what
/// this used to do) hardcoded the cursor's colours against the theme and left the
/// renderer no way to hide it — so it could never blink or dim on focus loss.
fn cursor_cell(
    text: String,
    fg: Color,
    bg: Color,
    attrs: RunAttrs,
    hyperlink: Option<String>,
) -> TerminalCell {
    TerminalCell {
        cursor: true,
        ..run_cell(text, fg, bg, attrs, hyperlink)
    }
}

fn run_cell(
    text: String,
    fg: Color,
    bg: Color,
    attrs: RunAttrs,
    hyperlink: Option<String>,
) -> TerminalCell {
    TerminalCell {
        text,
        fg,
        bg,
        bold: attrs.bold,
        underline: attrs.underline,
        italic: attrs.italic,
        dim: attrs.dim,
        strike: attrs.strike,
        hyperlink,
        cursor: false,
    }
}

/// Convert one grid row into a coalesced [`TerminalLine`], trimming trailing
/// default blanks and rendering the cursor (if present) as its own run.
fn render_row(
    cells: &[Cell],
    cursor_col: Option<usize>,
    links: &[String],
    wrapped: bool,
) -> TerminalLine {
    let cursor_col = cursor_col.map(|col| {
        if col > 0 && cells.get(col).is_some_and(|cell| cell.spacer) {
            col - 1
        } else {
            col
        }
    });
    let images = cells
        .iter()
        .enumerate()
        .filter_map(|(col, cell)| {
            cell.image.as_deref().map(|rgba| crate::TerminalImage {
                col,
                rgba: rgba.clone(),
            })
        })
        .collect();
    // A soft-wrapped row keeps its trailing blanks. It is full by definition, so
    // those blanks are text: `docker ps` pads its columns with spaces, and a row
    // that wraps just after such padding would otherwise be trimmed and pull the
    // continuation up against the wrong column when the two are rejoined for a
    // copy. Same reasoning as `line_content_len`, which reflow already relies on.
    let mut last_meaningful: Option<usize> = if wrapped {
        cells.len().checked_sub(1)
    } else {
        None
    };
    for (i, cell) in cells.iter().enumerate() {
        if !is_default_blank(cell) {
            last_meaningful = Some(last_meaningful.map_or(i, |last| last.max(i)));
        }
    }

    let limit = match (last_meaningful, cursor_col) {
        (Some(l), Some(c)) => l.max(c),
        (Some(l), None) => l,
        (None, Some(c)) => c,
        (None, None) => {
            return TerminalLine {
                cells: Vec::new(),
                images,
                wrapped,
            };
        }
    };

    // Resolve a cell's `link` id to its interned URL (id 0 / out of range ⇒ none).
    let link_url =
        |id: u32| -> Option<String> { (id > 0).then(|| links.get((id - 1) as usize).cloned())? };

    let default_cell = Cell::blank(Color::Default);
    let mut out: Vec<TerminalCell> = Vec::new();
    // Run key: (fg, bg, attrs, link id) — a link boundary starts a new run.
    let mut run: Option<(String, Color, Color, RunAttrs, u32)> = None;

    for col in 0..=limit {
        let cell = cells.get(col).unwrap_or(&default_cell);
        if cell.spacer {
            continue;
        }
        let mut encoded = [0; 4];
        let glyph = match cell.glyph.as_deref() {
            Some(text) => text,
            None => cell.ch.encode_utf8(&mut encoded),
        };

        if cursor_col == Some(col) {
            if let Some((text, fg, bg, attrs, id)) = run.take() {
                out.push(run_cell(text, fg, bg, attrs, link_url(id)));
            }
            // Keep the cell's own colours; the renderer decides how to mark it.
            let (fg, bg, attrs) = resolve(cell);
            out.push(cursor_cell(
                glyph.to_owned(),
                fg,
                bg,
                attrs,
                link_url(cell.link),
            ));
            continue;
        }

        let (fg, bg, attrs) = resolve(cell);
        let id = cell.link;
        match &mut run {
            Some((text, rfg, rbg, rattrs, rid))
                if *rfg == fg && *rbg == bg && *rattrs == attrs && *rid == id =>
            {
                text.push_str(glyph);
            }
            _ => {
                if let Some((text, pfg, pbg, pattrs, pid)) = run.take() {
                    out.push(run_cell(text, pfg, pbg, pattrs, link_url(pid)));
                }
                run = Some((glyph.to_owned(), fg, bg, attrs, id));
            }
        }
    }

    if let Some((text, fg, bg, attrs, id)) = run.take() {
        out.push(run_cell(text, fg, bg, attrs, link_url(id)));
    }

    TerminalLine {
        cells: out,
        images,
        wrapped,
    }
}

#[cfg(test)]
#[path = "compatibility_tests.rs"]
mod compatibility_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(snapshot: &TerminalSnapshot, row: usize) -> String {
        snapshot
            .lines
            .get(row)
            .map(|line| line.cells.iter().map(|c| c.text.as_str()).collect())
            .unwrap_or_default()
    }

    fn term(cols: u16, rows: u16) -> VtTerminal {
        VtTerminal::new(TerminalSize::new(cols, rows))
    }

    #[test]
    fn writes_plain_text() {
        let mut t = term(20, 4);
        t.feed_str("hello");
        let snap = t.snapshot(Viewport::tail(4));
        assert_eq!(line_text(&snap, 0).trim_end(), "hello");
    }

    #[test]
    fn tracks_bracketed_paste_mode() {
        let mut t = term(20, 4);
        assert!(!t.bracketed_paste());
        t.feed_str("\x1b[?2004h");
        assert!(t.bracketed_paste());
        t.feed_str("\x1b[?2004l");
        assert!(!t.bracketed_paste());
    }

    #[test]
    fn italic_and_dim_reach_cells() {
        let mut t = term(20, 2);
        t.feed_str("\x1b[3mI\x1b[0m\x1b[2mD\x1b[0m");
        let snap = t.snapshot(Viewport::tail(2));
        let cells = &snap.lines[0].cells;
        assert!(cells.iter().find(|c| c.text.contains('I')).unwrap().italic);
        assert!(cells.iter().find(|c| c.text.contains('D')).unwrap().dim);
    }

    /// The hyperlink attached to the run whose text contains `needle`.
    fn link_of(snapshot: &TerminalSnapshot, row: usize, needle: &str) -> Option<String> {
        snapshot.lines[row]
            .cells
            .iter()
            .find(|c| c.text.contains(needle))
            .and_then(|c| c.hyperlink.clone())
    }

    #[test]
    fn osc8_hyperlink_reaches_cells_and_closes() {
        let mut t = term(40, 2);
        // OSC 8 open (BEL-terminated) … link text … OSC 8 close … plain text.
        t.feed_str("\x1b]8;;https://example.com\x07link\x1b]8;;\x07after");
        let snap = t.snapshot(Viewport::tail(2));
        assert_eq!(
            link_of(&snap, 0, "link").as_deref(),
            Some("https://example.com")
        );
        // Text after the empty-URI close carries no link.
        assert_eq!(link_of(&snap, 0, "after"), None);
    }

    #[test]
    fn osc8_link_survives_sgr_reset() {
        let mut t = term(40, 2);
        // A colour change and an SGR reset happen *inside* the hyperlink; both the
        // coloured and the reset text must stay linked (the link is off the pen).
        t.feed_str("\x1b]8;;https://x/y\x07\x1b[31mred\x1b[0mplain\x1b]8;;\x07");
        let snap = t.snapshot(Viewport::tail(2));
        assert_eq!(link_of(&snap, 0, "red").as_deref(), Some("https://x/y"));
        assert_eq!(link_of(&snap, 0, "plain").as_deref(), Some("https://x/y"));
    }

    #[test]
    fn osc8_uri_with_semicolon_and_id_params() {
        let mut t = term(40, 2);
        // id= params are ignored; a ';' inside the URI is preserved by rejoining.
        t.feed_str("\x1b]8;id=42;https://x/a;b\x07L\x1b]8;;\x07");
        let snap = t.snapshot(Viewport::tail(2));
        assert_eq!(link_of(&snap, 0, "L").as_deref(), Some("https://x/a;b"));
    }

    #[test]
    fn osc8_link_closed_by_full_reset() {
        let mut t = term(40, 2);
        // Open a link, then RIS (ESC c) without closing it: later text must NOT
        // inherit the link.
        t.feed_str("\x1b]8;;https://example.com\x07\x1bcplain");
        let snap = t.snapshot(Viewport::tail(2));
        assert_eq!(link_of(&snap, 0, "plain"), None);
    }

    #[test]
    fn osc8_link_does_not_bleed_across_alt_screen() {
        let mut t = term(40, 3);
        // Open a link on the primary screen and leave it open, then switch to the
        // alt screen: alt output must NOT inherit the link.
        t.feed_str("\x1b]8;;https://primary\x07\x1b[?1049hALT");
        let alt = t.snapshot(Viewport::tail(3));
        assert_eq!(link_of(&alt, 0, "ALT"), None, "link bled into alt screen");
        // Returning to the primary screen restores its still-open link state.
        t.feed_str("\x1b[?1049lBACK");
        let back = t.snapshot(Viewport::tail(3));
        assert_eq!(
            link_of(&back, 0, "BACK").as_deref(),
            Some("https://primary")
        );
    }

    #[test]
    fn tracks_mouse_reporting_modes() {
        use crate::MouseMode;
        let mut t = term(20, 4);
        assert_eq!(t.mouse_mode(), MouseMode::Off);
        assert!(!t.mouse_sgr());
        t.feed_str("\x1b[?1000h");
        assert_eq!(t.mouse_mode(), MouseMode::Normal);
        t.feed_str("\x1b[?1002h");
        assert_eq!(t.mouse_mode(), MouseMode::ButtonEvent);
        t.feed_str("\x1b[?1003h");
        assert_eq!(t.mouse_mode(), MouseMode::AnyMotion);
        t.feed_str("\x1b[?1006h");
        assert!(t.mouse_sgr());
        t.feed_str("\x1b[?1003l");
        assert_eq!(t.mouse_mode(), MouseMode::Off);
    }

    #[test]
    fn carriage_return_overwrites() {
        let mut t = term(20, 4);
        t.feed_str("hello\rby");
        let snap = t.snapshot(Viewport::tail(4));
        assert_eq!(line_text(&snap, 0).trim_end(), "byllo");
    }

    #[test]
    fn newline_advances_row() {
        let mut t = term(20, 4);
        t.feed_str("a\r\nb");
        let snap = t.snapshot(Viewport::tail(4));
        assert_eq!(line_text(&snap, 0).trim_end(), "a");
        assert_eq!(line_text(&snap, 1).trim_end(), "b");
    }

    #[test]
    fn autowrap_to_next_line() {
        let mut t = term(4, 4);
        t.feed_str("abcdef");
        let snap = t.snapshot(Viewport::tail(4));
        assert_eq!(line_text(&snap, 0).trim_end(), "abcd");
        assert_eq!(line_text(&snap, 1).trim_end(), "ef");
    }

    #[test]
    fn scroll_pushes_to_scrollback() {
        let mut t = term(10, 2);
        t.feed_str("one\r\ntwo\r\nthree");
        // Two-row screen now shows "two"/"three"; "one" went to scrollback.
        let visible = t.snapshot(Viewport::tail(2));
        assert_eq!(line_text(&visible, 0).trim_end(), "two");
        assert_eq!(line_text(&visible, 1).trim_end(), "three");
        // A taller viewport reveals the scrolled-off line.
        let full = t.snapshot(Viewport::tail(3));
        assert_eq!(line_text(&full, 0).trim_end(), "one");
    }

    #[test]
    fn shrinking_rows_carries_the_top_into_scrollback() {
        let mut t = term(10, 4);
        t.feed_str("one\r\ntwo\r\nthree\r\nfour");
        t.resize(TerminalSize::new(10, 2));

        // The window ending at the cursor stays on screen...
        let visible = t.snapshot(Viewport::tail(2));
        assert_eq!(line_text(&visible, 0).trim_end(), "three");
        assert_eq!(line_text(&visible, 1).trim_end(), "four");

        // ...and the rows it displaced are in scrollback, not discarded.
        let full = t.snapshot(Viewport::tail(4));
        assert_eq!(line_text(&full, 0).trim_end(), "one");
        assert_eq!(line_text(&full, 1).trim_end(), "two");
    }

    #[test]
    fn shrinking_the_alternate_screen_does_not_touch_scrollback() {
        let mut t = term(10, 4);
        t.feed_str("kept\r\n");
        // Enter the alternate screen (1049) and fill it.
        t.feed_str("\x1b[?1049h");
        t.feed_str("alt1\r\nalter2\r\naltr3\r\naltr4");
        t.resize(TerminalSize::new(10, 2));

        // A full-screen app redraws itself, so its rows must never become
        // history — scrollback still holds only what the primary screen wrote.
        t.feed_str("\x1b[?1049l");
        let full = t.snapshot(Viewport::tail(4));
        let text = (0..4)
            .map(|row| line_text(&full, row))
            .collect::<Vec<_>>()
            .join("|");
        assert!(!text.contains("alt"), "alt-screen rows leaked: {text}");
    }

    /// Every row of a snapshot, trailing padding removed, concatenated — so a
    /// re-wrapped line reads back as the text that produced it.
    fn joined(snap: &TerminalSnapshot) -> String {
        (0..snap.lines.len())
            .map(|row| line_text(snap, row).trim_end().to_string())
            .collect()
    }

    #[test]
    fn narrowing_rewraps_instead_of_truncating() {
        let mut t = term(10, 4);
        t.feed_str("abcdefgh");
        t.resize(TerminalSize::new(4, 4));

        // Every character survives, re-wrapped at the new width — the old code
        // clipped this to "abcd" and lost the rest for good. The re-wrap needs
        // more rows than the screen holds, so the first one is in scrollback;
        // the viewport has to reach past the screen to see it.
        let snap = t.snapshot(Viewport::tail(8));
        let text = joined(&snap);
        assert!(text.contains("abcdefgh"), "content lost: {text:?}");
    }

    #[test]
    fn widening_rejoins_a_soft_wrapped_line() {
        let mut t = term(4, 4);
        t.feed_str("abcdefgh");
        // Wrapped across two rows at 4 columns; at 10 it is one line again.
        t.resize(TerminalSize::new(10, 4));

        let snap = t.snapshot(Viewport::tail(4));
        assert_eq!(line_text(&snap, 0).trim_end(), "abcdefgh");
    }

    #[test]
    fn reflow_does_not_join_across_a_real_newline() {
        let mut t = term(4, 4);
        t.feed_str("ab\r\ncd");
        // Both rows fit their width, so neither is soft-wrapped. Widening must
        // keep them apart — joining here is the failure the `wrapped` flag exists
        // to prevent.
        t.resize(TerminalSize::new(10, 4));

        let snap = t.snapshot(Viewport::tail(4));
        assert_eq!(line_text(&snap, 0).trim_end(), "ab");
        assert_eq!(line_text(&snap, 1).trim_end(), "cd");
    }

    #[test]
    fn reflow_keeps_wide_glyphs_whole() {
        let mut t = term(6, 4);
        t.feed_str("中文字");
        t.resize(TerminalSize::new(4, 4));

        // A double-width glyph may never be split across the wrap, and its
        // spacer column must follow it to the new position.
        let snap = t.snapshot(Viewport::tail(8));
        let text = joined(&snap);
        assert!(
            text.contains('中') && text.contains('文') && text.contains('字'),
            "glyph lost: {text:?}"
        );
    }

    #[test]
    fn reflow_leaves_the_alternate_screen_alone() {
        let mut t = term(8, 4);
        t.feed_str("\x1b[?1049h");
        t.feed_str("abcdefgh");
        // A full-screen app redraws at the new size; joining its rows would
        // splice unrelated parts of a TUI together.
        t.resize(TerminalSize::new(4, 4));
        let snap = t.snapshot(Viewport::tail(4));
        assert_eq!(line_text(&snap, 0).trim_end(), "abcd");
    }

    // --- logical-line anchors across a reflow --------------------------------

    /// The whole buffer (scrollback + screen) as one string, rows separated by
    /// `|` — enough to assert that nothing was clipped away.
    fn whole_buffer(t: &VtTerminal) -> String {
        let total = t.snapshot(Viewport::tail(1)).total_rows;
        let snap = t.snapshot(Viewport {
            first_row: 0,
            height: total,
        });
        (0..snap.lines.len())
            .map(|row| line_text(&snap, row).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("|")
    }

    /// One absolute buffer row as text, spacers dropped.
    fn row_text(t: &VtTerminal, row: usize) -> String {
        t.state
            .row_at(row)
            .cells
            .iter()
            .filter(|cell| !cell.spacer)
            .map(|cell| cell.ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// The character in the absolute buffer CELL at `(row, col)` — column space,
    /// so a wide glyph's spacer occupies its own column, exactly as a selection
    /// anchor sees it.
    fn cell_at(t: &VtTerminal, row: usize, col: usize) -> char {
        t.state.row_at(row).cells[col].ch
    }

    /// Text of the half-open absolute span `[start, end)` — the convention the
    /// UI's selection uses (its end column is exclusive).
    fn span_text(t: &VtTerminal, start: (usize, usize), end: (usize, usize)) -> String {
        let mut out = String::new();
        for row in start.0..=end.0 {
            let cells = &t.state.row_at(row).cells;
            let lo = if row == start.0 { start.1 } else { 0 }.min(cells.len());
            let hi = if row == end.0 { end.1 } else { cells.len() }
                .min(cells.len())
                .max(lo);
            out.extend(cells[lo..hi].iter().filter(|c| !c.spacer).map(|c| c.ch));
        }
        out
    }

    #[test]
    fn a_selection_anchor_survives_a_narrowing_reflow() {
        let mut t = term(20, 4);
        t.feed_str("0123456789abcdefghij\r\nsecond\r\n");
        // "cdef" — absolute row 0, columns 12..16.
        assert_eq!(span_text(&t, (0, 12), (0, 16)), "cdef");
        let start = t.logical_anchor(0, 12);
        let end = t.logical_anchor(0, 16);

        t.resize(TerminalSize::new(10, 4));

        let start = t.resolve_anchor(start);
        let end = t.resolve_anchor(end);
        // The row it lived on split in two, so the anchor moved with the text.
        assert_ne!(
            start,
            (0, 12),
            "the reflow was expected to renumber this row"
        );
        assert_eq!(cell_at(&t, start.0, start.1), 'c');
        assert_eq!(span_text(&t, start, end), "cdef");
    }

    #[test]
    fn a_selection_anchor_survives_a_widening_reflow() {
        let mut t = term(10, 4);
        t.feed_str("0123456789abcdefghij\r\nsecond\r\n");
        // Soft-wrapped at 10 columns: "cdef" is on the continuation row.
        assert_eq!(span_text(&t, (1, 2), (1, 6)), "cdef");
        let start = t.logical_anchor(1, 2);
        let end = t.logical_anchor(1, 6);

        t.resize(TerminalSize::new(20, 4));

        let start = t.resolve_anchor(start);
        let end = t.resolve_anchor(end);
        assert_ne!(
            start,
            (1, 2),
            "the reflow was expected to renumber this row"
        );
        assert_eq!(span_text(&t, start, end), "cdef");
    }

    /// The snapshot has to carry the soft wrap out of the emulator. Nothing
    /// downstream can recover it — on screen a wrapped line and two separate
    /// lines are the same two rows — and the copy path is what needs it.
    #[test]
    fn a_snapshot_marks_the_row_a_soft_wrap_continues_from() {
        let mut t = term(10, 4);
        t.feed_str("0123456789abcdef\r\nsecond\r\n");
        let snap = t.snapshot(Viewport {
            first_row: 0,
            height: 4,
        });
        assert!(snap.lines[0].wrapped, "the row that ran off the margin");
        assert!(
            !snap.lines[1].wrapped,
            "its continuation ended with a newline"
        );
        assert!(
            !snap.lines[2].wrapped,
            "and a line of its own is not a wrap"
        );
    }

    /// `docker ps` pads its columns with spaces, so a row can wrap *just after*
    /// padding. Trimming those blanks would pull the continuation up against the
    /// wrong column when the two are rejoined for a copy.
    #[test]
    fn a_soft_wrapped_row_keeps_the_blanks_it_wrapped_after() {
        let mut t = term(10, 3);
        // "abc" then five spaces fills eight columns; "xy" fills the row and the
        // next character wraps. The blanks are between two pieces of text, so
        // they are content, not a ragged end.
        t.feed_str("abc     xyzz\r\n");
        let snap = t.snapshot(Viewport {
            first_row: 0,
            height: 3,
        });
        assert_eq!(line_text(&snap, 0), "abc     xy");
        assert_eq!(line_text(&snap, 1), "zz");
    }

    #[test]
    fn a_scroll_position_survives_a_reflow_in_both_directions() {
        let mut t = term(20, 4);
        // Lines longer than any of the widths under test, so every re-wrap
        // really does renumber the rows — with lines that fit at all three, the
        // absolute row number happens to survive and the test proves nothing.
        for i in 0..30 {
            t.feed_str(&format!("line-{i:02}{}\r\n", "-".repeat(17)));
        }
        // Row 20 is where the viewport is parked: the first row of line 10,
        // which takes two rows at 20 columns.
        assert!(row_text(&t, 20).starts_with("line-10"));
        let top = t.logical_anchor(20, 0);

        t.resize(TerminalSize::new(8, 4));
        let (row, _) = t.resolve_anchor(top);
        assert_ne!(row, 20, "the reflow was expected to renumber this row");
        assert!(
            row_text(&t, row).starts_with("line-10"),
            "narrowed to: {:?}",
            row_text(&t, row)
        );

        t.resize(TerminalSize::new(40, 4));
        let (row, _) = t.resolve_anchor(top);
        assert_ne!(row, 20, "the reflow was expected to renumber this row");
        assert!(
            row_text(&t, row).starts_with("line-10"),
            "widened to: {:?}",
            row_text(&t, row)
        );
    }

    #[test]
    fn an_anchor_next_to_a_wide_glyph_survives_a_reflow() {
        let mut t = term(8, 4);
        // a b 中(2,3) 文(4,5) c d — the wide glyphs each own two columns.
        t.feed_str("ab中文cd\r\n");
        assert_eq!(cell_at(&t, 0, 4), '文');
        assert_eq!(cell_at(&t, 0, 6), 'c');

        let wide = t.logical_anchor(0, 4);
        let after = t.logical_anchor(0, 6);

        // At 4 columns "ab中" fills the row and 文 cannot straddle the margin,
        // so it starts the continuation row.
        t.resize(TerminalSize::new(4, 4));

        let wide = t.resolve_anchor(wide);
        let after = t.resolve_anchor(after);
        assert_eq!(cell_at(&t, wide.0, wide.1), '文');
        assert_eq!(cell_at(&t, after.0, after.1), 'c');
        assert_eq!(span_text(&t, wide, after), "文");
    }

    #[test]
    fn shrinking_inside_the_alternate_screen_keeps_the_stashed_primary_rows() {
        let mut t = term(20, 6);
        t.feed_str("row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4\r\n");
        t.feed_str("\x1b[?1049h");
        t.feed_str("a full-screen app");
        // Shrinking used to clip the stashed primary at the bottom, so the rows
        // the app was covering never came back.
        t.resize(TerminalSize::new(20, 3));
        t.feed_str("\x1b[?1049l");

        let text = whole_buffer(&t);
        for i in 0..5 {
            assert!(text.contains(&format!("row{i}")), "row{i} lost: {text:?}");
        }
        assert!(!text.contains("full-screen"), "alt rows leaked: {text:?}");
    }

    #[test]
    fn narrowing_inside_the_alternate_screen_reflows_the_stashed_primary() {
        let mut t = term(20, 4);
        t.feed_str("0123456789abcdefghij\r\n");
        t.feed_str("\x1b[?1049h");
        t.feed_str("app");
        // The alternate screen must not reflow, but the primary behind it must —
        // clipping it to 10 columns threw away the second half of the line.
        t.resize(TerminalSize::new(10, 4));
        t.feed_str("\x1b[?1049l");

        let text = whole_buffer(&t).replace('|', "");
        assert!(
            text.contains("0123456789abcdefghij"),
            "stashed primary clipped: {text:?}"
        );
    }

    #[test]
    fn sgr_sets_foreground_color() {
        let mut t = term(20, 2);
        t.feed_str("\x1b[31mred\x1b[0m");
        let snap = t.snapshot(Viewport::tail(2));
        let first = &snap.lines[0].cells[0];
        assert_eq!(first.fg, Color::Indexed(1));
        assert_eq!(first.text, "red");
    }

    #[test]
    fn truecolor_foreground() {
        let mut t = term(20, 2);
        t.feed_str("\x1b[38;2;10;20;30mx");
        let snap = t.snapshot(Viewport::tail(2));
        assert_eq!(snap.lines[0].cells[0].fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn cursor_position_and_erase() {
        let mut t = term(20, 4);
        t.feed_str("hello world");
        t.feed_str("\x1b[1;1H"); // home
        t.feed_str("\x1b[0K"); // erase to end of line
        let snap = t.snapshot(Viewport::tail(4));
        assert_eq!(line_text(&snap, 0).trim_end(), "");
    }

    #[test]
    fn alternate_screen_round_trip() {
        let mut t = term(20, 3);
        t.feed_str("primary");
        t.feed_str("\x1b[?1049h"); // enter alt
        t.feed_str("alt-screen");
        let alt = t.snapshot(Viewport::tail(3));
        assert_eq!(line_text(&alt, 0).trim_end(), "alt-screen");
        t.feed_str("\x1b[?1049l"); // leave alt
        let restored = t.snapshot(Viewport::tail(3));
        assert_eq!(line_text(&restored, 0).trim_end(), "primary");
    }

    #[test]
    fn cursor_position_report() {
        let mut t = term(20, 4);
        t.feed_str("\x1b[3;5H\x1b[6n");
        let response = t.take_responses();
        assert_eq!(response, b"\x1b[3;5R");
    }

    #[test]
    fn wide_characters_occupy_two_columns() {
        let mut t = term(10, 2);
        t.feed_str("ab\u{4e2d}c"); // 中 is double width
        let snap = t.snapshot(Viewport::tail(2));
        assert_eq!(line_text(&snap, 0).trim_end(), "ab\u{4e2d}c");
    }

    #[test]
    fn osc_sets_title() {
        let mut t = term(20, 2);
        t.feed_str("\x1b]0;my-title\x07");
        let snap = t.snapshot(Viewport::tail(2));
        assert_eq!(snap.title, "my-title");
    }

    #[test]
    fn resize_preserves_recent_rows() {
        let mut t = term(20, 4);
        t.feed_str("l1\r\nl2\r\nl3");
        t.resize(TerminalSize::new(20, 2));
        let snap = t.snapshot(Viewport::tail(2));
        assert_eq!(line_text(&snap, 1).trim_end(), "l3");
    }
}
