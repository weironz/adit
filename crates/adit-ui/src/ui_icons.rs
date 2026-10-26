use super::*;

#[derive(Clone, Copy)]
pub(crate) enum UiIcon {
    Hosts,
    Terminal,
    Tasks,
    Key,
    Code,
    Forward,
    Settings,
    Folder,
    Search,
    Grid,
    List,
    Tree,
    Plus,
}

/// One stroke grid keeps toolbar and navigation icons aligned across fonts.
pub(crate) fn ui_icon(icon: UiIcon, size: f32, color: Color) -> Element<'static, Message> {
    let path = match icon {
        UiIcon::Hosts => r#"<rect x="4" y="3" width="16" height="7" rx="2"/><rect x="4" y="14" width="16" height="7" rx="2"/><path d="M8 6.5h.01M8 17.5h.01M12 6.5h5M12 17.5h5"/>"#,
        UiIcon::Terminal => r#"<rect x="3" y="4" width="18" height="16" rx="3"/><path d="m7 9 3 3-3 3m6 0h4"/>"#,
        UiIcon::Tasks => r#"<rect x="3" y="4" width="18" height="16" rx="3"/><path d="M12 4v16m0-8h9"/>"#,
        UiIcon::Key => r#"<circle cx="8" cy="9" r="4"/><path d="m11 12 8 8m-4-4 3-3m-1 5 3-3"/>"#,
        UiIcon::Code => r#"<path d="m8 6-6 6 6 6m8-12 6 6-6 6m-3-15-2 18"/>"#,
        UiIcon::Forward => r#"<path d="M3 7h17m-4-4 4 4-4 4M21 17H4m4-4-4 4 4 4"/>"#,
        UiIcon::Settings => r#"<path d="M4 6h16M4 12h16M4 18h16"/><circle cx="8" cy="6" r="2"/><circle cx="16" cy="12" r="2"/><circle cx="10" cy="18" r="2"/>"#,
        UiIcon::Folder => r#"<path d="M3 7V5a2 2 0 0 1 2-2h5l2 3h7a2 2 0 0 1 2 2v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z"/>"#,
        UiIcon::Search => r#"<circle cx="10.5" cy="10.5" r="6.5"/><path d="m16 16 5 5"/>"#,
        UiIcon::Grid => r#"<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/>"#,
        UiIcon::List => r#"<path d="M9 5h12M9 12h12M9 19h12M3 5h1M3 12h1M3 19h1"/>"#,
        UiIcon::Tree => r#"<path d="M5 3v14h5M5 7h5"/><rect x="10" y="4" width="10" height="6" rx="1"/><rect x="10" y="14" width="10" height="6" rx="1"/>"#,
        UiIcon::Plus => r#"<path d="M12 4v16M4 12h16"/>"#,
    };
    let color = format!(
        "#{:02x}{:02x}{:02x}",
        (color.r * 255.0) as u8,
        (color.g * 255.0) as u8,
        (color.b * 255.0) as u8,
    );
    let bytes = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="{color}" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">{path}</svg>"#
    );
    iced::widget::svg(iced::widget::svg::Handle::from_memory(bytes.into_bytes()))
        .width(size)
        .height(size)
        .into()
}
