//! One immutable style per pane, shared by painting and all pixel/cell conversions.
use super::*;
use adit_domain::{TerminalAppearance, TerminalPalette};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TerminalMetrics {
    pub font_size: f32,
    pub cell_width: f32,
    pub cell_height: f32,
}

impl TerminalMetrics {
    pub(crate) fn new(family: &str, size: f32) -> Self {
        let font_size = if size.is_finite() {
            size.clamp(MIN_FONT_SIZE as f32, MAX_FONT_SIZE as f32)
        } else {
            13.0
        };
        // Fixed cells remain authoritative even if the requested font is absent.
        // Consolas has a narrower advance than the system/Cascadia grid.
        let width_ratio = if family == "Consolas" {
            0.55
        } else {
            CELL_WIDTH_RATIO
        };
        Self {
            font_size,
            cell_width: font_size * width_ratio,
            cell_height: font_size * CELL_HEIGHT_RATIO,
        }
    }

    pub(crate) fn size_for_area(self, width: f32, height: f32) -> TerminalSize {
        TerminalSize::new(
            (width / self.cell_width).floor().clamp(20.0, 220.0) as u16,
            (height / self.cell_height).floor().clamp(6.0, 80.0) as u16,
        )
    }

    pub(crate) fn point(self, point: Point, size: TerminalSize) -> TerminalPoint {
        TerminalPoint {
            col: (point.x / self.cell_width)
                .floor()
                .clamp(0.0, f32::from(size.cols)) as usize,
            row: (point.y / self.cell_height)
                .floor()
                .clamp(0.0, f32::from(size.rows.saturating_sub(1))) as usize,
        }
    }

