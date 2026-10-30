use super::*;
use std::collections::HashSet;

#[test]
fn clipboard_policy_request_confirmation_and_failure_have_both_languages() {
    for (zh, en) in [
        ("请求远程桌面共享剪贴板（VNC 仅基础文本）", "Request clipboard sharing with remote desktops (VNC: basic text only)"),
        ("这是请求值；RDP 确认结果在各会话窗口显示。", "This is the requested setting; each RDP session shows its own confirmation."),
        ("RDP 本地文件发送至远端暂时禁用。", "Sending local files to the remote desktop over RDP is temporarily disabled."),
        ("请求剪贴板共享", "Request clipboard sharing"),
        ("已请求开启远程桌面剪贴板共享", "Requested clipboard sharing for remote desktops"),
        ("已请求关闭远程桌面剪贴板共享", "Requested disabling clipboard sharing for remote desktops"),
        ("剪贴板：开启请求中", "Clipboard: enable request pending"),
        ("剪贴板：关闭请求中", "Clipboard: disable request pending"),
        ("剪贴板：已确认开启", "Clipboard: enable confirmed"),
        ("剪贴板：已确认关闭", "Clipboard: disable confirmed"),
        ("剪贴板：辅助进程已确认开启", "Clipboard: enable confirmed by helper"),
        ("剪贴板：辅助进程已确认关闭", "Clipboard: disable confirmed by helper"),
        ("剪贴板：开启请求失败", "Clipboard: enable request failed"),
        ("剪贴板：关闭请求失败", "Clipboard: disable request failed"),
        ("辅助进程已观察请求且剪贴板通道就绪，不代表远端应用已接收内容。", "The helper observed the request and the clipboard channel is ready; this does not mean the remote application received content."),
        ("辅助进程已断开", "Helper disconnected"),
        ("剪贴板通道未协商", "Clipboard channel was not negotiated"),
        ("等待确认超时", "Confirmation timed out"),
        ("共享策略不可用", "Shared policy unavailable"),
        ("辅助进程拒绝请求", "Helper rejected the request"),
        ("共享策略缓冲区已满", "Shared policy buffer limit reached"),
    ] {
        assert_eq!(translate(false, "", zh), zh);
        assert_eq!(translate(true, "", zh), en);
    }
}

#[test]
fn contextual_lookup_has_both_fallbacks_and_chinese_always_wins_when_selected() {
    assert_eq!(translate(true, "field", "主机"), "Host");
    assert_eq!(translate(true, "navigation", "主机"), "Hosts");
    assert_eq!(translate(true, "field", "分组"), "Group");
    assert_eq!(translate(true, "layout", "网格"), "Grid");
    assert_eq!(translate(true, "action", "应用"), "Apply");
    assert_eq!(translate(true, "navigation", "应用"), "App");
    assert_eq!(translate(true, "unknown-context", "取消"), "Cancel");
    assert_eq!(translate(true, "field", "尚未翻译的文案"), "尚未翻译的文案");
    assert_eq!(translate(false, "field", "主机"), "主机");
}

#[test]
fn template_values_are_inserted_once_even_when_they_contain_holes_or_chinese() {
    let value = r"C:\用户\{}";
    let template = translate(
        true,
        "",
        "已导入 {} 个会话，跳过 {} 个同分组同名会话；凭据未导入",
    );
    assert_eq!(fill(template, &[&3, &2]), "Imported 3 sessions; skipped 2 with matching group and name; credentials were not imported");
    assert_eq!(fill("{} | {}", &[&value, &"主机"]), r"C:\用户\{} | 主机");
    assert_eq!(fill("{} + {}", &[&"one"]), "one + {}");
    assert_eq!(fill("value {}", &[&"one", &"two"]), "value onetwo");
    assert_eq!(
        fill(translate(true, "field", "未翻译 {}"), &[&value]),
        format!("未翻译 {value}")
    );
}

