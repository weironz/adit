//! Shared wire/COM name validation. No filesystem probing or normalization:
//! unsafe input is rejected, never rewritten into a different destination.
use crate::ClipFile;
use std::collections::{HashMap, HashSet};

pub const MAX_CLIPBOARD_DESCRIPTORS: usize = 65_536;

pub fn validate_clipboard_relative_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.encode_utf16().count() > 259 {
        return Err("文件相对名称为空或超过描述符长度");
    }
    for component in name.split(['/', '\\']) {
        let stem = component
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end_matches(' ')
            .to_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|suffix| {
                    matches!(
                        suffix,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
            });
        if component.is_empty()
            || matches!(component, "." | "..")
            || component.ends_with(['.', ' '])
            || device
            || component
                .chars()
                .any(|c| c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        {
            return Err("拒绝绝对路径、父路径、设备名或非法文件相对名称");
        }
    }
    Ok(())
}

pub fn validate_clipboard_file_list(files: &[ClipFile]) -> Result<(), &'static str> {
    if files.len() > MAX_CLIPBOARD_DESCRIPTORS {
        return Err("文件描述数量超过安全上限");
    }
    let mut names = HashSet::new();
    let mut kinds = HashMap::new();
    for file in files {
        validate_clipboard_relative_name(&file.name)?;
        let key = file.name.replace('/', "\\").to_uppercase();
        if !names.insert(key.clone()) || file.size > i64::MAX as u64 {
            return Err("文件描述存在重复名称或不支持的长度");
        }
        kinds.insert(key, file.is_dir);
    }
    for name in names {
        let mut parent = name.as_str();
        while let Some((prefix, _)) = parent.rsplit_once('\\') {
            if kinds.get(prefix) == Some(&false) {
                return Err("普通文件不能作为其他文件的父目录");
            }
            parent = prefix;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn untrusted_relative_paths_are_rejected_without_rewriting() {
        for name in [
            "../x",
            "a\\..\\x",
            "/absolute",
            "\\absolute",
            "\\\\server\\share",
            "C:\\absolute",
            "C:relative",
            "a:b",
            "a//b",
            ".",
            "NUL.txt",
            "COM¹.txt",
            "a\\LPT9.txt",
            "a\0b",
            "x.",
        ] {
            assert!(validate_clipboard_relative_name(name).is_err(), "{name:?}");
        }
        assert!(validate_clipboard_relative_name("目录\\报告.txt").is_ok());
    }
    #[test]
    fn duplicate_conflicting_and_excessive_descriptors_fail_closed() {
        let file = |name: &str| ClipFile {
            name: name.into(),
            size: 3,
            is_dir: false,
        };
        assert!(validate_clipboard_file_list(&[file("A.txt"), file("a.txt")]).is_err());
        assert!(validate_clipboard_file_list(&[file("a"), file("a\\b")]).is_err());
        assert!(
            validate_clipboard_file_list(&vec![file("a"); MAX_CLIPBOARD_DESCRIPTORS + 1]).is_err()
        );
        assert!(validate_clipboard_file_list(&[ClipFile {
            size: u64::MAX,
            ..file("a")
        }])
        .is_err());
    }
}
