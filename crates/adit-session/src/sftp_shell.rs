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

use adit_ssh::{SftpCommand, SftpEntry, SftpEvent, SftpHandle, TransferMode};
use adit_terminal::{TerminalCore, Viewport, VtTerminal};
mod line_editor;
use line_editor::LineEditor;
use unicode_width::UnicodeWidthStr;

const PROMPT: &str = "sftp> ";

const HELP: &str = "\
可用命令:
  ls [路径]             列出远程目录
  cd [路径]             切换远程目录 (不带参数回到家目录)
  pwd                   显示远程目录
  lls [路径]            列出本地目录
  lcd <路径>            切换本地目录
  lpwd                  显示本地目录
  get <远程> [本地]     下载文件
  reget <远程> [本地]   校验完整已有前缀后继续下载
  put <本地> [远程]     上传文件 (也可以直接把文件拖进窗口)
  reput <本地> [远程]   校验远程前缀后继续上传
  mkdir <路径>          新建远程目录
  rmdir <路径>          删除远程目录
  rm <路径>             删除远程文件
  rename <原名> <新名>  重命名远程文件
  clear                 清屏
  help                  显示本帮助
  exit                  关闭 SFTP 连接
编辑: 左右/Home/End/Delete；Ctrl+A/E/U/K/W；Ctrl+R 搜索，
      Ctrl+R 查找更早匹配，Enter 接受后再次 Enter 执行，Esc/Ctrl+G 退出搜索。
传输: Ctrl+C 停止；get/put 不覆盖已有文件，续传使用 reget/reput。";

/// What the shell is waiting on, so a reply prints against the right command.
#[derive(Debug, Clone, Copy)]
enum Pending {
    List,
    /// `cd`: a listing that succeeds commits the target as the new cwd.
    Cd,
    Transfer,
    /// A listing Tab asked for, to be turned into a completion rather than
    /// printed.
    Complete,
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
    editor: LineEditor,
    next_transfer_id: u64,
    active_transfer: Option<u64>,
    /// Characters collected after an ESC, so an arrow key is read as one key.
    /// Without this the ESC was dropped as a control character and the `[A`
    /// behind it was typed into the line.
    escape: Option<String>,
    /// Where a Tab-triggered completion should splice its result, while the
    /// remote listing it needs is still on its way back.
    completing: Option<(usize, String, String, usize)>,
    pub connected: bool,
    pending: Option<Pending>,
}

/// Command names, for completing the first word.
const COMMANDS: &[&str] = &[
    "ls", "cd", "pwd", "lls", "lcd", "lpwd", "get", "put", "mkdir", "rmdir", "rm", "rename",
    "clear", "help", "exit", "reget", "reput",
];

