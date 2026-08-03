use super::*;
use iced::widget::column;

/// Which top-level view fills the area beside the nav rail.
///
/// Public only because `Message` is, and it rides inside one of its variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainView {
    /// The card-based host manager.
    Hosts,
    /// Session tabs and the terminal.
    Terminal,
}

pub(crate) const NAV_RAIL_WIDTH: f32 = 156.0;
pub(crate) const HOST_CARD_WIDTH: f32 = 260.0;
/// What a grid card builds out to (10px padding around a 34px tile), for the
/// absolute layout the animated bands use.
pub(crate) const HOST_CARD_HEIGHT: f32 = 54.0;


/// One preset a profile's tile can use.
pub(crate) struct HostIcon {
    /// What a profile stores. The artwork behind it can change without
    /// invalidating anyone's choice.
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    pub(crate) glyph: &'static str,
    pub(crate) rgb: (u8, u8, u8),
}

/// The icons a profile can carry.
///
/// Glyphs and brand-ish colours rather than real logos: shipping a
/// distribution's artwork means shipping its trademark, and the colour does most
/// of the recognising anyway — an orange tile in a wall of grey ones reads as
/// Ubuntu long before its shape does.
pub(crate) const HOST_ICONS: &[HostIcon] = &[
    HostIcon { key: "ubuntu", label: "Ubuntu", glyph: "\u{25c9}", rgb: (233, 84, 32) },
    HostIcon { key: "debian", label: "Debian", glyph: "\u{25c8}", rgb: (215, 10, 83) },
    HostIcon { key: "redhat", label: "RHEL", glyph: "\u{25b3}", rgb: (238, 0, 0) },
    HostIcon { key: "alpine", label: "Alpine", glyph: "\u{25b2}", rgb: (13, 89, 126) },
    HostIcon { key: "windows", label: "Windows", glyph: "\u{25a6}", rgb: (0, 120, 212) },
    HostIcon { key: "server", label: "服务器", glyph: "\u{25a4}", rgb: (90, 100, 120) },
    HostIcon { key: "network", label: "网络", glyph: "\u{21c4}", rgb: (16, 138, 122) },
    HostIcon { key: "database", label: "数据库", glyph: "\u{25ab}", rgb: (52, 92, 168) },
    HostIcon { key: "container", label: "容器", glyph: "\u{25a7}", rgb: (36, 150, 237) },
];

/// The glyph and colour a profile's tile shows.
///
/// An unset icon falls back to the protocol and the profile's accent, which is
/// what every profile did before the field existed — so nothing needed migrating
/// and nothing looks different until someone chooses.
pub(crate) fn host_icon(app: &AditApp, profile: &ConnectionProfile) -> (&'static str, Color) {
    HOST_ICONS
        .iter()
        .find(|icon| icon.key == profile.icon)
        .map(|icon| {
            let (r, g, b) = icon.rgb;
            (icon.glyph, Color::from_rgb8(r, g, b))
        })
        .unwrap_or_else(|| {
            (
                protocol_glyph(profile.protocol),
                profile_accent(app, profile.id).unwrap_or_else(accent),
            )
        })
}

/// Profiles with a session that is up right now.
///
/// Built once per render and handed down, rather than each card scanning the
/// session list — with a hundred hosts and a dozen sessions that is the
/// difference between one pass and a hundred.
pub(crate) fn connected_profiles(app: &AditApp) -> std::collections::HashSet<ProfileId> {
    app.manager
        .sessions()
        .into_iter()
        .filter(|session| session.status == SessionStatus::Connected)
        .map(|session| session.profile_id)
        .collect()
}

/// The glyph on a host's tile.
///
/// The protocol, not the operating system. The reference client shows an OS
/// logo, but Adit never asks which OS a host runs, and guessing from its name
/// would be wrong often enough to be worse than nothing. The protocol is
/// something a profile actually knows, and it is what changes when opening it.
pub(crate) fn protocol_glyph(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Ssh => "\u{25b8}",
        Protocol::LocalShell => "\u{25a3}",
        Protocol::Serial => "\u{2325}",
        Protocol::Rdp => "\u{25a4}",
    }
}

