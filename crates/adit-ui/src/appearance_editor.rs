use super::*;
use adit_domain::{TerminalAppearance, TerminalPalette};
use iced::widget::column;

#[derive(Debug, Clone)]
pub enum AppearanceEdit {
    Font(String),
    Size(String),
    Scheme(String),
    Background(String),
    Custom(bool),
    Palette(usize, String),
    Reset,
}

pub(crate) struct AppearanceForm {
    font: String,
    size: String,
    scheme: String,
    background: String,
    custom: bool,
    colors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppearanceChoice {
    value: Option<String>,
    label: String,
}

impl AppearanceChoice {
    fn named(value: &str) -> Self {
        Self {
            value: Some(value.to_owned()),
            label: t(value).to_owned(),
        }
    }
}

impl std::fmt::Display for AppearanceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

impl AppearanceForm {
    pub(crate) fn new(value: &TerminalAppearance) -> Self {
        let palette = value.custom_palette.unwrap_or_else(|| {
            TerminalStyle::resolve(
                "",
                13.0,
                value.color_scheme.as_deref().unwrap_or(""),
                &TerminalAppearance::default(),
                Environment::None,
            )
            .palette
        });
        Self {
            font: value.font_family.clone().unwrap_or_default(),
            size: value.font_size.map(|v| v.to_string()).unwrap_or_default(),
            scheme: value.color_scheme.clone().unwrap_or_default(),
            background: value.background.map(hex).unwrap_or_default(),
            custom: value.custom_palette.is_some(),
            colors: [palette.background, palette.foreground, palette.selection]
                .into_iter()
                .chain(palette.ansi)
                .map(hex)
                .collect(),
        }
    }

    pub(crate) fn update(&mut self, edit: AppearanceEdit) {
        match edit {
            AppearanceEdit::Font(v) => self.font = v,
            AppearanceEdit::Size(v) => self.size = v,
            AppearanceEdit::Scheme(v) => self.scheme = v,
            AppearanceEdit::Background(v) => self.background = v,
            AppearanceEdit::Custom(v) => self.custom = v,
            AppearanceEdit::Palette(index, v) => {
                if let Some(color) = self.colors.get_mut(index) {
                    *color = v;
                }
            }
            AppearanceEdit::Reset => *self = Self::new(&TerminalAppearance::default()),
        }
    }

    pub(crate) fn value(&self) -> Result<TerminalAppearance, String> {
        let font_size = if self.size.trim().is_empty() {
            None
        } else {
            Some(
                self.size
                    .trim()
                    .parse::<u8>()
                    .map_err(|_| t("字号必须为 9–28 的整数；留空继承全局"))?,
            )
        };
        let custom_palette = if self.custom {
            let values: Vec<_> = self
                .colors
                .iter()
                .map(|v| parse_color(v))
                .collect::<Result<_, _>>()?;
            Some(TerminalPalette {
                background: values[0],
                foreground: values[1],
                selection: values[2],
                ansi: std::array::from_fn(|i| values[i + 3]),
            })
        } else {
            None
        };
        Ok(TerminalAppearance {
            font_family: (!self.font.is_empty()).then(|| self.font.clone()),
            font_size,
            color_scheme: (!self.scheme.is_empty()).then(|| self.scheme.clone()),
            custom_palette,
            background: if self.background.trim().is_empty() {
                None
            } else {
                Some(parse_color(&self.background)?)
            },
        })
    }
}

fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

fn parse_color(value: &str) -> Result<[u8; 3], String> {
    let value = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if value.len() != 6 || !value.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(t("颜色请填写 #RRGGBB，例如 #201818").into());
    }
    Ok(std::array::from_fn(|i| {
        u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).unwrap()
    }))
}

fn message(edit: AppearanceEdit) -> Message {
    Message::ProfileOptionChanged(ProfileOptionEdit::Appearance(edit))
}

