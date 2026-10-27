use crate::{check_format_version, ProfileCatalog, StoredProfiles, PROFILES_FORMAT_VERSION};
use adit_domain::{ConnectionProfile, ProfileId, Protocol};
use std::{collections::HashMap, fs::File, io::Read, path::Path};

pub const MAX_IMPORT_BYTES: usize = 8 * 1024 * 1024;

/// Parsing is independent of the live stores and never opens keys or credentials.
#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub catalog: ProfileCatalog,
    pub warnings: Vec<String>,
}

pub fn read_import_file(path: &Path) -> Result<String, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take(MAX_IMPORT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    decode_import_text(&bytes)
}

/// Regedit exports UTF-16LE with a BOM; JSON and SSH config normally use UTF-8.
pub fn decode_import_text(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > MAX_IMPORT_BYTES {
        return Err("文件超过 8 MiB 导入限制".into());
    }
    let text = if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let big_endian = bytes[0] == 0xfe;
        let rest = &bytes[2..];
        if !rest.len().is_multiple_of(2) {
            return Err("UTF-16 文件长度无效".into());
        }
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| {
                if big_endian {
                    u16::from_be_bytes(*b)
                } else {
                    u16::from_le_bytes(*b)
                }
            })
            .collect();
        String::from_utf16(&units).map_err(|_| "无效的 UTF-16 编码".to_string())?
    } else {
        String::from_utf8(
            bytes
                .strip_prefix(&[0xef, 0xbb, 0xbf])
                .unwrap_or(bytes)
                .to_vec(),
        )
        .map_err(|_| "文件必须为 UTF-8 或带 BOM 的 UTF-16".to_string())?
    };
    if text.contains('\0') {
        return Err("文件包含 NUL 字符".into());
    }
    Ok(text)
}

// Deliberate allowlist: a future profile credential field must not become public
// just because ConnectionProfile gained a Serialize field.
const PUBLIC_FIELDS: &[&str] = &[
    "id",
    "group",
    "name",
    "host",
    "port",
    "username",
    "sort_order",
    "auth_method",
    "identity_file",
    "tunnels",
    "protocol",
    "vnc",
    "session_options",
    "terminal_type",
    "jumps",
    "environment",
    "accent_color",
    "icon",
    "label",
    "keep_connection_after_exit",
    "pinned",
    "pinned_order",
];
const VNC_FIELDS: &[&str] = &[
    "security",
    "tls_ca_file",
    "quality",
    "read_only",
    "clipboard_send",
    "clipboard_receive",
    "clipboard_legacy",
    "unicode_bmp_only",
    "auto_reconnect",
    "resize_remote",
    "gateway_profile",
];
const JUMP_FIELDS: &[&str] = &["host", "port", "username", "auth_profile"];
const SESSION_OPTION_FIELDS: &[&str] = &[
    "environment",
    "encoding",
    "rdp_quality",
    "rdp_monitors",
    "rdp_selected_monitor",
    "reconnect",
    "logging",
    "triggers",
    "terminal_appearance",
];
const APPEARANCE_FIELDS: &[&str] = &[
    "font_family",
    "font_size",
    "color_scheme",
    "custom_palette",
    "background",
];
const PALETTE_FIELDS: &[&str] = &["background", "foreground", "selection", "ansi"];
const MONITOR_FIELDS: &[&str] = &["left", "top", "width", "height", "primary", "scale_percent"];
const RECONNECT_FIELDS: &[&str] = &[
    "enabled",
    "max_attempts",
    "initial_delay_secs",
    "max_delay_secs",
];
const LOG_FIELDS: &[&str] = &[
    "auto_start",
    "directory",
    "filename_pattern",
    "plaintext",
    "record_input",
];
const TUNNEL_FIELDS: &[&str] = &[
    "kind",
    "bind_address",
    "bind_port",
    "target_host",
    "target_port",
];