/// A small filled dot, for "this host has a session up".
pub(crate) fn online_dot() -> Element<'static, Message> {
    container(Space::new())
        .width(Length::Fixed(7.0))
        .height(Length::Fixed(7.0))
        .style(|_theme| container::Style {
            background: Some(Background::Color(success())),
            border: Border {
                radius: 999.0.into(),
                ..Border::default()
            },
            ..container::Style::default()
        })
        .into()
}

/// One host, as a card: an accent tile, the name, and `user@host` beneath it.
pub(crate) fn host_card(
    app: &AditApp,
    profile: &ConnectionProfile,
    online: bool,
    // Cards stretch, so their width is not a constant and the midpoint the drop
    // test needs cannot be one either.
    card_width: f32,
) -> Element<'static, Message> {
    let (glyph, tile_color) = host_icon(app, profile);
    // The source fades while its ghost travels — the ghost is the thing being
    // moved, and a second highlighted copy sitting in place reads as two cards.
    let dragging = app.dragged_profile == Some(profile.id);
    let tile_color = if dragging {
        Color { a: 0.35, ..tile_color }
    } else {
        tile_color
    };
    let tile = container(text(glyph).size(15).color(Color::WHITE))
    .width(Length::Fixed(34.0))
    .height(Length::Fixed(34.0))
    .center_x(Length::Fixed(34.0))
    .center_y(Length::Fixed(34.0))
    .style(move |_theme| container::Style {
        background: Some(Background::Color(tile_color)),
        border: Border {
            radius: RADIUS_SM.into(),
            ..Border::default()
        },
        ..container::Style::default()
    });

    let subtitle = if profile.username.trim().is_empty() {
        profile.host.clone()
    } else {
        format!("{}@{}", profile.username, profile.host)
    };

    let profile_id = profile.id;
    let dragging = app.dragged_profile == Some(profile_id);
    // Whether the dragged card would land on this one.
    let incoming = matches!(
        &app.profile_drop,
        Some(ProfileDrop::Beside { profile_id: target, .. }) if *target == profile_id
    );
    mouse_area(
        container(
        row![
            tile,
            column![
                {
                    let mut title = row![text(profile.name.clone()).size(13).color(primary_text())]
                        .spacing(6)
                        .align_y(Alignment::Center);
                    if online {
                        title = title.push(online_dot());
                    }
                    title
                },
                text(subtitle).size(11).font(Font::MONOSPACE).color(muted_text()),
            ]
            .spacing(3),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .width(Fill)
    .padding(10)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(if dragging {
            panel_background_hover()
        } else {
            app_background()
        })),
        text_color: Some(primary_text()),
        // A thicker accent edge on the target says where the card will land.
        // Without it the drag read as inert even while it worked: the move
        // happened, and nothing on screen had said it would.
        border: Border {
            color: if dragging || incoming {
                accent()
            } else {
                border_color()
            },
            width: if incoming { 2.5 } else { 1.0 },
            radius: RADIUS_SM.into(),
        },
        ..container::Style::default()
    }),
    )
    // The same press/move/release the tree uses, so one drag implementation
    // serves both views. Press arms rather than acts, which is what makes a drag
    // possible at all — and why a single click selects here and a double click
    // connects, exactly as in the tree.
    .on_press(Message::GridProfilePressed(profile_id))
    .on_release(Message::ProfileDropped(profile_id))
    .on_double_click(Message::ProfileDoubleClicked(profile_id))
    .on_right_press(Message::ShowProfileContextMenu(profile_id))
    .on_enter(Message::GridProfileHovered(profile_id))
    // Left half or right half, not top or bottom. `profile_drop_position` splits
    // on Y because the tree stacks its rows; a grid puts them side by side, so
    // asking which vertical half the cursor is in answers a question nobody
    // asked and drops the card somewhere unrelated to where it was let go.
    .on_move(move |point| {
        let position = if point.x < card_width / 2.0 {
            ProfileDropPosition::Before
        } else {
            ProfileDropPosition::After
        };
        Message::GridProfileDragOver(profile_id, position)
    })
    .on_exit(Message::ProfileHoverExited(profile_id))
    .into()
}

