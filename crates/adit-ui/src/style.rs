use super::*;

pub(crate) const RADIUS_SM: f32 = 8.0;
pub(crate) const RADIUS_MD: f32 = 12.0;

/// Resolve a token to its light or dark value based on the active mode.
pub(crate) fn pick(light: Color, dark: Color) -> Color {
    if is_dark() {
        dark
    } else {
        light
    }
}

// Palette inspired by Termius: deep navy-charcoal dark chrome, a clean light
// theme, and a teal-green accent. Tokens resolve to a (light, dark) pair.
pub(crate) fn muted_text() -> Color {
    pick(Color::from_rgb8(108, 113, 134), Color::from_rgb8(139, 144, 160))
}

pub(crate) fn primary_text() -> Color {
    pick(Color::from_rgb8(28, 34, 48), Color::from_rgb8(230, 232, 238))
}

pub(crate) fn app_background() -> Color {
    pick(Color::from_rgb8(244, 246, 249), Color::from_rgb8(21, 22, 30))
}

/// Raised surface: sidebar, cards, dialogs, floating menus.
pub(crate) fn surface() -> Color {
    pick(Color::from_rgb8(255, 255, 255), Color::from_rgb8(27, 29, 41))
}

/// Secondary chrome: toolbar, status bar, tab strip.
pub(crate) fn surface_alt() -> Color {
    pick(Color::from_rgb8(238, 241, 246), Color::from_rgb8(27, 29, 41))
}

/// Recessed area the terminal panel floats on.
pub(crate) fn surface_sunken() -> Color {
    pick(Color::from_rgb8(231, 235, 241), Color::from_rgb8(18, 18, 25))
}

pub(crate) fn panel_background_hover() -> Color {
    pick(Color::from_rgb8(232, 242, 240), Color::from_rgb8(38, 42, 56))
}

pub(crate) fn field_background() -> Color {
    pick(Color::from_rgb8(255, 255, 255), Color::from_rgb8(32, 35, 47))
}

pub(crate) fn terminal_background() -> Color {
    let (r, g, b) = active_scheme().background;
    Color::from_rgb8(r, g, b)
}

pub(crate) fn selection_background() -> Color {
    let (r, g, b) = active_scheme().selection;
    Color::from_rgb8(r, g, b)
}

pub(crate) fn border_color() -> Color {
    pick(Color::from_rgb8(225, 230, 236), Color::from_rgb8(38, 42, 56))
}

pub(crate) fn border_strong() -> Color {
    pick(Color::from_rgb8(157, 217, 208), Color::from_rgb8(54, 64, 85))
}

pub(crate) fn accent() -> Color {
    // Deep enough that white button text stays legible.
    Color::from_rgb8(15, 158, 140)
}

/// Colour for OSC 8 hyperlink runs — a readable link blue on both light and dark
/// terminal backgrounds.
pub(crate) fn hyperlink_color() -> Color {
    Color::from_rgb8(88, 166, 255)
}

/// Whether the link-open modifier is held (Ctrl, or Cmd/⌘ on macOS). A terminal
/// hyperlink opens only on modifier+click, so a plain click always falls through
/// to selection / mouse-report passthrough (in every pane, regardless of mode).
pub(crate) fn link_open_modifier(app: &AditApp) -> bool {
    app.modifiers.control() || app.modifiers.logo()
}

/// Parse a `#RRGGBB` (or `RRGGBB`) hex string into a `Color`.
pub(crate) fn parse_hex_color(hex: &str) -> Option<Color> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::from_rgb8(r, g, b))
}

/// A profile's tab accent colour (environment preset or custom hex), or `None`.
pub(crate) fn profile_accent(app: &AditApp, profile_id: ProfileId) -> Option<Color> {
    let hex = app.manager.profile(profile_id)?.accent_hex()?;
    parse_hex_color(&hex)
}

/// A profile's short tab badge (e.g. `生产`), or `None`.
pub(crate) fn profile_badge(app: &AditApp, profile_id: ProfileId) -> Option<String> {
    app.manager.profile(profile_id)?.badge_label()
}

pub(crate) fn accent_hover() -> Color {
    Color::from_rgb8(22, 182, 164)
}

