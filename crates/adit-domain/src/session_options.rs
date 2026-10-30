use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Empty overrides preserve the behavior of catalogs saved before these options.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionOptions {
    pub environment: BTreeMap<String, String>,
    pub encoding: TerminalEncoding,
    pub rdp_quality: Option<DesktopQuality>,
    pub rdp_monitors: Option<Vec<DesktopMonitor>>,
    pub rdp_selected_monitor: Option<usize>,
    pub reconnect: Option<ReconnectPolicy>,
    pub logging: Option<LogPolicy>,
    pub triggers: Vec<OutputTrigger>,
    pub terminal_appearance: TerminalAppearance,
}

/// Each absent field follows the current application setting.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalAppearance {
    pub font_family: Option<String>,
    pub font_size: Option<u8>,
    pub color_scheme: Option<String>,
    pub custom_palette: Option<TerminalPalette>,
    pub background: Option<[u8; 3]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalPalette {
    pub background: [u8; 3],
    pub foreground: [u8; 3],
    pub selection: [u8; 3],
    pub ansi: [[u8; 3]; 16],
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputTrigger {
    pub enabled: bool,
    pub pattern: String,
    pub action: TriggerAction,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerAction {
    #[default]
    Notify,
    SendLine(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopMonitor {
    pub left: i32,
    pub top: i32,
    pub width: u16,
    pub height: u16,
    pub primary: bool,
    pub scale_percent: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalEncoding {
    #[default]
    Utf8,
    Gbk,
    Big5,
    ShiftJis,
    Windows1252,
}

impl TerminalEncoding {
    pub const ALL: [Self; 5] = [
        Self::Utf8,
        Self::Gbk,
        Self::Big5,
        Self::ShiftJis,
        Self::Windows1252,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Gbk => "GBK",
            Self::Big5 => "Big5",
            Self::ShiftJis => "Shift-JIS",
            Self::Windows1252 => "Windows-1252",
        }
    }
}

impl std::fmt::Display for TerminalEncoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopQuality {
    High,
    Balanced,
    Speed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconnectPolicy {
    pub enabled: bool,
    pub max_attempts: u32,
    pub initial_delay_secs: u64,
    pub max_delay_secs: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: 10,
            initial_delay_secs: 1,
            max_delay_secs: 30,
        }
    }
}

impl ReconnectPolicy {
    pub fn delay_secs(&self, attempt: u32) -> u64 {
        self.initial_delay_secs
            .saturating_mul(1_u64.checked_shl(attempt.min(63)).unwrap_or(u64::MAX))
            .min(self.max_delay_secs)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogPolicy {
    pub auto_start: bool,
    /// Empty paths and patterns inherit the application's configured defaults.
    pub directory: String,
    pub filename_pattern: String,
    pub plaintext: bool,
    /// Explicitly opt-in; never inferred from output logging or auto-start.
    pub record_input: bool,
}

impl SessionOptions {
    pub fn validate(&self) -> Result<(), String> {
        let appearance = &self.terminal_appearance;
        if appearance
            .font_size
            .is_some_and(|size| !(9..=28).contains(&size))
            || [&appearance.font_family, &appearance.color_scheme]
                .into_iter()
                .flatten()
                .any(|value| {
                    value.trim().is_empty()
                        || value.len() > 128
                        || value.chars().any(char::is_control)
                })
        {
            return Err("终端外观配置无效".into());
        }
        if self.triggers.len() > 16 {
            return Err("触发器最多为 16 项".into());
        }
        for trigger in &self.triggers {
            if trigger.pattern.len() > 512
                || trigger.pattern.contains('\0')
                || (trigger.enabled && trigger.pattern.is_empty())
            {
                return Err("启用的触发器必须填写不超过 512 字节的匹配表达式".into());
            }
            if let TriggerAction::SendLine(text) = &trigger.action {
                if text.len() > 4096 || text.contains(['\0', '\r', '\n']) {
                    return Err("触发器发送的文本不能超过 4096 字节或包含换行".into());
                }
            }
        }
        if let Some(monitors) = &self.rdp_monitors {
            if monitors.is_empty()
                || monitors.len() > 16
                || monitors.iter().filter(|m| m.primary).count() != 1
                || self
                    .rdp_selected_monitor
                    .is_some_and(|i| i >= monitors.len())
                || monitors.iter().any(|m| {
                    !(200..=8192).contains(&m.width)
                        || m.width & 1 != 0
                        || !(200..=8192).contains(&m.height)
                        || !(100..=500).contains(&m.scale_percent)
                        || !(-8192..=8192).contains(&m.left)
                        || !(-8192..=8192).contains(&m.top)
                        || (m.primary && (m.left != 0 || m.top != 0))
                })
            {
                return Err("远程显示器配置无效".into());
            }
            for (index, m) in monitors.iter().enumerate() {
                for other in &monitors[..index] {
                    if m.left < other.left + i32::from(other.width)
                        && other.left < m.left + i32::from(m.width)
                        && m.top < other.top + i32::from(other.height)
                        && other.top < m.top + i32::from(m.height)
                    {
                        return Err("远程显示器不能重叠".into());
                    }
                }
            }
            let left = monitors.iter().map(|m| m.left).min().unwrap_or(0);
            let top = monitors.iter().map(|m| m.top).min().unwrap_or(0);
            let right = monitors
                .iter()
                .map(|m| m.left + i32::from(m.width))
                .max()
                .unwrap_or(0);
            let bottom = monitors
                .iter()
                .map(|m| m.top + i32::from(m.height))
                .max()
                .unwrap_or(0);
            if right - left > 8192 || bottom - top > 8192 {
                return Err("远程桌面宽高不能超过 8192 像素".into());
            }
            let mut connected = vec![false; monitors.len()];
            let primary = monitors
                .iter()
                .position(|m| m.primary)
                .ok_or("缺少主显示器")?;
            connected[primary] = true;
            loop {
                let mut changed = false;
                for (index, a) in monitors.iter().enumerate() {
                    if connected[index] {
                        continue;
                    }
                    if monitors.iter().enumerate().any(|(other_index, b)| {
                        connected[other_index]
                            && (((a.left + i32::from(a.width) == b.left
                                || b.left + i32::from(b.width) == a.left)
                                && a.top <= b.top + i32::from(b.height)
                                && b.top <= a.top + i32::from(a.height))
                                || ((a.top + i32::from(a.height) == b.top
                                    || b.top + i32::from(b.height) == a.top)
                                    && a.left <= b.left + i32::from(b.width)
                                    && b.left <= a.left + i32::from(a.width)))
                    }) {
                        connected[index] = true;
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
            if connected.contains(&false) {
                return Err("远程显示器之间必须相邻".into());
            }
        } else if self.rdp_selected_monitor.is_some() {
            return Err("请先配置远程显示器布局".into());
        }
        if self.environment.len() > 64 {
            return Err("环境变量最多为 64 项".into());
        }
        for (name, value) in &self.environment {
            let mut chars = name.chars();
            let valid_start = chars
                .next()
                .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');
            if !valid_start
                || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                || name.len() > 256
                || value.len() > 16 * 1024
                || value.contains('\0')
            {
                return Err(format!("环境变量无效：{name}"));
            }
        }
        if let Some(policy) = &self.reconnect {
            if !(1..=100).contains(&policy.max_attempts)
                || !(1..=3600).contains(&policy.initial_delay_secs)
                || !(policy.initial_delay_secs..=3600).contains(&policy.max_delay_secs)
            {
                return Err(
                    "重连次数应为 1–100，间隔应为 1–3600 秒，且最大间隔不能小于初始间隔".into(),
                );
            }
        }
        if let Some(policy) = &self.logging {
            if policy.directory.contains('\0')
                || policy.filename_pattern.contains(['\0', '/', '\\'])
                || policy.filename_pattern.len() > 256
            {
                return Err("日志目录或文件名模式无效，文件名不能包含路径分隔符".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_defaults_and_validation_preserve_legacy_inheritance() {
        let options: SessionOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(options.terminal_appearance, TerminalAppearance::default());
        let mut options = options;
        options.terminal_appearance.font_size = Some(8);
        assert!(options.validate().is_err());
        options.terminal_appearance.font_size = Some(20);
        assert!(options.validate().is_ok());
        let encoded = serde_json::to_string(&options).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionOptions>(&encoded).unwrap(),
            options
        );
    }

    #[test]
    fn invalid_environment_and_unbounded_retry_are_rejected() {
        let mut options = SessionOptions::default();
        options
            .environment
            .insert("LD_PRELOAD;echo".into(), "ignored".into());
        assert!(options.validate().is_err());
        options.environment.clear();
        options
            .environment
            .insert("LANG".into(), "zh_CN.UTF-8".into());
        assert!(options.validate().is_ok());
        options.reconnect = Some(ReconnectPolicy {
            max_attempts: 0,
            ..Default::default()
        });
        assert!(options.validate().is_err());
    }

    #[test]
    fn retry_backoff_saturates_at_the_configured_cap() {
        let policy = ReconnectPolicy {
            enabled: true,
            max_attempts: 100,
            initial_delay_secs: 3,
            max_delay_secs: 20,
        };
        assert_eq!(policy.delay_secs(0), 3);
        assert_eq!(policy.delay_secs(1), 6);
        assert_eq!(policy.delay_secs(3), 20);
        assert_eq!(policy.delay_secs(u32::MAX), 20);
    }

    #[test]
    fn input_recording_is_never_enabled_by_default() {
        assert!(!LogPolicy::default().record_input);
        let options = SessionOptions {
            logging: Some(LogPolicy {
                filename_pattern: "../capture.log".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(options.validate().is_err());
    }

    #[test]
    fn monitor_preferences_reject_bad_selection_overlap_and_disconnected_layouts() {
        let screen = DesktopMonitor {
            left: 0,
            top: 0,
            width: 800,
            height: 600,
            primary: true,
            scale_percent: 100,
        };
        let mut options = SessionOptions {
            rdp_monitors: Some(vec![screen.clone()]),
            rdp_selected_monitor: Some(1),
            ..Default::default()
        };
        assert!(options.validate().is_err());
        options.rdp_selected_monitor = None;
        options.rdp_monitors.as_mut().unwrap().push(DesktopMonitor {
            left: 800,
            primary: false,
            ..screen.clone()
        });
        assert!(options.validate().is_ok());
        options.rdp_monitors.as_mut().unwrap()[1].left = 799;
        assert!(options.validate().is_err());
        options.rdp_monitors.as_mut().unwrap()[1].left = 801;
        assert!(options.validate().is_err());
    }

    #[test]
    fn review97_corner_touching_layout_validates_and_round_trips() {
        let monitors = vec![
            DesktopMonitor {
                left: 0,
                top: 0,
                width: 200,
                height: 200,
                primary: true,
                scale_percent: 100,
            },
            DesktopMonitor {
                left: 200,
                top: 200,
                width: 200,
                height: 200,
                primary: false,
                scale_percent: 100,
            },
        ];
        let options = SessionOptions {
            rdp_monitors: Some(monitors.clone()),
            rdp_selected_monitor: Some(1),
            ..Default::default()
        };
        assert!(
            options.validate().is_ok(),
            "corner-adjacent topology rejected before persistence"
        );
        let json = serde_json::to_string(&options).unwrap();
        let restored: SessionOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.rdp_monitors, Some(monitors));
        assert!(restored.validate().is_ok());
    }
}
