use crate::{catalog_transfer::validate_profile, ImportReport, MAX_IMPORT_BYTES};
use adit_domain::{AuthMethod, ConnectionProfile};
use std::collections::BTreeMap;

const SESSIONS: &str = r"HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions\";
const STRING_FIELDS: &[&str] = &[
    "HostName",
    "UserName",
    "Protocol",
    "PublicKeyFile",
    "RemoteCommand",
    "RemoteCommand2",
];
const DWORD_FIELDS: &[&str] = &["PortNumber", "ProxyMethod"];

/// This is the opt-in production adapter. Tests use exported registry data
/// instead, so they never inspect the signed-in user's PuTTY sessions.
#[cfg(windows)]
pub fn read_putty_sessions() -> Result<ImportReport, String> {
    use winreg::{
        enums::{HKEY_CURRENT_USER, KEY_READ, REG_DWORD, REG_SZ},
        RegKey,
    };
    let root = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(r"Software\SimonTatham\PuTTY\Sessions", KEY_READ)
        .map_err(|e| format!("无法只读打开 PuTTY Sessions：{e}"))?;
    let mut report = ImportReport::default();
    let mut total_bytes = 0usize;
    for (index, name) in root.enum_keys().enumerate() {
        if index >= 4096 {
            return Err("PuTTY 会话超过 4096 个导入限制".into());
        }
        let name = name.map_err(|e| e.to_string())?;
        let session = root
            .open_subkey_with_flags(&name, KEY_READ)
            .map_err(|e| e.to_string())?;
        let mut fields = BTreeMap::new();
        // Do not enumerate values: querying a fixed allowlist excludes saved
        // passwords and proxy credentials even when a fork stores them here.
        for field in STRING_FIELDS.iter().chain(DWORD_FIELDS) {
            let raw = match session.get_raw_value(field) {
                Ok(raw) => raw,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.to_string()),
            };
            total_bytes = total_bytes.saturating_add(raw.bytes.len());
            if total_bytes > MAX_IMPORT_BYTES {
                return Err("PuTTY 会话超过 8 MiB 导入限制".into());
            }
            let encoded = if DWORD_FIELDS.contains(field) {
                if raw.vtype != REG_DWORD || raw.bytes.len() != 4 {
                    return Err(format!("{field} 不是有效的 DWORD"));
                }
                let bytes: [u8; 4] = raw.bytes.as_slice().try_into().map_err(|_| "无效 DWORD")?;
                format!("dword:{:08x}", u32::from_le_bytes(bytes))
            } else {
                if raw.vtype != REG_SZ || !raw.bytes.len().is_multiple_of(2) {
                    return Err(format!("{field} 不是有效的字符串"));
                }
                let units: Vec<u16> = raw
                    .bytes
                    .chunks_exact(2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .collect();
                let text = String::from_utf16(&units).map_err(|_| "注册表字符串编码无效")?;
                let text = text.trim_end_matches('\0');
                format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
            };
            fields.insert((*field).to_string(), encoded);
        }
        flush(&mut report, Some(decode_session_name(&name)?), &mut fields)?;
    }
    Ok(report)
}

#[cfg(not(windows))]
pub fn read_putty_sessions() -> Result<ImportReport, String> {
    Err("直接读取 PuTTY 注册表仅适用于 Windows；请选择 .reg 导出文件".into())
}

