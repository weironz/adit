//! Adit terminal model.
//!
//! This crate converts a remote PTY byte stream into an immutable, render-ready
//! [`TerminalSnapshot`]. The real work lives in [`VtTerminal`], an Adit-owned VT
//! grid driven by the [`vte`](https://docs.rs/vte) escape-sequence parser. The
//! public types here are intentionally renderer-agnostic so `adit-ui` (or any
//! other front end) never has to understand escape sequences.

mod vt;

pub use vt::{set_scrollback_limit, VtTerminal};

use serde::{Deserialize, Serialize};

/// The mouse-reporting mode an app has enabled via DEC private modes: 1000
/// (button press/release), 1002 (also motion while a button is held), 1003
/// (all motion). When not [`MouseMode::Off`], the UI forwards mouse events to
/// the remote instead of doing local selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    Off,
    Normal,
    ButtonEvent,
    AnyMotion,
}

impl MouseMode {
    /// Whether motion (drag) events should be reported while a button is held.
    #[must_use]
    pub fn reports_drag(self) -> bool {
        matches!(self, MouseMode::ButtonEvent | MouseMode::AnyMotion)
    }

    /// Whether motion should be reported even with no button held.
    #[must_use]
    pub fn reports_any_motion(self) -> bool {
        matches!(self, MouseMode::AnyMotion)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl TerminalSize {
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { cols: 96, rows: 28 }
    }
}

/// A window into the terminal history requested by the renderer.
///
/// `first_row` is an absolute index into `scrollback + screen`. The sentinel
/// [`usize::MAX`] means "anchor to the bottom" so the most recent output stays
/// visible without the caller having to know how many lines exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub first_row: usize,
    pub height: usize,
}

impl Viewport {
    #[must_use]
    pub fn visible(height: usize) -> Self {
        Self {
            first_row: 0,
            height,
        }
    }

    #[must_use]
    pub fn tail(height: usize) -> Self {
        Self {
            first_row: usize::MAX,
            height,
        }
    }
}

/// A terminal color. `Default` defers to the renderer's theme; `Indexed` is an
/// xterm 256-color palette slot (0-15 are the named ANSI colors); `Rgb` is a
/// direct truecolor value.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// A run of one or more glyphs that share the same visual attributes.
///
/// The grid stores one cell per column, but [`TerminalSnapshot`] coalesces
/// adjacent cells with identical attributes into a single run to keep the
/// render tree small.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCell {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub underline: bool,
    pub italic: bool,
    pub dim: bool,
}

impl TerminalCell {
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            underline: false,
            italic: false,
            dim: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalLine {
    pub cells: Vec<TerminalCell>,
}

impl TerminalLine {
    #[must_use]
    pub fn from_cells(cells: impl IntoIterator<Item = TerminalCell>) -> Self {
        Self {
            cells: cells.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            cells: vec![TerminalCell::plain(text)],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    pub title: String,
    pub size: TerminalSize,
    pub first_row: usize,
    pub total_rows: usize,
    pub lines: Vec<TerminalLine>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub cursor_visible: bool,
}

impl TerminalSnapshot {
    #[must_use]
    pub fn empty(size: TerminalSize) -> Self {
        Self {
            title: String::from("terminal"),
            size,
            first_row: 0,
            total_rows: 0,
            lines: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalChangeSet {
    pub dirty_rows: Vec<usize>,
}

impl TerminalChangeSet {
    #[must_use]
    pub fn all(rows: u16) -> Self {
        Self {
            dirty_rows: (0..usize::from(rows)).collect(),
        }
    }
}

/// Adit-owned boundary over a terminal emulator. Keeps `adit-session` and
/// `adit-ui` independent of whichever VT implementation backs it.
pub trait TerminalCore {
    fn resize(&mut self, size: TerminalSize) -> TerminalChangeSet;
    fn feed(&mut self, bytes: &[u8]) -> TerminalChangeSet;
    fn snapshot(&self, viewport: Viewport) -> TerminalSnapshot;
}