#[test]
fn every_catalog_has_unique_sources_and_preserves_all_template_values() {
    let holes = regex::Regex::new(r"\{(?:[A-Za-z_][A-Za-z_0-9]*)?\}").unwrap();
    for table in [COMMON, RUNTIME] {
        let mut sources = HashSet::new();
        for &(zh, en) in table {
            assert!(sources.insert(zh), "duplicate source: {zh}");
            assert_eq!(
                holes.find_iter(zh).count(),
                holes.find_iter(en).count(),
                "{zh}"
            );
            assert!(
                !en.chars().any(|c| ('\u{3400}'..='\u{9fff}').contains(&c)),
                "{zh}"
            );
        }
    }
    let mut sources = HashSet::new();
    for &(context, zh, en) in CONTEXTUAL {
        assert!(sources.insert((context, zh)), "{context}: {zh}");
        assert_eq!(
            holes.find_iter(zh).count(),
            holes.find_iter(en).count(),
            "{zh}"
        );
    }
}

#[test]
fn runtime_translates_triggers_nested_errors_and_keeps_profile_names_opaque() {
    assert_eq!(
        runtime::render(true, "主机{}：触发器发送失败：会话已断开"),
        "主机{}: trigger send failed: Session disconnected"
    );
    assert_eq!(
        runtime::render(true, "取消：输出触发器已匹配"),
        "取消: output trigger matched"
    );
    assert_eq!(
        runtime::render(true, "第 12 行：Port 必须为 1–65535"),
        "Line 12: Port must be 1-65535"
    );
    assert_eq!(
        runtime::render(true, "第 3 行：引号未闭合"),
        "Line 3: Unclosed quotation mark"
    );
    assert_eq!(
        runtime::render(true, "连接中断，4 秒后重试（2/10）"),
        "Connection lost; retry in 4 seconds (2/10)"
    );
}

#[test]
fn runtime_keeps_unknown_diagnostics_paths_and_chinese_default_intact() {
    assert_eq!(
        runtime::render(true, "环境变量无效：{name}"),
        "Invalid environment variable: {name}"
    );
    let path = "C:\\用户\\资料{}\n第二行";
    assert_eq!(
        runtime::render(true, &format!("本地目录不存在: {path}")),
        format!("Local folder does not exist: {path}")
    );
    for message in [
        "unknown: 中文 {} bytes",
        "前缀 会话已断开 后缀",
        "保存",
        "自定义诊断",
    ] {
        assert_eq!(runtime::render(false, message), message);
    }
    assert_eq!(
        runtime::render(true, "unknown: 中文 {} bytes"),
        "unknown: 中文 {} bytes"
    );
    assert_eq!(
        runtime::render(true, "前缀 会话已断开 后缀"),
        "前缀 会话已断开 后缀"
    );
}

#[test]
fn session_options_validation_and_catalog_warnings_cross_the_gui_boundary() {
    let mut options = adit_domain::SessionOptions::default();
    options
        .environment
        .insert("BAD NAME".to_owned(), "值".to_owned());
    let error = options.validate().unwrap_err();
    assert_eq!(
        runtime::render(true, &error),
        "Invalid environment variable: BAD NAME"
    );
    assert_eq!(runtime::render(false, &error), error);
    assert_eq!(
        runtime::render(true, "生产：已移除自动触发器，请检查后自行配置"),
        "生产: automatic triggers removed; review and configure them manually"
    );
    assert_eq!(
        runtime::render(true, "远程显示器不能重叠"),
        "Remote monitors must not overlap"
    );
    assert_eq!(
        runtime::render(true, "凭据库已锁定，请先在设置中输入主密码解锁"),
        "Credential store locked; enter the master password in settings to unlock"
    );
}

#[test]
fn sftp_recovery_notices_translate_diagnostics_but_keep_paths_opaque() {
    assert_eq!(
        runtime::render(true, "发布中；中断时部分目标保留于 /保存，完整源保留于 /确定"),
        "Publishing; if interrupted, the partial destination remains at /保存 and the complete source at /确定"
    );
    assert_eq!(
        runtime::render(true, "未覆盖当前目标；恢复失败：续传目标不存在；备份 /生产/保存"),
        "The current destination was not overwritten; recovery failed: The resume destination does not exist; backup: /生产/保存"
    );
}