    pub(crate) fn cell_bounds(self, row: usize, col: usize, span: usize) -> iced::Rectangle {
        iced::Rectangle {
            x: col as f32 * self.cell_width,
            y: row as f32 * self.cell_height,
            width: span as f32 * self.cell_width,
            height: self.cell_height,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TerminalStyle {
    pub font: Font,
    pub metrics: TerminalMetrics,
    pub palette: TerminalPalette,
}

impl TerminalStyle {
    pub(crate) fn resolve(
        family: &str,
        size: f32,
        scheme: &str,
        appearance: &TerminalAppearance,
        environment: Environment,
    ) -> Self {
        let family = appearance.font_family.as_deref().unwrap_or(family);
        let size = appearance.font_size.map(f32::from).unwrap_or(size);
        let scheme = theme::scheme_for(appearance.color_scheme.as_deref().unwrap_or(scheme));
        let rgb = |(r, g, b)| [r, g, b];
        let mut palette = appearance.custom_palette.unwrap_or(TerminalPalette {
            background: rgb(scheme.background),
            foreground: rgb(scheme.foreground),
            selection: rgb(scheme.selection),
            ansi: scheme.ansi.map(rgb),
        });
        if let Some(background) = appearance.background {
            palette.background = background;
        } else if environment == Environment::Production {
            // An environment warning belongs to this pane, including its empty
            // padding. Preserve light/dark contrast and server-painted ANSI cells.
            palette.background = std::array::from_fn(|i| {
                ((u16::from(palette.background[i]) * 9 + u16::from([209u8, 69, 59][i])) / 10) as u8
            });
        }
        Self {
            font: theme::font_for(family),
            metrics: TerminalMetrics::new(family, size),
            palette,
        }
    }

    pub(crate) fn background(self) -> Color {
        rgb_color(self.palette.background)
    }
    pub(crate) fn foreground(self) -> Color {
        rgb_color(self.palette.foreground)
    }
    pub(crate) fn selection(self) -> Color {
        rgb_color(self.palette.selection)
    }

    pub(crate) fn selection_foreground(self) -> Color {
        let [r, g, b] = self.palette.selection;
        if 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b) > 140.0 {
            Color::from_rgb8(20, 22, 28)
        } else {
            Color::from_rgb8(245, 249, 255)
        }
    }

    pub(crate) fn color(self, color: TermColor, fallback: Color) -> Color {
        match color {
            TermColor::Default => fallback,
            TermColor::Rgb(r, g, b) => Color::from_rgb8(r, g, b),
            TermColor::Indexed(index) => self.indexed(index),
        }
    }

    pub(crate) fn indexed(self, index: u8) -> Color {
        match index {
            0..=15 => rgb_color(self.palette.ansi[index as usize]),
            16..=231 => {
                let c = index - 16;
                let level = |v| if v == 0 { 0 } else { 55 + v * 40 };
                Color::from_rgb8(level(c / 36), level((c / 6) % 6), level(c % 6))
            }
            232..=255 => {
                let v = 8 + (index - 232) * 10;
                Color::from_rgb8(v, v, v)
            }
        }
    }
}

fn rgb_color([r, g, b]: [u8; 3]) -> Color {
    Color::from_rgb8(r, g, b)
}

pub(crate) fn global_terminal_style(app: &AditApp) -> TerminalStyle {
    TerminalStyle::resolve(
        &app.font_family,
        app.font_size,
        &app.color_scheme,
        &TerminalAppearance::default(),
        Environment::None,
    )
}

pub(crate) fn terminal_style_for(app: &AditApp, session: Option<SessionId>) -> TerminalStyle {
    let profile = session
        .and_then(|id| app.manager.session_summary(id))
        .and_then(|summary| app.manager.profile(summary.profile_id));
    match profile {
        Some(profile) => TerminalStyle::resolve(
            &app.font_family,
            app.font_size,
            &app.color_scheme,
            &profile.session_options.terminal_appearance,
            profile.environment,
        ),
        None => global_terminal_style(app),
    }
}

pub(crate) fn active_terminal_style(app: &AditApp) -> TerminalStyle {
    terminal_style_for(app, app.manager.active_session())
}

pub(crate) fn session_terminal_size(app: &AditApp, session: SessionId) -> TerminalSize {
    pane_layout(app).pane_terminal_size(terminal_style_for(app, Some(session)).metrics)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(appearance: &TerminalAppearance) -> TerminalStyle {
        TerminalStyle::resolve("Cascadia Mono", 13.0, "Nord", appearance, Environment::None)
    }

    #[test]
    fn independent_fields_inherit_live_global_settings() {
        let inherited = style(&TerminalAppearance::default());
        assert_eq!(inherited.font, Font::with_name("Cascadia Mono"));
        assert_eq!(inherited.metrics.font_size, 13.0);
        let size_only = TerminalAppearance {
            font_size: Some(22),
            ..Default::default()
        };
        let updated = TerminalStyle::resolve(
            "Consolas",
            16.0,
            "Solarized Light",
            &size_only,
            Environment::None,
        );
        assert_eq!(updated.font, Font::with_name("Consolas"));
        assert_eq!(updated.metrics.font_size, 22.0);
        assert_eq!(updated.palette.background, [253, 246, 227]);
        assert_eq!(style(&size_only).metrics.font_size, 22.0);
        assert_eq!(inherited, style(&TerminalAppearance::default()));
    }

    #[test]
    fn different_fonts_and_sizes_keep_their_own_pixels_cells_scroll_and_colors() {
        let first = style(&TerminalAppearance {
            font_family: Some("Consolas".into()),
            font_size: Some(10),
            color_scheme: Some("Solarized Light".into()),
            ..Default::default()
        });
        let second = style(&TerminalAppearance {
            font_family: Some("Cascadia Code".into()),
            font_size: Some(24),
            color_scheme: Some("Dracula".into()),
            ..Default::default()
        });
        assert_ne!(first.font, second.font);
        assert_ne!(first.metrics, second.metrics);
        assert_ne!(
            first.color(TermColor::Indexed(4), first.foreground()),
            second.color(TermColor::Indexed(4), second.foreground())
        );
        for pane in [first, second, first] {
            let m = pane.metrics;
            let point = m.point(
                Point::new(m.cell_width * 7.25, m.cell_height * 2.25),
                m.size_for_area(600.0, 400.0),
            );
            assert_eq!(point, TerminalPoint { row: 2, col: 7 });
            assert_eq!(
                scroll_delta_to_rows(
                    mouse::ScrollDelta::Pixels {
                        x: 0.0,
                        y: -m.cell_height * 2.0,
                    },
                    m
                ),
                Some(-2)
            );
        }
        assert!(
            first.metrics.size_for_area(600.0, 400.0).cols
                > second.metrics.size_for_area(600.0, 400.0).cols
        );
    }

    #[test]
    fn custom_colors_and_prod_background_do_not_mutate_ansi_rgb_or_other_panes() {
        let base = style(&TerminalAppearance::default());
        let palette = TerminalPalette {
            background: [10, 20, 30],
            foreground: [210, 220, 230],
            selection: [240, 241, 242],
            ansi: [[7, 8, 9]; 16],
        };
        let custom = TerminalAppearance {
            custom_palette: Some(palette),
            ..Default::default()
        };
        let prod = TerminalStyle::resolve("", 13.0, "", &custom, Environment::Production);
        assert_ne!(prod.palette.background, palette.background);
        assert_eq!(
            prod.color(TermColor::Indexed(1), prod.foreground()),
            Color::from_rgb8(7, 8, 9)
        );
        assert_eq!(
            prod.color(TermColor::Rgb(1, 2, 3), prod.foreground()),
            Color::from_rgb8(1, 2, 3)
        );
        assert_eq!(
            prod.color(TermColor::Default, prod.background()),
            prod.background()
        );
        assert_eq!(prod.selection(), Color::from_rgb8(240, 241, 242));
        assert_eq!(prod.selection_foreground(), Color::from_rgb8(20, 22, 28));
        let explicit = TerminalAppearance {
            background: Some([30, 11, 12]),
            ..custom
        };
        let prod = TerminalStyle::resolve("", 13.0, "", &explicit, Environment::Production);
        assert_eq!(prod.palette.background, [30, 11, 12]);
        assert_eq!(base, style(&TerminalAppearance::default()));
    }

    #[test]
    fn sixel_render_rectangles_and_selection_cells_agree_for_both_pane_sizes() {
        use adit_terminal::{TerminalCore, VtTerminal};
        let mut terminal = VtTerminal::new(TerminalSize::new(30, 6));
        terminal.feed_str("\x1b[2;4H\x1bPq#1;2;100;0;0#1!8~\x1b\\");
        let snapshot = terminal.snapshot(Viewport::visible(6));
        let (row, tile) = snapshot
            .lines
            .iter()
            .enumerate()
            .find_map(|(row, line)| line.images.first().map(|tile| (row, tile)))
            .expect("decoder must hand a Sixel tile to the production renderer");
        for size in [10.0, 24.0] {
            let metrics = TerminalMetrics::new("Consolas", size);
            let bounds = metrics.cell_bounds(row, tile.col, 1);
            let cell = metrics.point(
                Point::new(
                    bounds.x + bounds.width / 2.0,
                    bounds.y + bounds.height / 2.0,
                ),
                snapshot.size,
            );
            assert_eq!(cell, TerminalPoint { row, col: tile.col });
            let style = TerminalStyle {
                font: Font::with_name("Consolas"),
                metrics,
                palette: style(&TerminalAppearance::default()).palette,
            };
            let _render = terminal_view(
                None,
                style,
                snapshot.clone(),
                true,
                None,
                0,
                Vec::new(),
                false,
                true,
                true,
            );
        }
    }

    fn panes() -> (AditApp, [SessionId; 2], [ProfileId; 2]) {
        let mut small = ConnectionProfile::new("small", "small.invalid", 22, "fixture");
        small.session_options.terminal_appearance = TerminalAppearance {
            font_family: Some("Consolas".into()),
            font_size: Some(10),
            ..Default::default()
        };
        let mut large = ConnectionProfile::new("large", "large.invalid", 22, "fixture");
        large.session_options.terminal_appearance = TerminalAppearance {
            font_family: Some("Cascadia Code".into()),
            font_size: Some(24),
            ..Default::default()
        };
        let profile_ids = [small.id, large.id];
        let mut manager = SessionManager::with_profiles(vec![small, large]);
        let ids = profile_ids.map(|id| manager.open_mock_session(id).unwrap());
        let mut app = AditApp {
            manager,
            font_family: "Cascadia Mono".into(),
            font_size: 13.0,
            window_width: 1360.0,
            window_height: 860.0,
            ..AditApp::default()
        };
        app.panes = ids.to_vec();
        focus_pane(&mut app, 0);
        (app, ids, profile_ids)
    }

    #[test]
    fn split_render_snapshot_and_mouse_selection_use_the_same_owner_metrics() {
        let (mut app, ids, _) = panes();
        let sizes = ids.map(|id| app.manager.snapshot_for(id, Viewport::tail(1)).size);
        assert_ne!(sizes[0], sizes[1]);
        for index in [0, 1, 0] {
            focus_pane(&mut app, index);
            assert_eq!(app.terminal_size, sizes[index]);
            assert_eq!(pane_snapshot(&app, ids[index], true).size, sizes[index]);
            let m = terminal_style_for(&app, Some(ids[index])).metrics;
            let point = Point::new(
                TERMINAL_PANEL_PADDING + m.cell_width * 3.25,
                TERMINAL_PANEL_PADDING + TERMINAL_HEADER_AND_GAP + m.cell_height * 2.25,
            );
            assert_eq!(
                terminal_point_from_cursor(&app, point),
                TerminalPoint { row: 2, col: 3 }
            );
            // Hovering a different pane must use its metrics before focus changes.
            let other = 1 - index;
            let m = terminal_style_for(&app, Some(ids[other])).metrics;
            let point = Point::new(
                TERMINAL_PANEL_PADDING + m.cell_width * 5.25,
                TERMINAL_PANEL_PADDING + TERMINAL_HEADER_AND_GAP + m.cell_height * 1.25,
            );
            assert_eq!(
                terminal_point_for_session(&app, Some(ids[other]), point),
                TerminalPoint { row: 1, col: 5 }
            );
            let _both = tiled_workspace_body(&app);
        }
        assert_eq!(app.manager.sessions().len(), 2);
    }

    #[test]
    fn mission_focus_return_and_resource_navigation_preserve_each_profile_style() {
        let (mut app, ids, _) = panes();
        let styles = ids.map(|id| terminal_style_for(&app, Some(id)));
        create_mission(&mut app, ids.to_vec());
        let split_sizes = ids.map(|id| session_terminal_size(&app, id));
        focus_mission(&mut app, Some(1));
        assert_eq!(app.manager.active_session(), Some(ids[1]));
        assert!(app.panes.is_empty());
        assert_eq!(active_terminal_style(&app), styles[1]);
        focus_mission(&mut app, None);
        for (index, id) in ids.into_iter().enumerate() {
            assert_eq!(terminal_style_for(&app, Some(id)), styles[index]);
            assert_eq!(
                app.manager.snapshot_for(id, Viewport::tail(1)).size,
                split_sizes[index]
            );
        }
        change_main_view(&mut app, MainView::Hosts);
        change_main_view(&mut app, MainView::Terminal);
        change_main_view(&mut app, MainView::Missions);
        assert_eq!(app.manager.sessions().len(), 2);
        assert_eq!(app.panes.len(), 2);
    }

    #[test]
    fn font_resize_preserves_selected_text_and_rebuilds_search_after_reflow() {
        let (mut app, ids, profile_ids) = panes();
        app.manager
            .send_input_to_active("echo appearance-anchor-fixture")
            .unwrap();
        app.search_query = "appearance-anchor-fixture".into();
        recompute_search(&mut app);
        let hit = *app
            .search_matches
            .last()
            .expect("mock echo must contain the fixture");
        app.terminal_selection = Some(TerminalSelection {
            start: TerminalPoint {
                row: hit.row,
                col: hit.col,
            },
            end: TerminalPoint {
                row: hit.row,
                col: hit.col + hit.len,
            },
        });
        let selected = selection_to_text(
            &active_terminal_snapshot(&app),
            app.terminal_selection.unwrap(),
        );
        let old = app.terminal_size;
        let mut options = app
            .manager
            .profile(profile_ids[0])
            .unwrap()
            .session_options
            .clone();
        options.terminal_appearance.font_size = Some(28);
        app.manager
            .set_profile_session_options(profile_ids[0], options)
            .unwrap();
        sync_terminal_size(&mut app);
        assert_ne!(old.cols, app.terminal_size.cols);
        // Use the full history: selection and search may be outside the current viewport.
        let total = app.manager.active_snapshot(Viewport::tail(1)).total_rows;
        let full = app.manager.active_snapshot(Viewport {
            first_row: 0,
            height: total,
        });
        assert_eq!(
            selection_to_text(&full, app.terminal_selection.unwrap()),
            selected
        );
        assert!(!app.search_matches.is_empty());
        assert_eq!(
            app.manager.snapshot_for(ids[1], Viewport::tail(1)).size,
            session_terminal_size(&app, ids[1])
        );
        assert_eq!(app.manager.sessions().len(), 2);
    }
}