pub(crate) fn accent_pressed() -> Color {
    Color::from_rgb8(11, 124, 110)
}

/// Soft accent tint for selected/active backgrounds.
pub(crate) fn accent_soft() -> Color {
    pick(Color::from_rgb8(220, 242, 238), Color::from_rgb8(26, 48, 43))
}

pub(crate) fn danger() -> Color {
    Color::from_rgb8(229, 72, 77)
}

/// "This is up." Distinct from `accent()`, which marks the selected thing —
/// a host can be both, and one colour could not say so.
pub(crate) fn success() -> Color {
    pick(Color::from_rgb8(22, 143, 92), Color::from_rgb8(63, 185, 122))
}

pub(crate) fn danger_background() -> Color {
    pick(Color::from_rgb8(253, 237, 237), Color::from_rgb8(58, 36, 38))
}

pub(crate) fn transparent() -> Color {
    Color {
        a: 0.0,
        ..Color::BLACK
    }
}

pub(crate) fn border(radius: f32, width: f32, color: Color) -> Border {
    Border {
        radius: radius.into(),
        width,
        color,
    }
}

/// Pronounced elevation for modal dialogs.
pub(crate) fn soft_shadow() -> Shadow {
    Shadow {
        color: Color {
            r: 0.05,
            g: 0.09,
            b: 0.16,
            a: 0.18,
        },
        offset: Vector::new(0.0, 10.0),
        blur_radius: 28.0,
    }
}

/// Light elevation for dropdowns and context menus.
pub(crate) fn subtle_shadow() -> Shadow {
    Shadow {
        color: Color {
            r: 0.05,
            g: 0.09,
            b: 0.16,
            a: 0.12,
        },
        offset: Vector::new(0.0, 4.0),
        blur_radius: 14.0,
    }
}

pub(crate) fn app_background_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(app_background())),
        text_color: Some(primary_text()),
        ..container::Style::default()
    }
}

pub(crate) fn top_bar_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(0.0, 1.0, border_color()),
        ..container::Style::default()
    }
}

pub(crate) fn menu_dropdown_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, border_color()),
        shadow: subtle_shadow(),
        ..container::Style::default()
    }
}

pub(crate) fn toolbar_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_alt())),
        text_color: Some(primary_text()),
        border: border(0.0, 1.0, border_color()),
        ..container::Style::default()
    }
}

pub(crate) fn sidebar_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(0.0, 1.0, border_color()),
        ..container::Style::default()
    }
}

pub(crate) fn workspace_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_sunken())),
        text_color: Some(primary_text()),
        ..container::Style::default()
    }
}

pub(crate) fn terminal_panel_style(focused: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(terminal_background())),
        text_color: Some(default_foreground()),
        border: border(
            RADIUS_MD,
            1.5,
            if focused {
                accent()
            } else {
                Color::from_rgb8(38, 43, 54)
            },
        ),
        ..container::Style::default()
    }
}

/// The title bar of a split pane; accent-tinted while it is the focused pane.
pub(crate) fn pane_header_style(focused: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(if focused {
            accent_soft()
        } else {
            surface_alt()
        })),
        text_color: Some(primary_text()),
        border: border(
            RADIUS_SM,
            1.0,
            if focused { accent() } else { border_color() },
        ),
        ..container::Style::default()
    }
}

pub(crate) fn dialog_scrim_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color {
            r: 0.04,
            g: 0.06,
            b: 0.10,
            a: 0.42,
        })),
        ..container::Style::default()
    }
}

pub(crate) fn connection_dialog_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_MD, 1.0, border_color()),
        shadow: soft_shadow(),
        ..container::Style::default()
    }
}

pub(crate) fn status_bar_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_alt())),
        text_color: Some(muted_text()),
        border: border(0.0, 1.0, border_color()),
        ..container::Style::default()
    }
}

pub(crate) fn error_panel_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(danger_background())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, danger()),
        ..container::Style::default()
    }
}

pub(crate) fn text_input_style(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    let border_color = match status {
        text_input::Status::Focused { .. } => accent(),
        text_input::Status::Hovered => border_strong(),
        text_input::Status::Active | text_input::Status::Disabled => border_color(),
    };

    text_input::Style {
        background: Background::Color(field_background()),
        border: border(RADIUS_SM, 1.0, border_color),
        icon: muted_text(),
        placeholder: muted_text(),
        value: primary_text(),
        selection: accent_soft(),
    }
}