fn check_profile_fields(value: &serde_json::Value) -> Result<(), String> {
    let object = value.as_object().ok_or("无效的会话对象")?;
    for key in object.keys() {
        if !PUBLIC_FIELDS.contains(&key.as_str()) && key != "folder" && key != "startup_command" {
            return Err(format!("尚未支持的会话字段: {key}；请使用支持该字段的版本"));
        }
    }
    for (field, allowed) in [
        ("vnc", VNC_FIELDS),
        ("session_options", SESSION_OPTION_FIELDS),
        ("jumps", JUMP_FIELDS),
        ("tunnels", TUNNEL_FIELDS),
    ] {
        let check = |item: &serde_json::Value| -> Result<(), String> {
            let item = item
                .as_object()
                .ok_or_else(|| format!("无效的 {field} 配置"))?;
            if let Some(key) = item.keys().find(|key| !allowed.contains(&key.as_str())) {
                return Err(format!("尚未支持的会话字段: {field}.{key}；未转换配置"));
            }
            Ok(())
        };
        if let Some(value) = object.get(field) {
            if field == "vnc" || field == "session_options" {
                check(value)?;
            } else {
                for item in value
                    .as_array()
                    .ok_or_else(|| format!("{field} 必须为列表"))?
                {
                    check(item)?;
                }
            }
        }
    }
    if let Some(options) = object.get("session_options").and_then(|v| v.as_object()) {
        if let Some(monitors) = options.get("rdp_monitors").filter(|v| !v.is_null()) {
            for monitor in monitors.as_array().ok_or("rdp_monitors 必须为列表")? {
                let fields = monitor.as_object().ok_or("无效的显示器配置")?;
                if fields
                    .keys()
                    .any(|key| !MONITOR_FIELDS.contains(&key.as_str()))
                {
                    return Err("显示器配置包含未知字段".into());
                }
            }
        }
        for (field, allowed) in [
            ("reconnect", RECONNECT_FIELDS),
            ("logging", LOG_FIELDS),
            ("terminal_appearance", APPEARANCE_FIELDS),
        ] {
            if let Some(value) = options.get(field).filter(|value| !value.is_null()) {
                let fields = value
                    .as_object()
                    .ok_or_else(|| format!("无效的 {field} 配置"))?;
                if let Some(key) = fields.keys().find(|key| !allowed.contains(&key.as_str())) {
                    return Err(format!("尚未支持的会话字段: session_options.{field}.{key}"));
                }
            }
        }
        if let Some(palette) = options
            .get("terminal_appearance")
            .and_then(|appearance| appearance.get("custom_palette"))
            .filter(|value| !value.is_null())
        {
            let fields = palette.as_object().ok_or("无效的自定义配色")?;
            if fields
                .keys()
                .any(|key| !PALETTE_FIELDS.contains(&key.as_str()))
            {
                return Err("自定义配色包含未知字段".into());
            }
        }
    }
    Ok(())
}

pub fn export_catalog(catalog: &ProfileCatalog) -> Result<String, String> {
    validate_jump_owners(&catalog.profiles)?;
    let mut profiles = Vec::new();
    for profile in &catalog.profiles {
        let mut value = serde_json::to_value(profile).map_err(|e| e.to_string())?;
        // Refuse an unknown future route field as well as excluding secrets:
        // stripping a new ProxyCommand policy could silently export a direct hop.
        check_profile_fields(&value)?;
        let object = value.as_object_mut().ok_or("无效的会话对象")?;
        object.retain(|name, _| PUBLIC_FIELDS.contains(&name.as_str()));
        if let Some(options) = object
            .get_mut("session_options")
            .and_then(|v| v.as_object_mut())
        {
            // Environment values are free-form and may contain passwords or tokens.
            options.insert("environment".into(), serde_json::json!({}));
            options.insert("triggers".into(), serde_json::json!([]));
        }
        profiles.push(value);
    }
    let normalized = ProfileCatalog::with_group_icons(
        catalog.groups.clone(),
        catalog.group_icons.clone(),
        catalog.profiles.clone(),
    );
    serde_json::to_string_pretty(&serde_json::json!({
        "version": PROFILES_FORMAT_VERSION,
        "groups": normalized.groups,
        "group_icons": normalized.group_icons,
        "profiles": profiles,
    }))
    .map_err(|e| e.to_string())
}

