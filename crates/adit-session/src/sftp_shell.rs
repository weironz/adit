//! Command-line SFTP: an `sftp>` prompt living in its own tab.
//!
//! This is the *command-line* SFTP — what SecureCRT opens on Alt+P — and is a
//! deliberately separate thing from the dual-pane file manager ([`crate::SftpBrowser`],
//! SecureFX-style). Both exist on purpose: this one is for typing `ls`/`cd`/`get`/`put`
//! (and for dropping files onto), the panel is for point-and-click.
//!
//! Output rides the session's [`VtTerminal`], so tabs, scrollback, selection, and
//! logging all work for free. SFTP is a request protocol, not a shell, so there is
//! no remote echo — this module echoes typed characters itself and owns the prompt.

use std::path::{Path, PathBuf};

use adit_ssh::{SftpCommand, SftpEntry, SftpEvent, SftpHandle};
use adit_terminal::VtTerminal;

const PROMPT: &str = "sftp> ";

/// The `id` every command-line-shell transfer carries. The shell runs transfers
/// one at a time and never cancels, so it doesn't need unique ids — its handle's
/// cancel set is never populated, and the worker clears the id after each.
const SHELL_TRANSFER_ID: u64 = 0;

const HELP: &str = "\
可用命令:
  ls [路径]             列出远程目录
  cd [路径]             切换远程目录 (不带参数回到家目录)
  pwd                   显示远程目录
  lls [路径]            列出本地目录
  lcd <路径>            切换本地目录
  lpwd                  显示本地目录
  get <远程> [本地]     下载文件
  put <本地> [远程]     上传文件 (也可以直接把文件拖进窗口)
  mkdir <路径>          新建远程目录
  rmdir <路径>          删除远程目录
  rm <路径>             删除远程文件
  rename <原名> <新名>  重命名远程文件
  clear                 清屏
  help                  显示本帮助
  exit                  关闭 SFTP 连接";

/// What the shell is waiting on, so a reply prints against the right command.
#[derive(Debug, Clone, Copy)]
enum Pending {
    List,
    /// `cd`: a listing that succeeds commits the target as the new cwd.
    Cd,
    Transfer,
}

/// A command-line SFTP session: its own SFTP connection plus a line editor.
pub struct SftpShell {
    handle: SftpHandle,
    /// Remote working directory.
    pub cwd: String,
    /// Remote home, for a bare `cd`.
    home: String,
    /// Local working directory, for `lls`/`lcd`/`get`/`put`.
    pub local_cwd: PathBuf,
    /// The line being typed. Echoed by us; see the module docs.
    line: String,
    history: Vec<String>,
    pub connected: bool,
    pending: Option<Pending>,
}

impl SftpShell {
    pub(crate) fn new(handle: SftpHandle, local_cwd: PathBuf) -> Self {
        Self {
            handle,
            cwd: String::from("/"),
            home: String::from("/"),
            local_cwd,
            line: String::new(),
            history: Vec::new(),
            connected: false,
            pending: None,
        }
    }

    /// Next pending event from the SFTP connection, if any.
    pub(crate) fn try_recv(&self) -> Option<SftpEvent> {
        self.handle.try_recv()
    }

    /// Feed typed bytes through the line editor, echoing as we go.
    pub(crate) fn feed_input(&mut self, terminal: &mut VtTerminal, bytes: &[u8]) {
        // The UI sends whole UTF-8 characters, so decoding per batch is safe.
        let text = String::from_utf8_lossy(bytes).into_owned();
        for ch in text.chars() {
            match ch {
                '\r' | '\n' => self.submit(terminal),
                // Backspace / DEL.
                '\u{8}' | '\u{7f}' => {
                    if self.line.pop().is_some() {
                        terminal.feed_str("\u{8} \u{8}");
                    }
                }
                // Ctrl+C: abandon the line, like a shell.
                '\u{3}' => {
                    self.line.clear();
                    terminal.feed_str("^C\r\n");
                    self.prompt(terminal);
                }
                // Ctrl+U: kill the line.
                '\u{15}' => {
                    while self.line.pop().is_some() {
                        terminal.feed_str("\u{8} \u{8}");
                    }
                }
                // Ignore other control characters (incl. Tab: no completion yet).
                ch if (ch as u32) < 0x20 => {}
                ch => {
                    self.line.push(ch);
                    terminal.feed_str(&ch.to_string());
                }
            }
        }
    }