/// A section heading with the count on the right, the way the reference client
/// labels each band of its host list.
pub(crate) fn host_section_header(
    icon: Option<&'static str>,
    title: String,
    count: usize,
    online: usize,
    // The group this band is, when it is one. A real group heading doubles as
    // the drop target for "put this host in that group" — the same
    // `ProfileDroppedOnGroup` the sidebar's folder rows use, so dragging behaves
    // the same in both views and there is one implementation of it. `None` for
    // 最近连接, 搜索结果 and 主机, which are not groups a host can move into.
    drop_group: Option<String>,
) -> Element<'static, Message> {
    let pill = |label: String, color: Color| {
        container(text(label).size(11).color(color))
            .padding([2, 8])
            .style(|_theme| container::Style {
                background: Some(Background::Color(surface_alt())),
                border: Border {
                    radius: 999.0.into(),
                    ..Border::default()
                },
                ..container::Style::default()
            })
    };
    // Bold and dark: these separate one band from the next, and at the same
    // weight as the host names under them they stop doing that.
    let mut header = row![].spacing(6).align_y(Alignment::Center);
    if let Some(icon) = icon {
        header = header.push(text(icon).size(12).color(muted_text()));
    }
    header = header
        .push(text(title).size(13).color(primary_text()).font(Font {
            weight: Weight::Bold,
            ..Font::DEFAULT
        }))
        .push(Space::new().width(Fill));
    if online > 0 {
        header = header.push(pill(format!("{online} 个在线"), success()));
    }
    let header = header
        .push(pill(format!("{count} 台"), muted_text()))
        .spacing(8)
        .align_y(Alignment::Center);

    let Some(group) = drop_group else {
        return header.into();
    };
    let (enter, hover, release) = (group.clone(), group.clone(), group);
    mouse_area(container(header).width(Fill).padding([2, 0]))
        .on_enter(Message::ProfileDragOverGroup(enter))
        .on_move(move |_| Message::ProfileDragOverGroup(hover.clone()))
        .on_release(Message::ProfileDroppedOnGroup(release))
        .into()
}