pub(crate) fn appearance_editor<'a>(
    form: &'a AppearanceForm,
    app: &AditApp,
) -> Element<'a, Message> {
    let inherit = AppearanceChoice {
        value: None,
        label: t("继承全局").to_owned(),
    };
    let fonts: Vec<_> = std::iter::once(inherit.clone())
        .chain(
            FONT_PRESETS
                .iter()
                .map(|(name, _)| AppearanceChoice::named(name)),
        )
        .collect();
    let schemes: Vec<_> = std::iter::once(inherit.clone())
        .chain(
            COLOR_SCHEMES
                .iter()
                .map(|scheme| AppearanceChoice::named(scheme.name)),
        )
        .collect();
    let inherited_font = if form.font.is_empty() {
        inherit.clone()
    } else {
        AppearanceChoice::named(&form.font)
    };
    let inherited_scheme = if form.scheme.is_empty() {
        inherit
    } else {
        AppearanceChoice::named(&form.scheme)
    };
    let mut body = column![
        row![
            text(t("此配置的终端外观")).size(12).width(Fill),
            button(text(t("全部继承全局")).size(11))
                .style(|_theme, status| secondary_button_style(status))
                .padding([4, 8])
                .on_press(message(AppearanceEdit::Reset)),
        ]
        .align_y(Alignment::Center),
        row![
            dialog_field(
                "字体",
                iced::widget::pick_list(fonts, Some(inherited_font), |v| {
                    message(AppearanceEdit::Font(v.value.unwrap_or_default()))
                })
                .text_size(13)
                .padding([5, 8])
                .style(pick_list_style)
                .width(Fill)
                .into()
            ),
            dialog_field(
                "字号（留空继承）",
                text_input("9–28", &form.size)
                    .size(13)
                    .on_input(|v| message(AppearanceEdit::Size(v)))
                    .padding([5, 8])
                    .style(text_input_style)
                    .into()
            ),
        ]
        .spacing(8),
        dialog_field(
            "配色方案",
            iced::widget::pick_list(schemes, Some(inherited_scheme), |v| {
                message(AppearanceEdit::Scheme(v.value.unwrap_or_default()))
            })
            .text_size(13)
            .padding([5, 8])
            .style(pick_list_style)
            .width(Fill)
            .into()
        ),
        dialog_field(
            "窗格背景（留空使用配色；PROD 自动提示）",
            text_input("#RRGGBB", &form.background)
                .on_input(|v| message(AppearanceEdit::Background(v)))
                .padding([5, 8])
                .style(text_input_style)
                .into()
        ),
        checkbox(form.custom)
            .label(t("自定义配色（随此配置保存，可通过配置目录导入导出）"))
            .on_toggle(|v| message(AppearanceEdit::Custom(v))),
    ]
    .spacing(8);
    if form.custom {
        let mut basics = row![].spacing(8);
        for (index, label) in ["背景", "文字", "选择"].into_iter().enumerate() {
            basics = basics.push(dialog_field(label, color_input(form, index)));
        }
        body = body
            .push(basics)
            .push(text(t("ANSI 标准色 / 亮色")).size(11).color(muted_text()));
        for first in [3, 7, 11, 15] {
            let mut colors = row![].spacing(8);
            for index in first..first + 4 {
                colors = colors.push(
                    column![
                        text(format!("{}", index - 3)).size(10).color(muted_text()),
                        color_input(form, index),
                    ]
                    .spacing(3)
                    .width(Fill),
                );
            }
            body = body.push(colors);
        }
    }
    if let Ok(appearance) = form.value() {
        let style = TerminalStyle::resolve(
            &app.font_family,
            app.font_size,
            &app.color_scheme,
            &appearance,
            app.profile_environment,
        );
        body = body.push(
            container(
                text("adit@host:~$  AaBbCc 0123 中文")
                    .font(style.font)
                    .size(style.metrics.font_size)
                    .color(style.foreground()),
            )
            .padding(10)
            .width(Fill)
            .style(move |_| container::Style {
                background: Some(style.background().into()),
                ..Default::default()
            }),
        );
    }
    body.into()
}

fn color_input(form: &AppearanceForm, index: usize) -> Element<'_, Message> {
    text_input("#RRGGBB", &form.colors[index])
        .on_input(move |v| message(AppearanceEdit::Palette(index, v)))
        .padding([5, 8])
        .size(12)
        .style(text_input_style)
        .width(Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translated_choice_labels_never_replace_persisted_font_keys_or_inheritance() {
        let mut form = AppearanceForm::new(&TerminalAppearance {
            font_family: Some("Consolas".into()),
            ..Default::default()
        });
        let inherit = AppearanceChoice {
            value: None,
            label: "Inherit global".into(),
        };
        form.update(AppearanceEdit::Font(inherit.value.unwrap_or_default()));
        assert_eq!(form.value().unwrap().font_family, None);
        let scheme = AppearanceChoice {
            value: Some("默认".into()),
            label: "Default".into(),
        };
        form.update(AppearanceEdit::Scheme(scheme.value.unwrap_or_default()));
        assert_eq!(form.value().unwrap().color_scheme.as_deref(), Some("默认"));
    }

    #[test]
    fn custom_scheme_roundtrips_and_bad_colors_or_size_do_not_silently_fall_back() {
        let mut form = AppearanceForm::new(&TerminalAppearance::default());
        assert_eq!(form.value().unwrap(), TerminalAppearance::default());
        form.update(AppearanceEdit::Custom(true));
        form.update(AppearanceEdit::Palette(4, "#123456".into()));
        form.update(AppearanceEdit::Background("#201010".into()));
        let value = form.value().unwrap();
        assert_eq!(value.custom_palette.unwrap().ansi[1], [18, 52, 86]);
        assert_eq!(AppearanceForm::new(&value).value().unwrap(), value);
        form.update(AppearanceEdit::Palette(4, "bad".into()));
        assert!(form.value().is_err());
        form.update(AppearanceEdit::Reset);
        form.update(AppearanceEdit::Size("invalid".into()));
        assert!(form.value().is_err());
        form.update(AppearanceEdit::Reset);
        assert_eq!(form.value().unwrap(), TerminalAppearance::default());
    }
}