    /// Queue an upload of a dropped file into the current remote directory.
    pub(crate) fn upload_dropped(&mut self, terminal: &mut VtTerminal, local: &Path) {
        if !self.connected {
            self.writeln(terminal, "尚未连接，无法上传");
            self.prompt(terminal);
            return;
        }
        let Some(name) = local.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            self.writeln(terminal, "无法识别文件名");
            self.prompt(terminal);
            return;
        };
        let remote = resolve_remote(&self.cwd, &self.home, &name);
        // A drop can land mid-typed-line; keep the transcript readable.
        terminal.feed_str("\r\n");
        self.writeln(terminal, &format!("上传 {} -> {remote}", local.display()));
        self.pending = Some(Pending::Transfer);
        // The shell runs one transfer at a time and never cancels, so a fixed id
        // is safe: its handle's cancel set is never populated.
        let _ = self.handle.send(SftpCommand::Upload {
            id: SHELL_TRANSFER_ID,
            local: local.to_path_buf(),
            remote,
        });
    }

    /// Fold an SFTP event into the transcript. Returns true when the connection closed.
    pub(crate) fn on_event(&mut self, terminal: &mut VtTerminal, event: SftpEvent) -> bool {
        match event {
            SftpEvent::Status(status) => {
                // Only interesting before the prompt exists; afterwards it is noise.
                if !self.connected {
                    self.writeln(terminal, &status);
                }
            }
            SftpEvent::Ready { home } => {
                self.home = home.clone();
                self.cwd = home;
                self.connected = true;
                self.writeln(terminal, &format!("已连接。远程目录: {}", self.cwd));
                self.writeln(terminal, "输入 help 查看可用命令，或直接把文件拖进来上传。");
                self.prompt(terminal);
            }
            // Only ever print a listing that was actually asked for. The SFTP
            // backend lists the home directory on its own right after connecting
            // (to populate the dual-pane panel, which shares this connection type),
            // and dumping that unasked buried the prompt under the whole home dir.
            SftpEvent::Listing { path, entries } => match self.pending {
                Some(Pending::Cd) => {
                    self.pending = None;
                    self.cwd = path;
                    self.writeln(terminal, &format!("远程目录: {}", self.cwd));
                    self.prompt(terminal);
                }
                Some(Pending::List) => {
                    self.pending = None;
                    self.print_listing(terminal, &entries);
                    self.prompt(terminal);
                }
                // Unsolicited (or a stray listing while a transfer is pending):
                // stay quiet and leave `pending` alone.
                _ => {}
            },
            // Per-chunk progress would fight the prompt for the line; `Done` reports.
            SftpEvent::Progress { .. } => {}
            SftpEvent::Done { label, bytes, .. } => {
                self.pending = None;
                self.writeln(terminal, &format!("{label} 完成 ({})", human_size(bytes)));
                self.prompt(terminal);
            }
            // The shell never cancels its own transfers, but the variant must be
            // handled; if one ever arrives, report it plainly.
            SftpEvent::Cancelled { label, .. } => {
                self.pending = None;
                self.writeln(terminal, &format!("{label} 已停止"));
                self.prompt(terminal);
            }
            SftpEvent::Error(error) => {
                self.pending = None;
                self.writeln(terminal, &format!("错误: {error}"));
                self.prompt(terminal);
            }
            SftpEvent::Closed => {
                self.connected = false;
                self.writeln(terminal, "SFTP 连接已关闭");
                return true;
            }
        }
        false
    }

    fn submit(&mut self, terminal: &mut VtTerminal) {
        terminal.feed_str("\r\n");
        let line = std::mem::take(&mut self.line);
        let line = line.trim().to_owned();
        if line.is_empty() {
            self.prompt(terminal);
            return;
        }
        self.history.push(line.clone());
        self.execute(terminal, &line);
    }

    fn execute(&mut self, terminal: &mut VtTerminal, line: &str) {
        let args = split_args(line);
        let Some(command) = args.first().cloned() else {
            self.prompt(terminal);
            return;
        };
        let rest: Vec<String> = args[1..].to_vec();

        // Remote work needs a live connection; fail fast rather than hang.
        let remote_command = matches!(
            command.as_str(),
            "ls" | "dir" | "cd" | "get" | "put" | "mkdir" | "rmdir" | "rm" | "del" | "rename" | "mv"
        );
        if remote_command && !self.connected {
            self.writeln(terminal, "尚未连接");
            self.prompt(terminal);
            return;
        }

        match command.as_str() {
            "help" | "?" => {
                self.writeln(terminal, HELP);
                self.prompt(terminal);
            }
            "clear" => {
                terminal.clear();
                self.prompt(terminal);
            }
            "pwd" => {
                let cwd = self.cwd.clone();
                self.writeln(terminal, &cwd);
                self.prompt(terminal);
            }
            "lpwd" => {
                let local = self.local_cwd.display().to_string();
                self.writeln(terminal, &local);
                self.prompt(terminal);
            }
            "lcd" => {
                self.local_cd(terminal, rest.first().map(String::as_str));
                self.prompt(terminal);
            }
            "lls" => {
                self.local_ls(terminal, rest.first().map(String::as_str));
                self.prompt(terminal);
            }
            "exit" | "quit" | "bye" => {
                let _ = self.handle.send(SftpCommand::Disconnect);
                self.writeln(terminal, "正在断开…");
            }
            "ls" | "dir" => {
                let path = rest
                    .first()
                    .map_or_else(|| self.cwd.clone(), |p| resolve_remote(&self.cwd, &self.home, p));
                self.pending = Some(Pending::List);
                self.send(terminal, SftpCommand::ListDir(path));
            }
            "cd" => {
                let target = rest
                    .first()
                    .map_or_else(|| self.home.clone(), |p| resolve_remote(&self.cwd, &self.home, p));
                // There is no SFTP "chdir": a successful listing is what proves the
                // directory exists, so `cd` is a listing whose reply commits the cwd.
                self.pending = Some(Pending::Cd);
                self.send(terminal, SftpCommand::ListDir(target));
            }
            "get" => {
                let Some(remote_arg) = rest.first() else {
                    self.usage(terminal, "get <远程> [本地]");
                    return;
                };
                let remote = resolve_remote(&self.cwd, &self.home, remote_arg);
                let name = remote.rsplit('/').next().unwrap_or("download").to_owned();
                let local = match rest.get(1) {
                    Some(arg) => {
                        let candidate = self.local_path(arg);
                        // `get f dir/` (or an existing dir) means "into that dir".
                        if candidate.is_dir() {
                            candidate.join(&name)
                        } else {
                            candidate
                        }
                    }
                    None => self.local_cwd.join(&name),
                };
                self.writeln(terminal, &format!("下载 {remote} -> {}", local.display()));
                self.pending = Some(Pending::Transfer);
                self.send(terminal, SftpCommand::Download { id: SHELL_TRANSFER_ID, remote, local });
            }
            "put" => {
                let Some(local_arg) = rest.first() else {
                    self.usage(terminal, "put <本地> [远程]");
                    return;
                };
                let local = self.local_path(local_arg);
                if !local.is_file() {
                    self.writeln(terminal, &format!("本地文件不存在: {}", local.display()));
                    self.prompt(terminal);
                    return;
                }
                let name = local
                    .file_name()
                    .map_or_else(|| String::from("upload"), |n| n.to_string_lossy().into_owned());
                let remote = rest
                    .get(1)
                    .map_or_else(|| resolve_remote(&self.cwd, &self.home, &name), |p| {
                        resolve_remote(&self.cwd, &self.home, p)
                    });
                self.writeln(terminal, &format!("上传 {} -> {remote}", local.display()));
                self.pending = Some(Pending::Transfer);
                self.send(terminal, SftpCommand::Upload { id: SHELL_TRANSFER_ID, local, remote });
            }
            "mkdir" => {
                let Some(arg) = rest.first() else {
                    self.usage(terminal, "mkdir <路径>");
                    return;
                };
                let path = resolve_remote(&self.cwd, &self.home, arg);
                self.send(terminal, SftpCommand::Mkdir(path));
            }
            "rmdir" | "rm" | "del" => {
                let Some(arg) = rest.first() else {
                    self.usage(terminal, &format!("{command} <路径>"));
                    return;
                };
                let path = resolve_remote(&self.cwd, &self.home, arg);
                let is_dir = command == "rmdir";
                self.send(terminal, SftpCommand::Remove { path, is_dir });
            }
            "rename" | "mv" => {
                let (Some(from), Some(to)) = (rest.first(), rest.get(1)) else {
                    self.usage(terminal, &format!("{command} <原名> <新名>"));
                    return;
                };
                let from = resolve_remote(&self.cwd, &self.home, from);
                let to = resolve_remote(&self.cwd, &self.home, to);
                self.send(terminal, SftpCommand::Rename { from, to });
            }
            other => {
                self.writeln(
                    terminal,
                    &format!("未知命令: {other} (输入 help 查看可用命令)"),
                );
                self.prompt(terminal);
            }
        }
    }

    /// Dispatch a command; a send failure must still return the prompt rather than
    /// leave the shell looking hung.
    fn send(&mut self, terminal: &mut VtTerminal, command: SftpCommand) {
        if self.handle.send(command).is_err() {
            self.pending = None;
            self.connected = false;
            self.writeln(terminal, "SFTP 连接已断开");
            self.prompt(terminal);
        }
    }

    fn usage(&mut self, terminal: &mut VtTerminal, usage: &str) {
        self.writeln(terminal, &format!("用法: {usage}"));
        self.prompt(terminal);
    }

    fn local_cd(&mut self, terminal: &mut VtTerminal, arg: Option<&str>) {
        let Some(arg) = arg else {
            let local = self.local_cwd.display().to_string();
            self.writeln(terminal, &local);
            return;
        };
        let target = self.local_path(arg);
        if target.is_dir() {
            // Canonicalize so `..` collapses instead of accumulating.
            self.local_cwd = std::fs::canonicalize(&target).unwrap_or(target);
            let local = self.local_cwd.display().to_string();
            self.writeln(terminal, &format!("本地目录: {local}"));
        } else {
            self.writeln(terminal, &format!("本地目录不存在: {}", target.display()));
        }
    }

    fn local_ls(&mut self, terminal: &mut VtTerminal, arg: Option<&str>) {
        let dir = arg.map_or_else(|| self.local_cwd.clone(), |a| self.local_path(a));
        let Ok(read) = std::fs::read_dir(&dir) else {
            self.writeln(terminal, &format!("无法读取本地目录: {}", dir.display()));
            return;
        };
        let mut rows: Vec<(bool, String, u64)> = read
            .flatten()
            .map(|entry| {
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                (is_dir, entry.file_name().to_string_lossy().into_owned(), size)
            })
            .collect();
        if rows.is_empty() {
            self.writeln(terminal, "(空目录)");
            return;
        }
        rows.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
        });
        for (is_dir, name, size) in rows {
            let kind = if is_dir { 'd' } else { '-' };
            let size = if is_dir {
                String::from("-")
            } else {
                human_size(size)
            };
            self.writeln(terminal, &format!("{kind} {size:>10}  {name}"));
        }
    }

    /// Resolve a user-typed local path against the shell's local cwd.
    fn local_path(&self, arg: &str) -> PathBuf {
        let path = Path::new(arg);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.local_cwd.join(path)
        }
    }

    fn print_listing(&mut self, terminal: &mut VtTerminal, entries: &[SftpEntry]) {
        if entries.is_empty() {
            self.writeln(terminal, "(空目录)");
            return;
        }
        let mut sorted: Vec<&SftpEntry> = entries.iter().collect();
        // Directories first, then case-insensitive by name — same order as the panel.
        sorted.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        for entry in sorted {
            let kind = if entry.is_dir { 'd' } else { '-' };
            let size = if entry.is_dir {
                String::from("-")
            } else {
                human_size(entry.size)
            };
            self.writeln(terminal, &format!("{kind} {size:>10}  {}", entry.name));
        }
    }

    fn writeln(&mut self, terminal: &mut VtTerminal, text: &str) {
        // The VT needs CRLF; a bare \n would stair-step the output.
        for line in text.split('\n') {
            terminal.feed_str(line);
            terminal.feed_str("\r\n");
        }
    }

    fn prompt(&mut self, terminal: &mut VtTerminal) {
        terminal.feed_str(PROMPT);
        // Redraw anything already typed (a reply can land mid-line).
        if !self.line.is_empty() {
            let line = self.line.clone();
            terminal.feed_str(&line);
        }
    }
}

