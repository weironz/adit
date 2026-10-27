use serde::{Deserialize, Serialize};
use std::{fmt, path::Path};

/// An explicitly enabled local executable, never a shell command line.
/// Arguments are local configuration and may be sensitive: Debug must redact them.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyCommand {
    pub enabled: bool,
    pub program: String,
    pub args: Vec<String>,
}

impl fmt::Debug for ProxyCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyCommand")
            .field("enabled", &self.enabled)
            .field("command", &"<redacted>")
            .finish()
    }
}

impl ProxyCommand {
    /// Omit unused routes from legacy/portable JSON. A populated route must be
    /// handled explicitly by an export allowlist, never silently made direct.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.enabled && self.program.is_empty() && self.args.is_empty()
    }

    /// Validates and expands arguments without ever splitting or interpreting
    /// the result. Endpoint values cannot introduce a new argument or option.
    pub fn expanded_args(
        &self,
        host: &str,
        port: u16,
        user: &str,
    ) -> Result<Vec<String>, &'static str> {
        if !Path::new(&self.program).is_absolute()
            || self.program.len() > 4096
            || self
                .program
                .chars()
                .any(|c| c.is_control() || matches!(c, '"' | '%' | '\''))
        {
            return Err("ProxyCommand 程序必须是无引号、无 token 的绝对路径");
        }
        let name = self
            .program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let stem = name.strip_suffix(".exe").unwrap_or(&name);
        if matches!(
            stem,
            "cmd"
                | "powershell"
                | "pwsh"
                | "sh"
                | "bash"
                | "dash"
                | "zsh"
                | "fish"
                | "wscript"
                | "cscript"
                | "mshta"
                | "command"
        ) || name.ends_with(".bat")
            || name.ends_with(".cmd")
            || name.ends_with(".ps1")
        {
            return Err("ProxyCommand 不支持 shell 或脚本启动器");
        }
        #[cfg(windows)]
        if !name.ends_with(".exe") || self.program.starts_with(['\\', '/']) {
            return Err("ProxyCommand 必须指向本地 .exe 程序");
        }
        if port == 0 || !valid_endpoint(host) || !valid_endpoint(user) {
            return Err("ProxyCommand 主机、端口或用户名无效");
        }
        if self.args.len() > 128 {
            return Err("ProxyCommand 参数不能超过 128 项");
        }
        let mut size = self.program.encode_utf16().count();
        let mut out = Vec::with_capacity(self.args.len());
        for arg in &self.args {
            if arg.chars().any(char::is_control) || arg.len() > 4096 {
                return Err("ProxyCommand 参数不能含控制字符或超过 4096 字节");
            }
            let mut expanded = String::new();
            let mut chars = arg.chars();
            while let Some(c) = chars.next() {
                if c != '%' {
                    expanded.push(c);
                    continue;
                }
                match chars.next() {
                    Some('h') => expanded.push_str(host),
                    Some('p') => expanded.push_str(&port.to_string()),
                    Some('r') => expanded.push_str(user),
                    Some('%') => expanded.push('%'),
                    _ => return Err("ProxyCommand 仅支持 %h、%p、%r 和 %%"),
                }
            }
            size += expanded.encode_utf16().count() * 2 + 3;
            if size > 24_000 {
                return Err("ProxyCommand 展开后的参数过长");
            }
            out.push(expanded);
        }
        Ok(out)
    }

    pub fn validate_route(
        &self,
        host: &str,
        port: u16,
        user: &str,
        has_jumps: bool,
    ) -> Result<(), &'static str> {
        if !self.enabled {
            return Ok(());
        }
        if has_jumps {
            return Err("ProxyCommand 不能与 ProxyJump 同时启用");
        }
        self.expanded_args(host, port, user).map(|_| ())
    }
}

fn valid_endpoint(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value.starts_with('-')
        && !value.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(
                    c,
                    '"' | '\'' | '%' | '`' | '$' | ';' | '&' | '|' | '<' | '>'
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(args: &[&str]) -> ProxyCommand {
        ProxyCommand {
            enabled: true,
            program: if cfg!(windows) {
                "C:\\Program Files\\Proxy\\proxy.exe"
            } else {
                "/usr/bin/proxy"
            }
            .into(),
            args: args.iter().map(|s| (*s).into()).collect(),
        }
    }

    #[test]
    fn tokens_expand_once_and_keep_argument_boundaries() {
        let command = route(&["--user=%r", "[%h]:%p", "%%h", "", "a b\\\"c"]);
        assert_eq!(
            command.expanded_args("::1", 2200, "alice").unwrap(),
            ["--user=alice", "[::1]:2200", "%h", "", "a b\\\"c"]
        );
        assert!(!format!("{command:?}").contains("a b"));
    }

    #[test]
    fn unsafe_routes_fail_closed_and_disabled_routes_never_run() {
        for token in ["%", "%n", "%d", "%C", "%x", "hello\nworld", "\0"] {
            assert!(route(&[token]).expanded_args("host", 22, "user").is_err());
        }
        for host in ["", "-oBad", "host name", "x\"y", "x%h", "x;exec"] {
            assert!(route(&["%h"]).expanded_args(host, 22, "user").is_err());
        }
        assert!(route(&[]).expanded_args("host", 0, "user").is_err());
        assert!(route(&[]).validate_route("host", 22, "user", true).is_err());
        for program in [
            "relative.exe",
            "C:\\Windows\\System32\\cmd.exe",
            "/bin/sh",
            "/tmp/test.bat",
        ] {
            let mut command = route(&[]);
            command.program = program.into();
            assert!(command.expanded_args("host", 22, "user").is_err());
            command.enabled = false;
            assert!(command.validate_route("", 0, "", true).is_ok());
        }
    }
}
