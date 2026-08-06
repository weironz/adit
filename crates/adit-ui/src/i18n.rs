//! UI language.
//!
//! Lookup is by the Chinese source string, and anything missing falls back to
//! it. Every string in this program was written in Chinese first, so English is
//! a translation of it rather than the other way round — which means a missing
//! entry shows the original instead of a bare key, and the table can grow
//! without touching a single call site.

use std::sync::atomic::{AtomicU8, Ordering};

use adit_storage::Language;

/// Read on every drawn string, written once when the setting changes. An atomic
/// rather than a field on `AditApp` because the alternative is threading a
/// language through every widget helper in the crate for a value that never
/// differs between them.
static LANGUAGE: AtomicU8 = AtomicU8::new(0);

pub(crate) fn set_language(language: Language) {
    LANGUAGE.store(u8::from(language == Language::En), Ordering::Relaxed);
}

/// Translate one UI string, or hand it back unchanged.
#[must_use]
pub(crate) fn t(zh: &'static str) -> &'static str {
    if LANGUAGE.load(Ordering::Relaxed) == 0 {
        return zh;
    }
    en(zh)
}

fn en(zh: &'static str) -> &'static str {
    match zh {
        "RDP 会话共享剪贴板（仅文本，下次连接生效）" => "Share the clipboard with RDP sessions (text only, applies to the next connection)",
        "SHA256 指纹" => "SHA256 fingerprint",
        "iced · russh · vte 终端核心 — 无 WebView，无 JavaScript" => "iced · russh · vte terminal core — no WebView, no JavaScript",
        "下载并更新" => "Download and update",
        "仅对服务端未着色的文本生效，全屏程序（vim、less 等）中不启用" => "Applies only to text the server left uncoloured; off inside full-screen programs such as vim and less",
        "会话日志" => "Session logs",
        "保存到会话配置（连接时自动开启）" => "Save to the session profile (on automatically when it connects)",
        "保存密码" => "Save password",
        "关闭" => "Close",
        "兼容 AWS S3、MinIO、Cloudflare R2、阿里云 OSS。MinIO 等自建网关需要路径风格寻址。" => "Works with AWS S3, MinIO, Cloudflare R2 and Alibaba OSS. Self-hosted gateways such as MinIO need path-style addressing.",
        "删除" => "Delete",
        "删除某台主机后，下次连接会重新记录其密钥；密钥被更改（可能的中间人）时仍会拦截。" => "Removing a host makes the next connection record its key afresh; a key that has changed — a possible man-in-the-middle — is still refused.",
        "加密保存在配置目录，可随 Dropbox 等同步到其他电脑" => "Stored encrypted in the config folder, and can travel to other machines through Dropbox or similar",
        "即时" => "Immediately",
        "原生 Rust 桌面 SSH 终端" => "A native Rust desktop SSH terminal",
        "发送" => "Send",
        "取消" => "Cancel",
        "受信主机密钥" => "Trusted host keys",
        "可用变量：%N 会话名  %H 主机  %Y 年 %M 月 %D 日  %h 时 %m 分 %s 秒" => "Variables: %N session  %H host  %Y year %M month %D day  %h hour %m minute %s second",
        "右键直接粘贴（不弹出菜单）" => "Right-click pastes straight away (no menu)",
        "同时同步已保存的密码（加密后上传，主密码不出本机）" => "Also sync saved passwords (uploaded encrypted; the master passphrase never leaves this machine)",
        "启动时自动检查更新" => "Check for updates on startup",
        "命令片段" => "Snippets",
        "填写自己的 client id 可避开共享配额；本地或自行编译的版本也需要它。" => "Your own client id avoids the shared quota; a local or self-built copy needs one anyway.",
        "填到文件而不是目录。Nextcloud、坚果云、群晖均可；该方式支持并发写检测，最安全。" => "Point this at a file, not a folder. Nextcloud, Jianguoyun and Synology all work; this backend detects concurrent writes, which makes it the safest of the six.",
        "字体" => "Font",
        "字号" => "Size",
        "完成后会自动启动安装程序" => "The installer starts by itself once the download finishes",
        "密钥变更可能意味着中间人攻击。仅在你确知服务器更换过密钥时才接受。" => "A changed key can mean a man-in-the-middle. Accept only if you know the server's key was replaced.",
        "尚无受信主机密钥（首次连接会自动信任并记录）" => "No trusted host keys yet (the first connection trusts and records one)",
        "已保存（连接时自动开启）" => "Saved (on automatically when it connects)",
        "恢复默认" => "Reset to default",
        "打开" => "Open",
        "打开链接？" => "Open this link?",
        "提示：右键粘贴开启后，清屏 / 回到底部可用工具栏或 Edit 菜单。程序也支持 bracketed paste（应用开启后粘贴不会被自动执行）。" => "Note: with right-click paste on, clear the screen and jump to the bottom from the Edit menu. Bracketed paste is supported, so once the remote program asks for it a paste is not executed on its own.",
        "新增片段" => "New snippet",
        "无需操作，安装完成后 Adit 会自动关闭并重启（可能需要确认一次 UAC）" => "Nothing to do — Adit closes and restarts itself when the install finishes (UAC may ask once)",
        "日志文件名（留空 = 默认）" => "Log file name (blank = default)",
        "日志目录（留空 = 配置目录下的 logs）" => "Log folder (blank = logs under the config folder)",
        "更改…" => "Change…",
        "查找" => "Find",
        "查看发布说明" => "Release notes",
        "检查/更新失败" => "Update check failed",
        "检查更新" => "Check for updates",
        "正在下载安装包…" => "Downloading the installer…",
        "正在后台安装更新…" => "Installing the update…",
        "正在检查更新…" => "Checking for updates…",
        "正在连接 RDP…" => "Connecting over RDP…",
        "此前记录的指纹" => "Fingerprint recorded earlier",
        "此链接来自终端输出，请确认目标地址后再打开：" => "This link came from terminal output. Check where it goes before opening it:",
        "活动转发" => "Active forwards",
        "浏览…" => "Browse…",
        "添加" => "Add",
        "添加转发" => "Add forward",
        "滚动历史行数" => "Scrollback lines",
        "确定" => "OK",
        "确认粘贴" => "Confirm paste",
        "端口转发" => "Port forwarding",
        "类型" => "Type",
        "粘贴" => "Paste",
        "粘贴多行内容前先确认" => "Confirm before pasting more than one line",
        "终端复制 / 粘贴（PuTTY 风格）" => "Terminal copy / paste (PuTTY style)",
        "自动信任新主机密钥（不逐个弹窗确认）" => "Trust new host keys automatically (no prompt each time)",
        "记录为纯文本（去除颜色/转义码，便于阅读和 grep）" => "Log as plain text (colours and escapes stripped, so it reads and greps cleanly)",
        "设置" => "Settings",
        "该版本暂无 Windows 安装包" => "That release has no Windows installer",
        "输出高亮" => "Output highlighting",
        "还没有片段。在下方添加常用命令，一键发送到当前终端。" => "No snippets yet. Add the commands you type often and send them to the terminal in one click.",
        "连接后自动开始记录日志" => "Start logging as soon as a session connects",
        "连接超时（秒，0 = 不限）" => "Connect timeout (seconds, 0 = no limit)",
        "选中内容即复制到剪贴板" => "Copy to the clipboard on selection",
        "选择一个云服务后，会话、分组与设置会在多台机器间合并同步。" => "Pick a cloud service and your sessions, groups and settings merge across machines.",
        "配置目录" => "Config folder",
        "配色方案" => "Colour scheme",
        "重命名标签" => "Rename tab",
        "重试" => "Retry",
        "重连" => "Reconnect",
        "需要一个带 gist 权限的 GitHub 个人访问令牌。GitHub 自带版本历史，可在网页端回滚。" => "Needs a GitHub personal access token with the gist scope. GitHub keeps version history, so you can roll back from the web.",
        "需要交互式验证" => "Interactive authentication required",
        "预览" => "Preview",
        "首次连接此主机。请通过其它可信渠道核对指纹后再信任。" => "First connection to this host. Check the fingerprint through another trusted channel before you trust it.",
        "（无）" => "(none)",
        "（暂无转发）" => "(no forwards)",
        "界面语言" => "Language",
        "切换后立即生效" => "Takes effect immediately",
        "应用" => "App",
        "外观" => "Appearance",
        "终端" => "Terminal",
        "日志" => "Logs",
        "同步与云" => "Sync & cloud",
        "云服务" => "Cloud services",
        "同步状态" => "Sync status",
        "未启用" => "Off",
        "S3 兼容存储" => "S3-compatible",
        "不同步，配置只留在本机" => "Not syncing; everything stays on this machine",
        "未使用" => "Not in use",
        "已连接" => "Connected",
        "尚未授权" => "Not authorised",
        "尚未填写凭据" => "No credentials yet",
        "使用中" => "In use",
        "使用" => "Use",
        "立即同步" => "Sync now",
        "同步中…" => "Syncing…",
        "连接账号" => "Connect account",
        "重新连接账号" => "Reconnect account",
        "正在等待浏览器授权…" => "Waiting for the browser…",
        "尚未连接" => "Not connected",
        "此构建未内置该云服务的 client id — 请在下方填写自己的" => "This build has no client id for that service — enter your own below",
        "留空则用本应用内置的" => "Blank uses the one built in",
        "Google 桌面客户端必需，留空则用内置的" => "Required for a Google desktop client; blank uses the built-in one",
        "仅访问本应用创建的文件，看不到你云端硬盘里的其他内容。" => "Reaches only files this app created; the rest of your Drive stays invisible to it.",
        "仅访问 Adit 自己的应用文件夹，碰不到其他文件。" => "Reaches only Adit's own app folder and nothing else.",
        "仅访问 Apps/Adit 文件夹，碰不到其他文件。" => "Reaches only the Apps/Adit folder and nothing else.",
        "侧边栏开关" => "Toggle sidebar",
        "深色模式开关" => "Toggle dark mode",
        "分屏（添加窗格）" => "Split pane",
        "垂直平铺（并排）" => "Tile vertically",
        "水平平铺（上下）" => "Tile horizontally",
        "网格平铺" => "Tile as grid",
        "合并为标签" => "Merge into tabs",
        "命令窗口" => "Command window",
        "输入广播开关" => "Toggle broadcast",
        "新建会话" => "New session",
        "新建分组" => "New group",
        "保存会话" => "Save session",
        "删除会话" => "Delete session",
        "按名称排序" => "Sort by name",
        "按主机排序" => "Sort by host",
        "导入 SecureCRT 会话…" => "Import SecureCRT sessions…",
        "设置…" => "Settings…",
        "关闭标签" => "Close tab",
        "连接" => "Connect",
        "断开" => "Disconnect",
        "自动重连开关" => "Toggle auto-reconnect",
        "主机密钥…" => "Host keys…",
        "打开演示标签" => "Open demo tab",
        "清屏" => "Clear screen",
        "记录日志开关" => "Toggle session log",
        "检查更新…" => "Check for updates…",
        "关于" => "About",
        other => other,
    }
}