/// Parse a Regedit export as data. Never import it into Windows, enumerate the
/// user's registry, or run a saved proxy/local/remote command.
pub fn parse_putty_reg(text: &str) -> Result<ImportReport, String> {
    if text.len() > MAX_IMPORT_BYTES || text.contains('\0') {
        return Err("PuTTY 导出文件过大或包含 NUL 字符".into());
    }
    let mut report = ImportReport::default();
    let mut name = None;
    let mut fields = BTreeMap::new();
    let mut header = false;
    for (index, raw) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if !header {
            if !matches!(line, "Windows Registry Editor Version 5.00" | "REGEDIT4") {
                return Err("不是 Windows 注册表导出文件".into());
            }
            header = true;
            continue;
        }
        if line.starts_with('[') {
            flush(&mut report, name.take(), &mut fields)?;
            let section = line
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .ok_or_else(|| format!("第 {} 行：注册表节格式无效", index + 1))?;
            if section.starts_with('-') {
                return Err("不支持包含删除操作的 .reg 文件".into());
            }
            if section
                .get(..SESSIONS.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(SESSIONS))
            {
                let encoded = &section[SESSIONS.len()..];
                if encoded.is_empty() || encoded.contains('\\') {
                    return Err("无效的 PuTTY 会话注册表路径".into());
                }
                name = Some(decode_session_name(encoded)?);
            }
            continue;
        }
        if name.is_none() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("第 {} 行：注册表值格式无效", index + 1));
        };
        let key =
            reg_string(key.trim()).map_err(|_| format!("第 {} 行：注册表键格式无效", index + 1))?;
        // Ignore all unrelated settings, including arbitrary binary and encrypted
        // credential data; only explicitly supported strings/DWORDs are parsed.
        if let Some(canonical) = STRING_FIELDS
            .iter()
            .chain(DWORD_FIELDS)
            .find(|known| known.eq_ignore_ascii_case(&key))
        {
            if fields
                .insert((*canonical).to_string(), value.trim().to_string())
                .is_some()
            {
                return Err(format!("第 {} 行：重复的 PuTTY 设置", index + 1));
            }
        }
    }
    flush(&mut report, name, &mut fields)?;
    if !header {
        return Err("空的 PuTTY 注册表导出文件".into());
    }
    Ok(report)
}

fn flush(
    report: &mut ImportReport,
    name: Option<String>,
    fields: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let Some(name) = name else {
        fields.clear();
        return Ok(());
    };
    let values = std::mem::take(fields);
    if name == "Default Settings" {
        return Ok(());
    }
    let string = |key: &str| -> Result<String, String> {
        values
            .get(key)
            .map(|s| reg_string(s))
            .unwrap_or_else(|| Ok(String::new()))
    };
    let dword = |key: &str, default| -> Result<u32, String> {
        match values.get(key) {
            None => Ok(default),
            Some(value) => value
                .strip_prefix("dword:")
                .filter(|hex| hex.len() == 8)
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .ok_or_else(|| format!("{name}：{key} 必须为 DWORD")),
        }
    };
    let protocol = string("Protocol")?;
    if !protocol.is_empty() && !protocol.eq_ignore_ascii_case("ssh") {
        report
            .warnings
            .push(format!("{name}：跳过非 SSH PuTTY 会话"));
        return Ok(());
    }
    if dword("ProxyMethod", 0)? != 0 {
        report.warnings.push(format!(
            "{name}：跳过使用 PuTTY 代理的会话，避免丢失代理路径"
        ));
        return Ok(());
    }
    let port = u16::try_from(dword("PortNumber", 22)?)
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| format!("{name}：PortNumber 必须为 1–65535"))?;
    let host = string("HostName")?;
    let identity = string("PublicKeyFile")?;
    let mut profile = ConnectionProfile::with_group("PuTTY", name, host, port, string("UserName")?);
    if profile.username.is_empty() {
        profile.username = crate::current_username().unwrap_or_default();
    }
    profile.auth_method = if identity.is_empty() {
        AuthMethod::Auto
    } else {
        AuthMethod::Key
    };
    profile.identity_file = identity;
    validate_profile(&profile)?;
    if !string("RemoteCommand")?.is_empty() || !string("RemoteCommand2")?.is_empty() {
        report
            .warnings
            .push(format!("{}：已移除 PuTTY 远端启动命令", profile.name));
    }
    report.catalog.profiles.push(profile);
    if report.catalog.groups.is_empty() {
        report.catalog.groups.push("PuTTY".into());
    }
    Ok(())
}

fn reg_string(value: &str) -> Result<String, String> {
    let rest = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .ok_or("注册表值必须为字符串")?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(c @ ('\\' | '"')) => out.push(c),
                _ => return Err("注册表字符串转义无效".into()),
            },
            '"' => return Err("注册表字符串引号无效".into()),
            _ => out.push(c),
        }
    }
    Ok(out)
}

fn decode_session_name(encoded: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    let raw = encoded.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' {
            let hex = raw
                .get(index + 1..index + 3)
                .ok_or("PuTTY 会话名百分号编码无效")?;
            let hex = std::str::from_utf8(hex).map_err(|_| "PuTTY 会话名编码无效")?;
            bytes.push(u8::from_str_radix(hex, 16).map_err(|_| "PuTTY 会话名编码无效")?);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    let name = String::from_utf8(bytes).map_err(|_| "PuTTY 会话名必须为 UTF-8")?;
    if name.trim().is_empty() || name.chars().any(char::is_control) {
        return Err("PuTTY 会话名为空或包含控制字符".into());
    }
    Ok(name)
}