/// One host as a full-width row — the shape List and Tree share, differing only
/// in how far it is indented.
pub(crate) fn host_row(app: &AditApp, profile: &ConnectionProfile, indent: f32, online: bool) -> Element<'static, Message> {
    // The accent while idle, the success colour while a session is up: one mark
    // doing both jobs keeps the row from growing a second column of dots.
    let dot_color = if online {
        success()
    } else {
        host_icon(app, profile).1
    };
    let dot = container(Space::new())
        .width(Length::Fixed(8.0))
        .height(Length::Fixed(8.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(dot_color)),
            border: Border {
                radius: 999.0.into(),
                ..Border::default()
            },
            ..container::Style::default()
        });
    let subtitle = if profile.username.trim().is_empty() {
        profile.host.clone()
    } else {
        format!("{}@{}", profile.username, profile.host)
    };

    let profile_id = profile.id;
    let dragging = app.dragged_profile == Some(profile_id);
    mouse_area(
        container(
            row![
                Space::new().width(Length::Fixed(indent)),
                dot,
                text(profile.name.clone()).size(13).color(primary_text()),
                Space::new().width(Fill),
                text(subtitle).size(11).font(Font::MONOSPACE).color(muted_text()),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .width(Fill)
        .padding([7, 10])
        .style(move |_theme| container::Style {
            background: Some(Background::Color(if dragging {
                panel_background_hover()
            } else {
                Color::TRANSPARENT
            })),
            text_color: Some(primary_text()),
            border: Border {
                radius: RADIUS_SM.into(),
                ..Border::default()
            },
            ..container::Style::default()
        }),
    )
    .on_press(Message::GridProfilePressed(profile_id))
    .on_release(Message::ProfileDropped(profile_id))
    .on_double_click(Message::ProfileDoubleClicked(profile_id))
    .on_right_press(Message::ShowProfileContextMenu(profile_id))
    .on_enter(Message::GridProfileHovered(profile_id))
    .on_move(move |point| Message::GridProfileDragOver(profile_id, profile_drop_position(point)))
    .on_exit(Message::ProfileHoverExited(profile_id))
    .into()
}

pub(crate) fn host_layout_button(current: HostLayout, target: HostLayout, label: &'static str) -> Element<'static, Message> {
    let selected = current == target;
    button(text(label).size(12))
        .padding([5, 12])
        .style(move |_theme, status| {
            if selected {
                primary_button_style(status)
            } else {
                secondary_button_style(status)
            }
        })
        .on_press(Message::HostLayoutChanged(target))
        .into()
}


/// How many cards fit across, from the window width.
///
/// Was hard-coded to three, which left two thirds of a wide window empty. iced
/// gives a view function no measured width, but the window's own is already
/// tracked, and the rail and padding around the grid are fixed — so this is
/// arithmetic rather than a guess.
pub(crate) fn host_columns(app: &AditApp) -> usize {
    let rail = if app.main_view == MainView::Hosts {
        NAV_RAIL_WIDTH
    } else {
        0.0
    };
    let available = app.window_width - rail - 40.0;
    ((available / (HOST_CARD_WIDTH + 8.0)).floor() as usize).clamp(1, 8)
}

/// A group, as a card. Collapsed by nature: the point of the band is to show
/// that a group exists and how big it is, not to spill it across the screen.
///
/// Clicking toggles the same `collapsed_groups` the sidebar tree uses, so a
/// group folded here is folded there too — that is what "aligned with the
/// session manager" has to mean if it means anything.
///
/// The same press/enter/release the sidebar's folder rows carry, so a host
/// dragged onto it lands in the group.
pub(crate) fn group_card(name: String, count: usize, expanded: bool, targeted: bool) -> Element<'static, Message> {
    let (press, enter, hover, release) =
        (name.clone(), name.clone(), name.clone(), name.clone());
    let tile = container(text(if expanded { "\u{25be}" } else { "\u{25b8}" }).size(15).color(Color::WHITE))
        .width(Length::Fixed(34.0))
        .height(Length::Fixed(34.0))
        .center_x(Length::Fixed(34.0))
        .center_y(Length::Fixed(34.0))
        .style(|_theme| container::Style {
            background: Some(Background::Color(accent())),
            border: Border {
                radius: RADIUS_SM.into(),
                ..Border::default()
            },
            ..container::Style::default()
        });

    mouse_area(
        container(
            row![
                tile,
                column![
                    text(name).size(13).color(primary_text()),
                    text(format!("{count} 台主机")).size(11).color(muted_text()),
                ]
                .spacing(3),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .width(Fill)
        .padding(10)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(if targeted {
                panel_background_hover()
            } else {
                app_background()
            })),
            text_color: Some(primary_text()),
            border: Border {
                color: if targeted { accent() } else { border_color() },
                width: 1.0,
                radius: RADIUS_SM.into(),
            },
            ..container::Style::default()
        }),
    )
    .on_press(Message::GroupPressed(press))
    .on_enter(Message::ProfileDragOverGroup(enter))
    .on_move(move |_| Message::ProfileDragOverGroup(hover.clone()))
    .on_release(Message::ProfileDroppedOnGroup(release))
    .into()
}

/// A titled run of hosts, drawn the way the current layout draws hosts.
/// What a band is, which decides what it can do.
///
/// Group bands take drops on their heading and animate reorders; the ungrouped
/// band animates but is no drop target (not a place, just the absence of one);
/// recent and search are projections of other state, so their cards neither
/// animate nor accept drops — reordering a projection means nothing.
pub(crate) enum BandKind {
    Group(String),
    Ungrouped,
    Recent,
    Search,
}

/// The part of a band's appearance that is the same for every band on screen.
/// Bundled so the call sites read as "this band, drawn like the others".
pub(crate) struct BandStyle {
    pub(crate) layout: HostLayout,
    pub(crate) columns: usize,
}

/// Whether any card is still easing towards its slot.
pub(crate) fn cards_are_moving(app: &AditApp) -> bool {
    let now = Instant::now();
    app.card_slots
        .values()
        .any(|animation| animation.is_animating(now))
}