pub(crate) fn base_button_style(background: Color, text_color: Color, border_color: Color) -> button::Style {
    button::Style {
        background: Some(Background::Color(background)),
        text_color,
        border: border(RADIUS_SM, 1.0, border_color),
        ..button::Style::default()
    }
}

pub(crate) fn primary_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => accent_hover(),
        button::Status::Pressed => accent_pressed(),
        button::Status::Disabled => Color::from_rgb8(206, 213, 224),
        button::Status::Active => accent(),
    };
    base_button_style(background, Color::WHITE, background)
}

pub(crate) fn secondary_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => panel_background_hover(),
        button::Status::Pressed => accent_soft(),
        button::Status::Disabled => surface_alt(),
        button::Status::Active => surface(),
    };
    base_button_style(background, primary_text(), border_color())
}

/// A restrained destructive style: danger-coloured text on a soft tinted fill,
/// filling in on hover. Used for the transfer "stop" / "stop all" controls.
pub(crate) fn danger_button_style(status: button::Status) -> button::Style {
    let (background, text_color) = match status {
        button::Status::Hovered => (danger(), Color::WHITE),
        button::Status::Pressed => (danger(), Color::WHITE),
        button::Status::Disabled => (surface_alt(), muted_text()),
        button::Status::Active => (danger_background(), danger()),
    };
    base_button_style(background, text_color, danger())
}

pub(crate) fn method_button_style(selected: bool, status: button::Status) -> button::Style {
    let background = match (selected, status) {
        (true, button::Status::Pressed) => accent_pressed(),
        (true, button::Status::Hovered) => accent_hover(),
        (true, _) => accent(),
        (false, button::Status::Hovered) => panel_background_hover(),
        (false, button::Status::Pressed) => accent_soft(),
        _ => surface(),
    };
    let border_color = if selected { accent() } else { border_color() };
    base_button_style(
        background,
        if selected { Color::WHITE } else { primary_text() },
        border_color,
    )
}

pub(crate) fn menu_button_style(active: bool, status: button::Status) -> button::Style {
    let background = match (active, status) {
        (true, _) => accent_soft(),
        (false, button::Status::Hovered) => panel_background_hover(),
        (false, button::Status::Pressed) => accent_soft(),
        _ => transparent(),
    };
    base_button_style(background, primary_text(), transparent())
}

/// The whole-tab pill: an accent-bordered surface when active, a flat chip
/// otherwise. The title and close controls share this single background.
/// Tab pill style; `drop_target` highlights the tab a dragged tab will drop onto.
pub(crate) fn tab_container_style_dnd(active: bool, dragging: bool) -> container::Style {
    let background = if dragging {
        accent_soft()
    } else if active {
        surface()
    } else {
        surface_alt()
    };
    let border_color = if active || dragging {
        accent()
    } else {
        border_color()
    };
    container::Style {
        background: Some(Background::Color(background)),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, if dragging { 2.0 } else { 1.0 }, border_color),
        // Lift the dragged tab so it reads as picked up while it slides.
        shadow: if dragging {
            subtle_shadow()
        } else {
            Shadow::default()
        },
        ..container::Style::default()
    }
}

/// Subtle close glyph that hugs the title and lifts gently on hover.
pub(crate) fn tab_close_button_style(status: button::Status) -> button::Style {
    let (background, text_color) = match status {
        button::Status::Hovered | button::Status::Pressed => {
            (panel_background_hover(), primary_text())
        }
        _ => (transparent(), muted_text()),
    };
    base_button_style(background, text_color, transparent())
}

pub(crate) fn close_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => danger_background(),
        button::Status::Pressed => Color::from_rgb8(250, 220, 220),
        _ => transparent(),
    };
    let text_color = match status {
        button::Status::Hovered | button::Status::Pressed => danger(),
        _ => muted_text(),
    };
    base_button_style(background, text_color, transparent())
}

pub(crate) fn menu_command_button_style(status: button::Status) -> button::Style {
    secondary_button_style(status)
}

