//! Terminal appearance: the font presets and colour schemes the settings UI
//! offers, and the lookups that turn a persisted name back into an index.
//!
//! The active choices live in `super`'s atomics rather than here: they are set
//! once per frame at the top of `view`, so the deep render and hit-test paths
//! can read them without threading a palette through every call.

use super::{TERM_FONT, TERM_SCHEME};
use iced::Font;
use std::sync::atomic::Ordering;

/// Selectable terminal fonts. The first is the system monospace default; the
/// rest are common Windows monospace families resolved by name (a missing
/// family falls back through cosmic-text, never a hard error).
pub(super) const FONT_PRESETS: &[(&str, Option<&'static str>)] = &[
    ("系统等宽", None),
    ("Consolas", Some("Consolas")),
    ("Cascadia Mono", Some("Cascadia Mono")),
    ("Cascadia Code", Some("Cascadia Code")),
    ("Courier New", Some("Courier New")),
    ("Lucida Console", Some("Lucida Console")),
];

/// The base terminal font (family only; per-cell weight is layered on top).
pub(super) fn term_font() -> Font {
    let idx = TERM_FONT.load(Ordering::Relaxed) as usize;
    match FONT_PRESETS.get(idx).and_then(|(_, family)| *family) {
        Some(name) => Font::with_name(name),
        None => Font::MONOSPACE,
    }
}

/// Preset index for a persisted font-family display name (0 = system default).
pub(super) fn font_preset_index(name: &str) -> u8 {
    FONT_PRESETS
        .iter()
        .position(|(display, _)| *display == name)
        .unwrap_or(0) as u8
}

/// A terminal color scheme: window background/foreground, selection highlight,
/// and the 16 ANSI colors (indices 16..=255 use the standard xterm cube/ramp).
pub(super) struct ColorScheme {
    pub(super) name: &'static str,
    pub(super) background: (u8, u8, u8),
    pub(super) foreground: (u8, u8, u8),
    pub(super) selection: (u8, u8, u8),
    pub(super) ansi: [(u8, u8, u8); 16],
}

pub(super) const COLOR_SCHEMES: &[ColorScheme] = &[
    ColorScheme {
        name: "默认",
        background: (20, 21, 28),
        foreground: (220, 226, 235),
        selection: (22, 92, 84),
        ansi: [
            (0, 0, 0),
            (205, 0, 0),
            (0, 205, 0),
            (205, 205, 0),
            (0, 0, 238),
            (205, 0, 205),
            (0, 205, 205),
            (229, 229, 229),
            (127, 127, 127),
            (255, 0, 0),
            (0, 255, 0),
            (255, 255, 0),
            (92, 92, 255),
            (255, 0, 255),
            (0, 255, 255),
            (255, 255, 255),
        ],
    },
    ColorScheme {
        name: "Dracula",
        background: (40, 42, 54),
        foreground: (248, 248, 242),
        selection: (68, 71, 90),
        ansi: [
            (33, 34, 44),
            (255, 85, 85),
            (80, 250, 123),
            (241, 250, 140),
            (189, 147, 249),
            (255, 121, 198),
            (139, 233, 253),
            (248, 248, 242),
            (98, 114, 164),
            (255, 110, 110),
            (105, 255, 148),
            (255, 255, 165),
            (214, 172, 255),
            (255, 146, 223),
            (164, 255, 255),
            (255, 255, 255),
        ],
    },
    ColorScheme {
        name: "One Dark",
        background: (40, 44, 52),
        foreground: (171, 178, 191),
        selection: (62, 68, 81),
        ansi: [
            (40, 44, 52),
            (224, 108, 117),
            (152, 195, 121),
            (229, 192, 123),
            (97, 175, 239),
            (198, 120, 221),
            (86, 182, 194),
            (171, 178, 191),
            (92, 99, 112),
            (224, 108, 117),
            (152, 195, 121),
            (229, 192, 123),
            (97, 175, 239),
            (198, 120, 221),
            (86, 182, 194),
            (255, 255, 255),
        ],
    },
    ColorScheme {
        name: "Nord",
        background: (46, 52, 64),
        foreground: (216, 222, 233),
        selection: (67, 76, 94),
        ansi: [
            (59, 66, 82),
            (191, 97, 106),
            (163, 190, 140),
            (235, 203, 139),
            (129, 161, 193),
            (180, 142, 173),
            (136, 192, 208),
            (229, 233, 240),
            (76, 86, 106),
            (191, 97, 106),
            (163, 190, 140),
            (235, 203, 139),
            (129, 161, 193),
            (180, 142, 173),
            (143, 188, 187),
            (236, 239, 244),
        ],
    },
    ColorScheme {
        name: "Gruvbox Dark",
        background: (40, 40, 40),
        foreground: (235, 219, 178),
        selection: (80, 73, 69),
        ansi: [
            (40, 40, 40),
            (204, 36, 29),
            (152, 151, 26),
            (215, 153, 33),
            (69, 133, 136),
            (177, 98, 134),
            (104, 157, 106),
            (168, 153, 132),
            (146, 131, 116),
            (251, 73, 52),
            (184, 187, 38),
            (250, 189, 47),
            (131, 165, 152),
            (211, 134, 155),
            (142, 192, 124),
            (235, 219, 178),
        ],
    },
    ColorScheme {
        name: "Solarized Dark",
        background: (0, 43, 54),
        foreground: (131, 148, 150),
        selection: (7, 54, 66),
        ansi: [
            (7, 54, 66),
            (220, 50, 47),
            (133, 153, 0),
            (181, 137, 0),
            (38, 139, 210),
            (211, 54, 130),
            (42, 161, 152),
            (238, 232, 213),
            (0, 43, 54),
            (203, 75, 22),
            (88, 110, 117),
            (101, 123, 131),
            (131, 148, 150),
            (108, 113, 196),
            (147, 161, 161),
            (253, 246, 227),
        ],
    },
    ColorScheme {
        name: "Solarized Light",
        background: (253, 246, 227),
        foreground: (101, 123, 131),
        selection: (238, 232, 213),
        ansi: [
            (7, 54, 66),
            (220, 50, 47),
            (133, 153, 0),
            (181, 137, 0),
            (38, 139, 210),
            (211, 54, 130),
            (42, 161, 152),
            (238, 232, 213),
            (0, 43, 54),
            (203, 75, 22),
            (88, 110, 117),
            (101, 123, 131),
            (131, 148, 150),
            (108, 113, 196),
            (147, 161, 161),
            (253, 246, 227),
        ],
    },
];

/// The active color scheme (defaults to the first if the index is stale).
pub(super) fn active_scheme() -> &'static ColorScheme {
    let idx = TERM_SCHEME.load(Ordering::Relaxed) as usize;
    &COLOR_SCHEMES[idx.min(COLOR_SCHEMES.len() - 1)]
}

/// Scheme index for a persisted scheme name (0 = default palette).
pub(super) fn color_scheme_index(name: &str) -> u8 {
    COLOR_SCHEMES
        .iter()
        .position(|scheme| scheme.name == name)
        .unwrap_or(0) as u8
}