impl SftpShell {
    pub(crate) fn new(handle: SftpHandle, local_cwd: PathBuf) -> Self {
        Self {
            handle,
            cwd: String::from("/"),
            home: String::from("/"),
            local_cwd,
            editor: LineEditor::default(),
            next_transfer_id: 0,
            active_transfer: None,
            escape: None,
            completing: None,
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
            if self.escape.as_deref() == Some("") && !matches!(ch, '[' | 'O') {
                self.escape = None;
                self.editor.cancel_search();
            }
            // Mid-escape: collect until the sequence's final byte, then act on
            // the whole thing. An arrow key is ESC `[` `A`, and handling the
            // characters one at a time dropped the ESC and typed the rest.
            if self.escape.is_some() {
                let complete = {
                    let buffer = self.escape.as_mut().expect("just checked");
                    buffer.push(ch);
                    (buffer.len() > 1 && ('@'..='~').contains(&ch)) || buffer.len() > 24
                };
                if complete {
                    match self.escape.take().unwrap_or_default().as_str() {
                        // `[` is the usual form; `O` is what a terminal in
                        // application-cursor mode sends for the same keys.
                        "[A" | "OA" => self.editor.recall(true),
                        "[B" | "OB" => self.editor.recall(false),
                        "[D" | "OD" => self.editor.left(),
                        "[C" | "OC" => self.editor.right(),
                        "[H" | "OH" | "[1~" | "[7~" => self.editor.home(),
                        "[F" | "OF" | "[4~" | "[8~" => self.editor.end(),
                        "[3~" => { self.editor.accept_search(); self.editor.delete(); }
                        _ => {}
                    }
                    self.redraw(terminal);
                }
                continue;
            }
            match ch {
                '\u{1b}' => {
                    self.editor.cancel_search();
                    self.redraw(terminal);
                    self.escape = Some(String::new());
                }
                '\t' => self.complete(terminal),
                '\r' | '\n' => {
                    if self.editor.accept_search() { self.redraw(terminal); }
                    else if self.pending.is_none() && self.active_transfer.is_none() { self.submit(terminal); }
                }
                // Backspace / DEL.
                '\u{8}' | '\u{7f}' => {
                    self.editor.backspace();
                    self.redraw(terminal);
                }
                // Ctrl+C: abandon the line, like a shell.
                '\u{3}' => {
                    self.editor.clear();
                    if let Some(id) = self.active_transfer { self.handle.cancel(id); }
                    terminal.feed_str("^C\r\n");
                    if self.active_transfer.is_none() { self.prompt(terminal); }
                }
                // Ctrl+U: kill the line.
                '\u{15}' => {
                    self.editor.kill_before(); self.redraw(terminal);
                }
                '\u{1}' => { self.editor.home(); self.redraw(terminal); }
                '\u{5}' => { self.editor.end(); self.redraw(terminal); }
                '\u{2}' => { self.editor.left(); self.redraw(terminal); }
                '\u{6}' => { self.editor.right(); self.redraw(terminal); }
                '\u{4}' => { self.editor.accept_search(); self.editor.delete(); self.redraw(terminal); }
                '\u{b}' => { self.editor.kill_after(); self.redraw(terminal); }
                '\u{17}' => { self.editor.kill_word(); self.redraw(terminal); }
                '\u{12}' => { self.editor.reverse_search(); self.redraw(terminal); }
                '\u{7}' => { self.editor.cancel_search(); self.redraw(terminal); }
                // Ignore the remaining control characters.
                ch if (ch as u32) < 0x20 => {}
                ch => {
                    self.editor.insert(ch);
                    self.redraw(terminal);
                }
            }
        }
    }

    /// Queue an upload of a dropped file into the current remote directory.
    pub(crate) fn upload_dropped(&mut self, terminal: &mut VtTerminal, local: &Path) {
        if self.pending.is_some() {
            self.writeln(terminal, "已有传输进行中；Ctrl+C 可停止");
            return;
        }
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
        let id = self.begin_transfer();
        self.send(terminal, SftpCommand::Upload {
            id,
            local: local.to_path_buf(),
            remote,
        });
    }