pub(crate) fn toolbar_icon_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => panel_background_hover(),
        button::Status::Pressed => accent_soft(),
        _ => transparent(),
    };
    base_button_style(background, primary_text(), transparent())
}


pub(crate) fn toolbar_input_style(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    let border_color = match status {
        text_input::Status::Focused { .. } => accent(),
        text_input::Status::Hovered => border_strong(),
        text_input::Status::Active | text_input::Status::Disabled => border_color(),
    };

    text_input::Style {
        background: Background::Color(field_background()),
        border: border(RADIUS_SM, 1.0, border_color),
        icon: muted_text(),
        placeholder: muted_text(),
        value: primary_text(),
        selection: accent_soft(),
    }
}

pub(crate) fn sidebar_header_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_alt())),
        text_color: Some(muted_text()),
        border: border(0.0, 1.0, border_color()),
        ..container::Style::default()
    }
}

pub(crate) fn sidebar_tool_button_style(status: button::Status) -> button::Style {
    toolbar_icon_button_style(status)
}

/// One card on the 设置 page: a bordered block holding a single option, or one
/// sync provider. Selection shows as fill and border rather than a tick, so a
/// glance down a column of them lands on the active one without reading.
pub(crate) fn settings_card_style(selected: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(if selected {
            accent_soft()
        } else {
            surface_alt()
        })),
        text_color: Some(primary_text()),
        border: border(
            RADIUS_SM,
            1.0,
            if selected { accent() } else { border_color() },
        ),
        ..container::Style::default()
    }
}

/// The category rail down the left of the 设置 page. Held apart from the
/// content by its own fill rather than a rule, which reads quieter at the
/// small sizes this dialog uses.
pub(crate) fn settings_rail_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_alt())),
        border: border(RADIUS_SM, 0.0, transparent()),
        ..container::Style::default()
    }
}

pub(crate) fn group_row_style(drop_target: bool) -> container::Style {
    let background = if drop_target {
        accent_soft()
    } else {
        transparent()
    };
    let border_color = if drop_target { accent() } else { transparent() };

    container::Style {
        background: Some(Background::Color(background)),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, border_color),
        ..container::Style::default()
    }
}

pub(crate) fn tree_item_container_style(selected: bool, hovered: bool, dragging: bool) -> container::Style {
    // The dragged row gets a clear "lifted" accent (soft fill + thicker accent
    // border) so, together with its live slide under the cursor, the drag is
    // obvious.
    let background = if dragging || selected {
        accent_soft()
    } else if hovered {
        panel_background_hover()
    } else {
        transparent()
    };

    let border_color = if dragging || selected {
        accent()
    } else {
        transparent()
    };

    container::Style {
        background: Some(Background::Color(background)),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, if dragging { 2.0 } else { 1.0 }, border_color),
        // A shadow "lifts" the dragged row off the list so it reads as picked up.
        shadow: if dragging {
            soft_shadow()
        } else {
            Shadow::default()
        },
        ..container::Style::default()
    }
}

pub(crate) fn profile_context_menu_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, border_color()),
        shadow: subtle_shadow(),
        ..container::Style::default()
    }
}

/// A vertical context-menu row: left-aligned, subtle hover, red for destructive.
pub(crate) fn profile_menu_item_style(status: button::Status, destructive: bool) -> button::Style {
    let hover_bg = if destructive {
        danger_background()
    } else {
        panel_background_hover()
    };
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => hover_bg,
        _ => transparent(),
    };
    let text_color = if destructive { danger() } else { primary_text() };
    let mut style = base_button_style(background, text_color, transparent());
    style.border = border(RADIUS_SM - 2.0, 0.0, transparent());
    style
}

pub(crate) fn profile_edit_menu_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_MD, 1.0, border_color()),
        ..container::Style::default()
    }
}

pub(crate) fn status_color(status: SessionStatus) -> Color {
    match status {
        SessionStatus::Connecting => Color::from_rgb8(245, 158, 11),
        SessionStatus::Connected => Color::from_rgb8(34, 197, 94),
        SessionStatus::Disconnected => muted_text(),
        SessionStatus::Error => danger(),
    }
}