/// Point every card at the slot it now occupies, easing from wherever it was.
///
/// Called when the drop target changes and when a drag ends, so the animation
/// covers both the shuffle during a drag and the settle after one.
pub(crate) fn retarget_card_slots(app: &mut AditApp) {
    let now = Instant::now();
    let mut order: Vec<ProfileId> = grid_ordered_profiles(app)
        .into_iter()
        .map(|profile| profile.id)
        .collect();
    // Mid-drag, aim at the arrangement being PREVIEWED, not the committed one.
    // The drawn slots come from the preview; easing towards the committed order
    // parks every card between origin and target one slot off, which piles some
    // cards up and leaves a blank strip where the others were.
    if app.drag_from_grid && app.profile_drag_active {
        if let (
            Some(dragged),
            Some(ProfileDrop::Beside {
                profile_id: target,
                position,
            }),
        ) = (app.dragged_profile, &app.profile_drop)
        {
            let (target, position) = (*target, *position);
            if dragged != target {
                order.retain(|id| *id != dragged);
                if let Some(mut at) = order.iter().position(|id| *id == target) {
                    if position == ProfileDropPosition::After {
                        at += 1;
                    }
                    order.insert(at, dragged);
                }
            }
        }
    }
    // Bands are laid out independently, so a card's index within its own band is
    // what its position is drawn from — not its index in the whole catalogue.
    let mut per_group: std::collections::HashMap<String, f32> =
        std::collections::HashMap::new();
    let hosts: Vec<(ProfileId, String)> = order
        .iter()
        .filter_map(|id| {
            app.manager
                .profile(*id)
                .map(|profile| (*id, profile.group.clone()))
        })
        .collect();
    for (id, group) in hosts {
        let slot = per_group.entry(group).or_insert(0.0);
        let target = *slot;
        *slot += 1.0;
        app.card_slots
            .entry(id)
            .and_modify(|animation| animation.go_mut(target, now))
            .or_insert_with(|| {
                iced::animation::Animation::new(target)
                    .quick()
                    .easing(iced::animation::Easing::EaseOut)
            });
    }
}

/// Lay a band out as it will look once the drag lands: the travelling card is
/// pulled from where it was and put where it would go, and everything between
/// the two shifts by one.
///
/// No placeholder box. The card *is* its own preview — an empty outline is a
/// third thing on screen that exists only during a drag, and it says nothing the
/// moved card does not say by being there.
///
/// iced has no tween, but the layout is rebuilt every frame, so moving the card
/// as the target changes produces the motion with nothing animating it.
pub(crate) fn band_slots<'a>(app: &AditApp, hosts: Vec<&'a ConnectionProfile>) -> Vec<&'a ConnectionProfile> {
    let live = app.drag_from_grid && app.profile_drag_active;
    let Some(dragged) = app.dragged_profile.filter(|_| live) else {
        return hosts;
    };
    let Some(ProfileDrop::Beside {
        profile_id: target,
        position,
    }) = app.profile_drop
    else {
        return hosts;
    };

    // Only the band holding the target reorders. Another band simply loses the
    // travelling card, which is in flight and belongs in neither place yet.
    let Some(moving) = hosts.iter().copied().find(|p| p.id == dragged) else {
        return hosts.into_iter().filter(|p| p.id != dragged).collect();
    };
    let mut slots: Vec<&ConnectionProfile> =
        hosts.into_iter().filter(|p| p.id != dragged).collect();
    let Some(at) = slots.iter().position(|p| p.id == target) else {
        return slots;
    };
    let index = if position == ProfileDropPosition::After {
        at + 1
    } else {
        at
    };
    slots.insert(index, moving);
    slots
}