#[test]
fn rdp_file_safety_and_handshake_diagnostics_translate() {
    assert_eq!(
        runtime::render(true, "本地文件发送至远端已暂时禁用；桌面、文字、图片和远端文件接收仍可使用。已发送或已被传输层接收的数据仍可能到达。"),
        "Sending local files to the remote desktop is temporarily disabled; the desktop, text, images, and receiving remote files remain available. Data already sent or accepted by the transport may still arrive."
    );
    assert_eq!(
        runtime::render(true, "RDP 本地文件发送至远端已暂时禁用；文字、图片和远端文件接收仍可使用，桌面连接保持正常。"),
        "Sending local files to the remote desktop over RDP is temporarily disabled; text, images, and receiving remote files remain available, and the desktop connection remains active."
    );
    assert_eq!(
        runtime::render(true, "RDP 本地文件发送至远端已暂时禁用；无法安全确认剪贴板内容时，本地同步将暂停，远端文件接收仍可使用。"),
        "Sending local files to the remote desktop over RDP is temporarily disabled; local clipboard synchronization is paused when its contents cannot be verified, while receiving remote files remains available."
    );
    assert_eq!(
        runtime::render(true, "RDP 本地文件发送至远端已暂时禁用；无法安全确认剪贴板内容时，文字同步将暂停，远端文件接收仍可使用。"),
        "Sending local files to the remote desktop over RDP is temporarily disabled; text synchronization is paused when clipboard contents cannot be verified, while receiving remote files remains available."
    );
    assert_eq!(
        runtime::render(true, "文件剪贴板工作线程已退出；本地剪贴板同步已暂停"),
        "The file clipboard worker has stopped; local clipboard synchronization is paused"
    );
    assert_eq!(
        runtime::render(true, "文件剪贴板监听已退出；本地剪贴板同步已暂停"),
        "The file clipboard watcher has stopped; local clipboard synchronization is paused"
    );
    assert_eq!(
        runtime::render(true, "无法验证本地剪贴板快照；RDP 本地文字发送已暂停。"),
        "The local clipboard snapshot cannot be verified; sending local text over RDP is paused."
    );
    assert_eq!(
        runtime::render(true, "RDP 本地文件发送至远端已暂时禁用；检测到文件或未知内容时暂停本地文字发送，远端文件接收仍可使用。"),
        "Sending local files to the remote desktop over RDP is temporarily disabled; local text sending is paused when a file or unknown clipboard content is observed, while receiving remote files remains available."
    );
    assert_eq!(
        runtime::render(true, "无法确认当前本地剪贴板状态；RDP 本地文字发送已暂停。"),
        "The current local clipboard state is unavailable; sending local text over RDP is paused."
    );
    assert_eq!(
        runtime::render(true, "本地剪贴板文字超过 8 MiB，未发送到此 RDP 会话；远程桌面仍可使用。"),
        "The local clipboard text exceeds 8 MiB and was not sent to this RDP session. The desktop remains available."
    );
    assert_eq!(
        runtime::render(true, "本次剪贴板内容未发送；请重新复制后重试，远程桌面仍可使用。"),
        "The clipboard content was not sent. Copy it again to retry; the desktop remains available."
    );
    assert_eq!(
        runtime::render(true, "本地文件共享已停止；远端撤销尚未确认，已发送或正在发送的数据仍可能到达。"),
        "Local file sharing has stopped; remote revocation is not confirmed, and data already sent or in flight may still arrive."
    );
    assert_eq!(
        runtime::render(true, "等待辅助进程接收版本信息超时"),
        "Timed out waiting for the helper to receive version information"
    );
    assert_eq!(
        runtime::render(true, "RDP IPC 协商失败，请同时更新主程序和 RDP 辅助进程：RDP 连接已取消"),
        "RDP IPC negotiation failed; update both the application and RDP helper: RDP connection canceled"
    );
}

#[test]
fn local_file_binding_errors_keep_their_specific_translations() {
    use crate::clipboard_files::BindError;

    for (error, english) in [
        (
            BindError::SelectionLimit,
            "This selection exceeds 1,024 regular files; copy them in batches. The desktop remains available.",
        ),
        (
            BindError::HandleBudget,
            "The global limit of 2,048 file-sharing handles has been reached; previous files may still be closing. Copy again later. The desktop remains available.",
        ),
        (
            BindError::Unavailable,
            "Could not publish the RDP file clipboard",
        ),
    ] {
        assert_eq!(runtime::render(false, error.message()), error.message());
        assert_eq!(runtime::render(true, error.message()), english);
    }
}
