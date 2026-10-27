use iced::{widget::button, Color};
use std::sync::OnceLock;

pub(crate) fn base_theme(dark: bool) -> iced::Theme {
    static LIGHT: OnceLock<iced::Theme> = OnceLock::new();
    static DARK: OnceLock<iced::Theme> = OnceLock::new();
    let cache = if dark { &DARK } else { &LIGHT };
    cache
        .get_or_init(|| {
            iced::Theme::custom(
                if dark { "Adit Dark" } else { "Adit Light" },
                iced::theme::Palette {
                    background: if dark {
                        Color::from_rgb8(24, 28, 38)
                    } else {
                        Color::WHITE
                    },
                    text: if dark {
                        Color::from_rgb8(226, 232, 240)
                    } else {
                        Color::from_rgb8(31, 43, 58)
                    },
                    primary: action_fill(button::Status::Active),
                    success: Color::from_rgb8(16, 138, 122),
                    danger: Color::from_rgb8(181, 49, 59),
                    warning: Color::from_rgb8(181, 119, 34),
                },
            )
        })
        .clone()
}

/// Solid buttons carry small white labels. The brighter link/focus accent is
/// intended for dark text surfaces and does not provide enough label contrast.
pub(crate) fn action_fill(status: button::Status) -> Color {
    match status {
        button::Status::Active => Color::from_rgb8(22, 105, 192),
        button::Status::Hovered => Color::from_rgb8(40, 100, 174),
        button::Status::Pressed => Color::from_rgb8(38, 83, 151),
        button::Status::Disabled => Color::from_rgb8(206, 213, 224),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color) -> f32 {
        let linear = |value: f32| {
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }

    #[test]
    fn enabled_action_labels_keep_normal_text_contrast_in_every_pointer_state() {
        for status in [
            button::Status::Active,
            button::Status::Hovered,
            button::Status::Pressed,
        ] {
            let ratio = 1.05 / (luminance(action_fill(status)) + 0.05);
            assert!(ratio >= 4.5, "{status:?}: white label contrast is {ratio}");
        }
    }
}