pub(crate) fn host_band(
    app: &AditApp,
    style: &BandStyle,
    icon: Option<&'static str>,
    kind: BandKind,
    title: String,
    hosts: Vec<&ConnectionProfile>,
    online: &std::collections::HashSet<ProfileId>,
) -> Element<'static, Message> {
    // A group heading doubles as the drop target for "put this host in that
    // group" — the same ProfileDroppedOnGroup the sidebar's folder rows use.
    let drop_group = match &kind {
        BandKind::Group(name) => Some(name.clone()),
        _ => None,
    };
    let animated = matches!(kind, BandKind::Group(_) | BandKind::Ungrouped);
    let count = hosts.len();
    let up = hosts
        .iter()
        .filter(|profile| online.contains(&profile.id))
        .count();
    let (layout, columns) = (style.layout, style.columns);
    let slots = band_slots(app, hosts);
    let entries: Element<'static, Message> = match layout {
        HostLayout::Grid => {
            // The same arithmetic wrap_cards lays them out with, so the midpoint
            // a card tests against is the midpoint it actually has.
            let card_width =
                ((app.window_width - app.sidebar_width - 56.0) / columns as f32 - 8.0).max(80.0);
            if animated {
                // Absolute positions in a stack, not flowing cells. A flowing
                // layout cannot draw a card outside its own cell, so an animated
                // offset either collapsed the card to nothing or pushed it out
                // of sight — the blank strips mid-drag. Here every card is
                // placed at its interpolated position and simply glides, row
                // wraps included.
                let n = slots.len();
                let step_x = card_width + 8.0;
                let step_y = HOST_CARD_HEIGHT + 8.0;
                let rows = n.div_ceil(columns.max(1));
                let band_height = (rows as f32 * step_y - 8.0).max(0.0);
                let place = |slot: usize| -> (f32, f32) {
                    let slot = slot.min(n.saturating_sub(1));
                    let row = (slot / columns.max(1)) as f32;
                    let column = (slot % columns.max(1)) as f32;
                    (column * step_x, row * step_y)
                };
                let now = Instant::now();
                // The base layer sizes the stack. iced's Stack takes its size
                // from its first child and lays the rest out inside it — without
                // this spacer the first CARD becomes the base, every later layer
                // is confined to one card's bounds, and the positioning spacers
                // push them all out of sight (a band of 21 rendering one card).
                let mut layers = iced::widget::stack![
                    Space::new().width(Fill).height(Length::Fixed(band_height))
                ];
                for (index, profile) in slots.iter().enumerate() {
                    let drawn = app
                        .card_slots
                        .get(&profile.id)
                        .map(|animation| animation.interpolate_with(|slot| slot, now))
                        .unwrap_or(index as f32)
                        .clamp(0.0, n.saturating_sub(1) as f32);
                    // Interpolate between the two neighbouring slot positions,
                    // so a card crossing a row boundary glides diagonally
                    // instead of teleporting to the far edge.
                    let base = drawn.floor();
                    let frac = drawn - base;
                    let (x0, y0) = place(base as usize);
                    let (x1, y1) = place(base as usize + 1);
                    let (x, y) = (x0 + (x1 - x0) * frac, y0 + (y1 - y0) * frac);
                    let card =
                        host_card(app, profile, online.contains(&profile.id), card_width);
                    layers = layers.push(
                        column![
                            Space::new().height(Length::Fixed(y)),
                            row![
                                Space::new().width(Length::Fixed(x)),
                                container(card).width(Length::Fixed(card_width)),
                            ],
                        ]
                        .width(Fill)
                        .height(Fill),
                    );
                }
                container(layers)
                    .width(Fill)
                    .height(Length::Fixed(band_height))
                    .into()
            } else {
                wrap_cards(
                    slots
                        .into_iter()
                        .map(|profile| {
                            host_card(app, profile, online.contains(&profile.id), card_width)
                        })
                        .collect(),
                    columns,
                )
            }
        }
        _ => slots
            .into_iter()
            .fold(column![].spacing(2), |rows, profile| {
                rows.push(host_row(app, profile, 0.0, online.contains(&profile.id)))
            })
            .into(),
    };
    column![host_section_header(icon, title, count, up, drop_group), entries]
        .spacing(10)
        .into()
}