    /// Fold an SFTP event into the transcript. Returns true when the connection closed.
    pub(crate) fn on_event(&mut self, terminal: &mut VtTerminal, event: SftpEvent) -> bool {
        match event {
            SftpEvent::Status(status) => {
                // Only interesting before the prompt exists; afterwards it is noise.
                if !self.connected || self.active_transfer.is_some() {
                    terminal.feed_str("\r\n");
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
                // Asked for by Tab: splice, never print the directory.
                Some(Pending::Complete) => {
                    self.pending = None;
                    self.finish_remote_completion(terminal, &entries);
                }
                // Unsolicited (or a stray listing while a transfer is pending):
                // stay quiet and leave `pending` alone.
                _ => {}
            },
            // Per-chunk progress would fight the prompt for the line; `Done` reports.
            SftpEvent::Progress { .. } => {}
            SftpEvent::Done { id, label, bytes } => {
                if self.active_transfer != Some(id) { return false; }
                self.active_transfer = None;
                self.pending = None;
                terminal.feed_str("\r\n");
                self.writeln(terminal, &format!("{label} 完成 ({})", human_size(bytes)));
                self.prompt(terminal);
            }
            SftpEvent::Cancelled { id, label } => {
                if self.active_transfer != Some(id) { return false; }
                self.active_transfer = None;
                self.pending = None;
                terminal.feed_str("\r\n");
                self.writeln(terminal, &format!("{label} 已停止"));
                self.prompt(terminal);
            }
            SftpEvent::Error(error) => {
                self.pending = None;
                self.writeln(terminal, &format!("错误: {error}"));
                self.prompt(terminal);
            }
            SftpEvent::TransferFailed { id, error, .. } => {
                if self.active_transfer != Some(id) { return false; }
                self.active_transfer = None;
                self.pending = None;
                terminal.feed_str("\r\n");
                self.writeln(terminal, &format!("传输失败: {error}"));
                self.prompt(terminal);
            }
            SftpEvent::TransferNote { id, note } => {
                if self.active_transfer == Some(id) {
                    terminal.feed_str("\r\n");
                    self.writeln(terminal, &note);
                }
            }
            SftpEvent::Closed => {
                self.connected = false;
                self.pending = None;
                self.active_transfer = None;
                self.writeln(terminal, "SFTP 连接已关闭");
                return true;
            }
        }
        false
    }

    fn submit(&mut self, terminal: &mut VtTerminal) {
        // Editing uses a horizontal viewport; commit the complete command to
        // scrollback/logging before executing it.
        terminal.feed_str("\r\u{1b}[K");
        terminal.feed_str(PROMPT);
        terminal.feed_str(&self.editor.line);
        terminal.feed_str("\r\n");
        let line = self.editor.submit();
        if line.is_empty() {
            self.prompt(terminal);
            return;
        }
        self.execute(terminal, &line);
    }

    /// Repaint the prompt and the current line in place.
    fn redraw(&mut self, terminal: &mut VtTerminal) {
        // Carriage return, then erase to end of line: shorter than counting
        // backspaces, and correct when the new line is shorter than the old.
        terminal.feed_str("\r\u{1b}[K");
        let cols = usize::from(terminal.snapshot(Viewport::tail(1)).size.cols).max(8);
        let caption = self.editor.search_caption().unwrap_or_else(|| PROMPT.into());
        let caption: String = caption.chars().scan(0usize, |width, ch| {
            *width += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            (*width < cols / 2).then_some(ch)
        }).collect();
        let budget = cols.saturating_sub(UnicodeWidthStr::width(caption.as_str()) + 1).max(1);
        let mut start = self.editor.cursor;
        let mut before = 0;
        for (index, ch) in self.editor.line[..self.editor.cursor].char_indices().rev() {
            let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if before + width > budget / 2 { break; }
            before += width;
            start = index;
        }
        let mut width = 0;
        let visible: String = self.editor.line[start..].chars().take_while(|ch| {
            width += unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0);
            width <= budget
        }).collect();
        terminal.feed_str(&caption);
        terminal.feed_str(&visible);
        terminal.feed_str("\r");
        let column = UnicodeWidthStr::width(caption.as_str()) + before;
        if column > 0 { terminal.feed_str(&format!("\u{1b}[{column}C")); }
    }

