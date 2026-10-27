use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Empty overrides preserve the behavior of catalogs saved before these options.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionOptions {
    pub environment: BTreeMap<String, String>,
    pub encoding: TerminalEncoding,
    pub rdp_quality: Option<DesktopQuality>,
    pub reconnect: Option<ReconnectPolicy>,
    pub logging: Option<LogPolicy>,
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
}
