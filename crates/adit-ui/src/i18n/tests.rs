use super::*;
use std::collections::HashSet;

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