    /// Complete the word under the cursor: a command name in first position, a
    /// path after that — local or remote depending on which argument of which
    /// command it is.
    fn complete(&mut self, terminal: &mut VtTerminal) {
        if self.editor.search_caption().is_some() || self.active_transfer.is_some() { return; }
        let start = word_start(&self.editor.line[..self.editor.cursor]);
        let word = self.editor.line[start..self.editor.cursor].to_owned();

        if self.editor.line[..start].trim().is_empty() {
            let candidates: Vec<String> = COMMANDS
                .iter()
                .filter(|name| name.starts_with(&word))
                .map(|name| (*name).to_owned())
                .collect();
            self.apply_completion(terminal, start, &word, &candidates);
            return;
        }

        let args = split_args(&self.editor.line);
        let command = args.first().map(String::as_str).unwrap_or_default();
        // Which argument the cursor sits in; 0 is the command itself.
        let index = split_args(&self.editor.line[..start]).len();
        let local = matches!(
            (command, index),
            ("lcd" | "lls", _) | ("put" | "reput", 1) | ("get" | "reget", 2)
        );
        if local {
            self.complete_local(terminal, start, &word);
        } else {
            self.complete_remote(terminal, start, &word);
        }
    }

    fn complete_local(&mut self, terminal: &mut VtTerminal, start: usize, word: &str) {
        let (dir_part, partial) = split_path(word, std::path::is_separator);
        let base = if dir_part.is_empty() {
            self.local_cwd.clone()
        } else {
            self.local_path(dir_part)
        };
        let Ok(entries) = std::fs::read_dir(base) else {
            return;
        };
        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(partial) {
                continue;
            }
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            candidates.push(format!(
                "{dir_part}{name}{}",
                if is_dir { "/" } else { "" }
            ));
        }
        candidates.sort();
        self.apply_completion(terminal, start, word, &candidates);
    }

    /// Ask the server for the directory being completed. The answer arrives as
    /// a `Listing` event, which is why the splice point has to be remembered
    /// here rather than recomputed there — by then the user may have typed on.
    fn complete_remote(&mut self, terminal: &mut VtTerminal, start: usize, word: &str) {
        if !self.connected || self.pending.is_some() {
            return;
        }
        let (dir_part, _) = split_path(word, |ch| ch == '/');
        let target = resolve_remote(&self.cwd, &self.home, dir_part);
        self.completing = Some((start, word.to_owned(), self.editor.line.clone(), self.editor.cursor));
        self.pending = Some(Pending::Complete);
        self.send(terminal, SftpCommand::ListDir(target));
    }

    /// Finish a remote completion once its listing lands.
    fn finish_remote_completion(&mut self, terminal: &mut VtTerminal, entries: &[SftpEntry]) {
        let Some((start, word, original, cursor)) = self.completing.take() else {
            return;
        };
        // The line may have moved on while the request was in flight; splicing
        // into a stale offset would corrupt it.
        if self.editor.line != original || self.editor.cursor != cursor {
            return;
        }
        let (dir_part, partial) = split_path(&word, |ch| ch == '/');
        let mut candidates: Vec<String> = entries
            .iter()
            .filter(|entry| entry.name.starts_with(partial))
            .map(|entry| {
                format!(
                    "{dir_part}{}{}",
                    entry.name,
                    if entry.is_dir { "/" } else { "" }
                )
            })
            .collect();
        candidates.sort();
        self.apply_completion(terminal, start, &word, &candidates);
    }

    /// Insert the longest unambiguous extension of `word`. When that adds
    /// nothing and several candidates remain, show them instead of silently
    /// doing nothing — a Tab that appears to be ignored is the worst outcome.
    fn apply_completion(
        &mut self,
        terminal: &mut VtTerminal,
        start: usize,
        word: &str,
        candidates: &[String],
    ) {
        if candidates.is_empty() {
            return;
        }
        let common = longest_common_prefix(candidates);
        if common.len() > word.len() {
            self.editor.line.replace_range(start..self.editor.cursor, &common);
            self.editor.cursor = start + common.len();
            // A single match is settled; save the space bar.
            if candidates.len() == 1 && !common.ends_with('/')
                && !self.editor.line[self.editor.cursor..].starts_with(' ') {
                self.editor.line.insert(self.editor.cursor, ' ');
                self.editor.cursor += 1;
            }
            self.redraw(terminal);
        } else if candidates.len() > 1 {
            terminal.feed_str("\r\n");
            let listing = candidates.join("  ");
            self.writeln(terminal, &listing);
            self.redraw(terminal);
        }
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
            "ls" | "dir" | "cd" | "get" | "put" | "reget" | "reput" | "mkdir" | "rmdir" | "rm" | "del" | "rename" | "mv"
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
            "get" | "reget" => {
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
                let id = self.begin_transfer();
                let mode = if command == "reget" { TransferMode::Resume } else { TransferMode::CreateNew };
                self.send(terminal, SftpCommand::DownloadWith { id, remote, local, mode });
            }
            "put" | "reput" => {
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
                let id = self.begin_transfer();
                let mode = if command == "reput" { TransferMode::Resume } else { TransferMode::CreateNew };
                self.send(terminal, SftpCommand::UploadWith { id, local, remote, mode });
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
            self.active_transfer = None;
            self.connected = false;
            self.writeln(terminal, "SFTP 连接已断开");
            self.prompt(terminal);
        }
    }

    fn begin_transfer(&mut self) -> u64 {
        let id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.saturating_add(1);
        self.active_transfer = Some(id);
        id
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
        self.redraw(terminal);
    }
}