/// Split a command line into arguments, honouring double quotes so paths with
/// spaces work (`put "C:\my files\a.txt"`).
fn split_args(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in line.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Resolve a user-typed remote path against the cwd, expanding `~` and collapsing
/// `.`/`..`. Always returns an absolute POSIX path (SFTP has no relative cwd).
fn resolve_remote(cwd: &str, home: &str, arg: &str) -> String {
    let joined = if arg.starts_with('/') {
        arg.to_owned()
    } else if arg == "~" {
        home.to_owned()
    } else if let Some(rest) = arg.strip_prefix("~/") {
        format!("{}/{rest}", home.trim_end_matches('/'))
    } else {
        format!("{}/{arg}", cwd.trim_end_matches('/'))
    };

    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_remote_handles_absolute_relative_home_and_dotdot() {
        let home = "/home/will";
        assert_eq!(resolve_remote("/srv", home, "/etc/nginx"), "/etc/nginx");
        assert_eq!(resolve_remote("/srv", home, "logs"), "/srv/logs");
        assert_eq!(resolve_remote("/srv/logs", home, ".."), "/srv");
        assert_eq!(resolve_remote("/srv/logs", home, "../conf"), "/srv/conf");
        assert_eq!(resolve_remote("/srv", home, "~"), "/home/will");
        assert_eq!(resolve_remote("/srv", home, "~/.ssh"), "/home/will/.ssh");
        assert_eq!(resolve_remote("/srv", home, "./a/./b"), "/srv/a/b");
        // Escaping past the root must clamp at "/" rather than underflow.
        assert_eq!(resolve_remote("/", home, "../../.."), "/");
        // A trailing slash must not produce a doubled separator.
        assert_eq!(resolve_remote("/srv/", home, "logs"), "/srv/logs");
    }

    #[test]
    fn split_args_keeps_quoted_paths_together() {
        assert_eq!(split_args("ls /tmp"), vec!["ls", "/tmp"]);
        assert_eq!(
            split_args(r#"put "C:\my files\a.txt" /srv/a.txt"#),
            vec!["put", r"C:\my files\a.txt", "/srv/a.txt"]
        );
        assert_eq!(split_args("   ls    "), vec!["ls"]);
        assert!(split_args("").is_empty());
    }

    #[test]
    fn human_size_is_readable() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(1024 * 1024), "1.0M");
    }
}