/// The host manager: search at the top, then one band per group.
/// The card that follows the cursor during a grid drag — the same pattern as
/// the SFTP pane's `drag_ghost`, positioned with leading spacers from
/// pane-relative coordinates. Offset from the hotspot so the pointer keeps
/// feeding the card underneath, not the ghost.
pub(crate) fn host_drag_ghost(
    app: &AditApp,
    profile: &ConnectionProfile,
    position: Point,
) -> Element<'static, Message> {
    let (glyph, tile_color) = host_icon(app, profile);
    let tile = container(text(glyph).size(13).color(Color::WHITE))
        .width(Length::Fixed(26.0))
        .height(Length::Fixed(26.0))
        .center_x(Length::Fixed(26.0))
        .center_y(Length::Fixed(26.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(tile_color)),
            border: Border {
                radius: RADIUS_SM.into(),
                ..Border::default()
            },
            ..container::Style::default()
        });
    column![
        Space::new().height(Length::Fixed((position.y + 14.0).max(0.0))),
        row![
            Space::new().width(Length::Fixed((position.x + 16.0).max(0.0))),
            container(
                row![tile, text(profile.name.clone()).size(12).color(primary_text())]
                    .spacing(8)
                    .align_y(Alignment::Center),
            )
            .padding([6, 12])
            .style(|_theme| drag_ghost_style()),
        ],
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

/// The grid's ordering: the saved arrangement first, then anything it has never
/// heard of, in the sidebar's order.
///
/// Ids that no longer resolve are simply skipped rather than pruned here — this
/// runs per render, and the stored list self-corrects on the next reorder.
pub(crate) fn grid_ordered_profiles(app: &AditApp) -> Vec<ConnectionProfile> {
    let mut profiles = app.manager.profiles().to_vec();
    profiles.sort_by(profile_sidebar_order);
    let mut ordered: Vec<ConnectionProfile> = Vec::with_capacity(profiles.len());
    for id in &app.grid_order {
        if let Some(at) = profiles.iter().position(|profile| profile.id == *id) {
            ordered.push(profiles.remove(at));
        }
    }
    ordered.extend(profiles);
    ordered
}

/// Commit a grid-originated drop: `source` lands beside `target` in the grid's
/// own order, and nothing else moves — the tree never hears about it.
pub(crate) fn reorder_grid(app: &mut AditApp, source: ProfileId, target: ProfileId, position: ProfileDropPosition) {
    let mut order: Vec<ProfileId> = grid_ordered_profiles(app)
        .iter()
        .map(|profile| profile.id)
        .collect();
    order.retain(|id| *id != source);
    let Some(mut at) = order.iter().position(|id| *id == target) else {
        return;
    };
    if position == ProfileDropPosition::After {
        at += 1;
    }
    order.insert(at, source);
    app.grid_order = order;
    app.notice = String::from("已调整网格排序");
}

pub(crate) fn hosts_view(app: &AditApp) -> Element<'_, Message> {
    let filter = app.session_filter.trim().to_ascii_lowercase();
    let profiles = grid_ordered_profiles(app);
    let matches = |profile: &ConnectionProfile| {
        filter.is_empty()
            || profile.name.to_ascii_lowercase().contains(&filter)
            || profile.host.to_ascii_lowercase().contains(&filter)
            || profile.username.to_ascii_lowercase().contains(&filter)
    };

    let search = text_input("搜索主机、地址或用户名…", &app.session_filter)
        .id(session_filter_id())
        .on_input(Message::SessionFilterChanged)
        .padding(9)
        .size(13);

    let layout = app.host_layout;
    // Grid and list. Tree was a third option that reproduced the sidebar, and
    // the sidebar does it better because doing it is all the sidebar does.
    let switcher = row![
        host_layout_button(layout, HostLayout::Grid, "网格"),
        host_layout_button(layout, HostLayout::List, "列表"),
    ]
    .spacing(6);

    let style = BandStyle {
        layout,
        columns: host_columns(app),
    };
    let online = connected_profiles(app);
    let searching = !filter.is_empty();
    let matching: Vec<&ConnectionProfile> = profiles
        .iter()
        .filter(|profile| matches(profile))
        .collect();

    let mut body = column![].spacing(match layout {
        HostLayout::Grid => 18,
        HostLayout::List | HostLayout::Tree => 10,
    });

    // One band per group, in the order and grouping the sidebar tree uses, so
    // the two views cannot disagree about where a host lives. Bands rather than
    // cards you open: the tree is right there for drilling in, and the reason to
    // have a grid at all is seeing hosts without drilling.
    if matching.is_empty() {
        body = body.push(
            text(if searching {
                "没有匹配的主机"
            } else {
                "还没有会话配置"
            })
            .size(13)
            .color(muted_text()),
        );
    } else if searching {
        // A search is asked of everything, so its answer is one list.
        body = body.push(host_band(
            app,
            &style,
            None,
            BandKind::Search,
            String::from("搜索结果"),
            matching,
            &online,
        ));
    } else {
        // Reaching for a recent host is what you do instead of looking, so it
        // disappears the moment you start looking.
        let recent: Vec<&ConnectionProfile> = app
            .recent_hosts
            .iter()
            .filter_map(|id| profiles.iter().find(|profile| profile.id == *id))
            .collect();
        if !recent.is_empty() {
            body = body.push(host_band(
                app,
                &style,
                Some("◷"),
                BandKind::Recent,
                String::from("最近连接"),
                recent,
                &online,
            ));
        }

        // Counted by name, not by walking runs of neighbours: nothing promises a
        // group's hosts are adjacent after sorting, and assuming they were turned
        // five groups into dozens on a real catalogue.
        let mut bands: Vec<(String, Vec<&ConnectionProfile>)> = Vec::new();
        let mut loose: Vec<&ConnectionProfile> = Vec::new();
        for profile in matching {
            if profile.group.trim().is_empty() {
                loose.push(profile);
                continue;
            }
            match bands.iter_mut().find(|(name, _)| name == &profile.group) {
                Some((_, hosts)) => hosts.push(profile),
                None => bands.push((profile.group.clone(), vec![profile])),
            }
        }
        // Groups are cards, folded. A band shows that a group exists and how big
        // it is; spilling every group across the screen is what the sidebar tree
        // is for, and what a grid of 151 hosts becomes unreadable doing.
        if !bands.is_empty() {
            let cards = bands
                .iter()
                .map(|(group, hosts)| {
                    group_card(
                        group.clone(),
                        hosts.len(),
                        !app.collapsed_groups.contains(group),
                        app.profile_drop == Some(ProfileDrop::IntoGroup(group.clone())),
                    )
                })
                .collect();
            body = body.push(
                column![
                    host_section_header(None, String::from("分组"), bands.len(), 0, None),
                    wrap_cards(cards, style.columns)
                ]
                .spacing(10),
            );
        }

        // An expanded group opens beneath the cards. `collapsed_groups` is the
        // sidebar's own set, so folding a group here folds it there — which is
        // the only reading of "aligned with the session manager" that survives
        // someone using both.
        for (group, hosts) in bands {
            if app.collapsed_groups.contains(&group) {
                continue;
            }
            let kind = BandKind::Group(group.clone());
            body = body.push(host_band(
                app, &style, None, kind, group, hosts, &online,
            ));
        }
        // Named 主机 rather than "ungrouped": for anyone who never made a group
        // that band is simply the list. Omitted when everything is grouped.
        if !loose.is_empty() {
            body = body.push(host_band(
                app,
                &style,
                None,
                BandKind::Ungrouped,
                String::from("主机"),
                loose,
                &online,
            ));
        }
    }

    let pane: Element<'_, Message> = container(
        column![
            // Search, then the shapes, then the one action. No tag filter or
            // multi-select alongside them: the reference client has both, Adit
            // has neither tags nor a bulk operation to select for, and a control
            // that opens onto nothing is worse than its absence.
            row![
                search,
                switcher,
                button(text("+ 新建主机").size(12))
                    .padding([6, 14])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::NewProfileDraft),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            scrollable(body).height(Fill),
        ]
        .spacing(16),
    )
    .width(Fill)
    .height(Fill)
    .padding(20)
    .into();

    // Track the cursor for the drag ghost. Tracked always rather than only
    // mid-drag, so the ghost's first frame appears where the drag began instead
    // of wherever the ghost last was.
    let pane: Element<'_, Message> =
        mouse_area(pane).on_move(Message::HostsCursorMoved).into();

    // The ghost rides above the pane while a card drag is live. Nothing in it
    // handles events, so the cards underneath keep receiving enter/move — the
    // ghost is presentation, and the drop logic never touches it.
    if app.drag_from_grid && app.profile_drag_active {
        if let (Some(id), Some(position)) = (app.dragged_profile, app.hosts_cursor) {
            if let Some(profile) = app.manager.profile(id).cloned() {
                return stack![pane, host_drag_ghost(app, &profile, position)]
                    .width(Fill)
                    .height(Fill)
                    .into();
            }
        }
    }
    pane
}