/// Byte offset where the word under the cursor begins.
///
/// The caller passes the prefix ending at the editor cursor, so completion can
/// replace that word while retaining the suffix to the cursor's right.
fn word_start(line: &str) -> usize {
    let mut quoted = false;
    let mut start = 0;
    for (at, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => start = at + c.len_utf8(),
            _ => {}
        }
    }
    start
}

/// Split a path being typed into the part that names a directory (kept,
/// separator included) and the part being completed.
fn split_path(word: &str, is_sep: impl Fn(char) -> bool) -> (&str, &str) {
    match word.char_indices().filter(|(_, ch)| is_sep(*ch)).last() {
        Some((at, ch)) => word.split_at(at + ch.len_utf8()),
        None => ("", word),
    }
}

/// The longest prefix every candidate shares — what Tab can safely insert.
fn longest_common_prefix(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut end = first.len();
    for other in &candidates[1..] {
        end = end.min(
            first
                .char_indices()
                .zip(other.char_indices())
                .take_while(|((_, a), (_, b))| a == b)
                .last()
                .map_or(0, |((at, ch), _)| at + ch.len_utf8()),
        );
    }
    first[..end].to_owned()
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

    /// Completion splices at the last unquoted space, so a quoted path with a
    /// space in it completes as one word rather than from its last fragment.
    #[test]
    fn the_word_under_the_cursor_ends_at_the_last_unquoted_space() {
        assert_eq!(word_start("ls"), 0);
        assert_eq!(word_start("get /var/lo"), 4);
        assert_eq!(word_start("put \"C:/my fi"), 4);
        assert_eq!(word_start("cd "), 3);
    }

    /// The directory part is kept verbatim, separator included, so the splice
    /// rebuilds exactly what the user typed plus the completion.
    #[test]
    fn a_path_splits_into_the_part_to_keep_and_the_part_to_complete() {
        assert_eq!(split_path("/var/lo", |c| c == '/'), ("/var/", "lo"));
        assert_eq!(split_path("lo", |c| c == '/'), ("", "lo"));
        assert_eq!(split_path("/", |c| c == '/'), ("/", ""));
    }

    /// Tab may only insert what every candidate agrees on; inserting more
    /// would silently pick one of them.
    #[test]
    fn only_the_shared_prefix_is_safe_to_insert() {
        let owned = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(longest_common_prefix(&owned(&["log/", "logs/"])), "log");
        assert_eq!(longest_common_prefix(&owned(&["etc/", "var/"])), "");
        assert_eq!(longest_common_prefix(&owned(&["only"])), "only");
        assert_eq!(longest_common_prefix(&[]), "");
    }

    #[test]
    fn human_size_is_readable() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(1024 * 1024), "1.0M");
    }
}