pub fn import_catalog(text: &str) -> Result<ImportReport, String> {
    if text.len() > MAX_IMPORT_BYTES {
        return Err("文件超过 8 MiB 导入限制".into());
    }
    check_format_version(text, "profile catalog", PROFILES_FORMAT_VERSION)
        .map_err(|e| e.to_string())?;
    // Unversioned catalogues predate StoredProfiles. Keep their folder alias and
    // defaulted profile fields compatible while still rejecting future versions.
    let mut value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let object = value.as_object_mut().ok_or("配置目录必须为 JSON 对象")?;
    object.entry("version").or_insert(serde_json::json!(0));
    // Never accept new transport/command fields without explicit review.
    for profile in object
        .get("profiles")
        .and_then(|v| v.as_array())
        .ok_or("缺少 profiles 列表")?
    {
        check_profile_fields(profile)?;
    }
    let mut document: StoredProfiles = serde_json::from_value(value).map_err(|e| e.to_string())?;
    let mut warnings = Vec::new();
    let mut ids = HashMap::new();
    for profile in &mut document.profiles {
        validate_profile(profile)?;
        let new_id = ProfileId::new();
        if ids.insert(profile.id, new_id).is_some() {
            return Err("配置包含重复的会话 ID".into());
        }
        // Fresh IDs prevent an imported catalogue from inheriting this machine's
        // vault entries. Commands may contain tokens, and never run on import.
        profile.id = new_id;
        if !profile.startup_command.is_empty() {
            warnings.push(format!("{}：已移除启动命令", profile.name));
            profile.startup_command.clear();
        }
        if !profile.session_options.environment.is_empty() {
            warnings.push(format!("{}：已移除环境变量值", profile.name));
            profile.session_options.environment.clear();
        }
        if !profile.session_options.triggers.is_empty() {
            warnings.push(format!(
                "{}：已移除自动触发器，请检查后自行配置",
                profile.name
            ));
            profile.session_options.triggers.clear();
        }
        if let Some(logging) = &mut profile.session_options.logging {
            if logging.auto_start || logging.record_input {
                warnings.push(format!(
                    "{}：自动日志和输入日志已关闭，请检查后自行启用",
                    profile.name
                ));
                logging.auto_start = false;
                logging.record_input = false;
            }
        }
        profile.session_options.validate()?;
    }
    for profile in &mut document.profiles {
        for hop in &mut profile.jumps {
            if let Some(old) = hop.auth_profile {
                hop.auth_profile = Some(*ids.get(&old).ok_or("跳板认证配置未包含在导入目录中")?);
            }
        }
        if let Some(old) = profile.vnc.gateway_profile {
            profile.vnc.gateway_profile = Some(*ids.get(&old).ok_or("VNC 网关未包含在导入目录中")?);
        }
    }
    validate_jump_owners(&document.profiles)?;
    Ok(ImportReport {
        catalog: ProfileCatalog::with_group_icons(
            document.groups,
            document.group_icons,
            document.profiles,
        ),
        warnings,
    })
}

fn validate_jump_owners(profiles: &[ConnectionProfile]) -> Result<(), String> {
    let owners: std::collections::HashSet<_> = profiles
        .iter()
        .filter(|p| p.protocol.is_ssh_based())
        .map(|p| p.id)
        .collect();
    for profile in profiles {
        if profile
            .jumps
            .iter()
            .filter_map(|hop| hop.auth_profile)
            .any(|id| !owners.contains(&id))
        {
            return Err(format!(
                "{}：跳板认证配置未包含在目录中或不是 SSH/SFTP 配置",
                profile.name
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_profile(profile: &ConnectionProfile) -> Result<(), String> {
    for (label, value) in [
        ("名称", profile.name.as_str()),
        ("主机", profile.host.as_str()),
        ("用户名", profile.username.as_str()),
        ("分组", profile.group.as_str()),
        ("密钥路径", profile.identity_file.as_str()),
    ] {
        if value.chars().any(char::is_control) {
            return Err(format!("{}：{label}包含控制字符", profile.name));
        }
    }
    if profile.name.trim().is_empty()
        || profile.host.trim().is_empty()
        || (profile.port == 0
            && !matches!(profile.protocol, Protocol::LocalShell | Protocol::Serial))
    {
        return Err("会话名称、主机或端口无效".into());
    }
    for hop in &profile.jumps {
        if hop.host.trim().is_empty()
            || hop.port == 0
            || hop
                .host
                .chars()
                .chain(hop.username.chars())
                .any(char::is_control)
        {
            return Err(format!("{}：无效的跳板配置", profile.name));
        }
    }
    Ok(())
}
