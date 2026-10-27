use crate::{catalog_transfer::validate_profile, ImportReport, ProfileCatalog, MAX_IMPORT_BYTES};
use adit_domain::{AuthMethod, ConnectionProfile, JumpHop};
use std::{collections::BTreeSet, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshConfigHost {
    pub alias: String,
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub identity_file: String,
    pub jumps: Vec<JumpHop>,
}

/// Compatibility API; callers needing diagnostics must use the report API.
/// A malformed/unsupported route never degrades to a direct connection.
#[must_use]
pub fn parse_ssh_config(text: &str) -> Vec<SshConfigHost> {
    parse_hosts(text, crate::home_dir().as_deref(), "").unwrap_or_default()
}

pub fn parse_ssh_config_report(text: &str) -> Result<ImportReport, String> {
    let user = crate::current_username().unwrap_or_default();
    let hosts = parse_hosts(text, crate::home_dir().as_deref(), &user)?;
    let profiles = hosts
        .into_iter()
        .map(|host| {
            let mut profile = ConnectionProfile::with_group(
                "Imported",
                host.alias,
                host.hostname,
                host.port,
                host.user,
            );
            profile.auth_method = if host.identity_file.is_empty() {
                AuthMethod::Auto
            } else {
                AuthMethod::Key
            };
            profile.identity_file = host.identity_file;
            profile.jumps = host.jumps;
            profile
        })
        .collect();
    Ok(ImportReport {
        catalog: ProfileCatalog::from_profiles(profiles),
        warnings: Vec::new(),
    })
}

#[derive(Default)]
struct Block {
    patterns: Vec<String>,
    options: Vec<(usize, String, Vec<String>)>,
}

#[derive(Default)]
struct Effective {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identities: Vec<String>,
    // ProxyJump and ProxyCommand compete: the first obtained one wins.
    proxy: Option<(String, String)>,
}

fn parse_hosts(
    text: &str,
    home: Option<&Path>,
    fallback_user: &str,
) -> Result<Vec<SshConfigHost>, String> {
    if text.len() > MAX_IMPORT_BYTES || text.contains('\0') {
        return Err("SSH config 过大或包含 NUL 字符".into());
    }
    let mut blocks = vec![Block::default()];
    let mut aliases = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let end = line
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(line.len());
        let key = line[..end].to_ascii_lowercase();
        let rest = line[end..].trim_start();
        let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
        let values = tokens(rest).map_err(|e| format!("第 {} 行：{e}", index + 1))?;
        if values.is_empty() {
            return Err(format!("第 {} 行：{key} 缺少值", index + 1));
        }
        match key.as_str() {
            "host" => {
                for pattern in &values {
                    if !pattern.starts_with('!')
                        && !pattern.contains(['*', '?'])
                        && seen.insert(pattern.to_ascii_lowercase())
                    {
                        aliases.push(pattern.clone());
                    }
                }
                blocks.push(Block {
                    patterns: values,
                    options: Vec::new(),
                });
            }
            // Evaluating these partially could erase the route inherited from
            // another file or conditional block. Refuse with a visible reason.
            "include" | "match" => {
                return Err(format!(
                    "第 {} 行：暂不支持 {key}，请先展开为独立配置后导入；未导入任何会话",
                    index + 1
                ))
            }
            _ => blocks
                .last_mut()
                .expect("initial block")
                .options
                .push((index + 1, key, values)),
        }
    }
    let mut hosts = Vec::new();
    if aliases.len() > 4096 {
        return Err("SSH config 超过 4096 个主机导入限制".into());
    }
    for alias in aliases {
        let effective = effective(&blocks, &alias)?;
        let user = effective
            .user
            .clone()
            .unwrap_or_else(|| fallback_user.to_string());
        let hostname = expand(
            effective.hostname.as_deref().unwrap_or(&alias),
            &alias,
            &alias,
            &user,
            effective.port.unwrap_or(22),
            home,
        )?;
        validate_endpoint(&hostname)?;
        let port = effective.port.unwrap_or(22);
        let identity = identity(&effective, &alias, &hostname, &user, port, home)?;
        let mut visiting = BTreeSet::from([alias.to_ascii_lowercase()]);
        let jumps = resolve_jumps(
            &blocks,
            &effective,
            &identity,
            home,
            fallback_user,
            &mut visiting,
        )?;
        let host = SshConfigHost {
            alias,
            hostname,
            user,
            port,
            identity_file: identity,
            jumps,
        };
        let mut profile =
            ConnectionProfile::with_group("", &host.alias, &host.hostname, port, &host.user);
        profile.jumps = host.jumps.clone();
        validate_profile(&profile)?;
        hosts.push(host);
    }
    Ok(hosts)
}

fn effective(blocks: &[Block], alias: &str) -> Result<Effective, String> {
    let mut result = Effective::default();
    for block in blocks {
        if !block.patterns.is_empty() && !matches_patterns(&block.patterns, alias) {
            continue;
        }
        for (line, key, values) in &block.options {
            let single = || -> Result<&str, String> {
                if values.len() == 1 {
                    Ok(&values[0])
                } else {
                    Err(format!("第 {line} 行：{key} 只能有一个值"))
                }
            };
            match key.as_str() {
                "hostname" if result.hostname.is_none() => result.hostname = Some(single()?.into()),
                "user" if result.user.is_none() => result.user = Some(single()?.into()),
                "port" if result.port.is_none() => {
                    result.port = Some(
                        single()?
                            .parse::<u16>()
                            .ok()
                            .filter(|p| *p != 0)
                            .ok_or_else(|| format!("第 {line} 行：Port 必须为 1–65535"))?,
                    );
                }
                "identityfile" => result.identities.push(single()?.into()),
                "proxyjump" | "proxycommand" if result.proxy.is_none() => {
                    result.proxy = Some((key.clone(), values.join(" ")));
                }
                // Metadata import never evaluates local/remote commands.
                "localcommand" | "remotecommand" | "permitlocalcommand"
                    if key != "permitlocalcommand" || !single()?.eq_ignore_ascii_case("no") => {
                    return Err(format!(
                        "第 {line} 行：配置含 {key} 命令，导入不会执行命令，请移除后重试"
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(result)
}

fn resolve_jumps(
    blocks: &[Block],
    config: &Effective,
    target_identity: &str,
    home: Option<&Path>,
    fallback_user: &str,
    visiting: &mut BTreeSet<String>,
) -> Result<Vec<JumpHop>, String> {
    let Some((kind, value)) = &config.proxy else {
        return Ok(Vec::new());
    };
    if value.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    if kind == "proxycommand" {
        return Err("ProxyCommand 传输尚未支持（#104 未完成）；未导入任何会话，未执行命令".into());
    }
    let mut result = Vec::new();
    for spec in value.split(',') {
        let spec = spec.trim();
        let spec = spec.strip_prefix("ssh://").unwrap_or(spec);
        let mut hop = JumpHop::parse(spec).ok_or("无效的 ProxyJump 主机或端口")?;
        validate_endpoint(&hop.host)?;
        if spec.contains('@') && hop.username.is_empty() {
            return Err("ProxyJump 用户名为空".into());
        }
        let alias = hop.host.to_ascii_lowercase();
        if visiting.len() >= 16 || !visiting.insert(alias.clone()) {
            return Err("ProxyJump 存在循环或超过 16 层".into());
        }
        let resolved = effective(blocks, &hop.host)?;
        let explicit_port = spec.rsplit('@').next().is_some_and(|address| {
            address
                .rsplit_once(']')
                .is_some_and(|(_, after)| after.starts_with(':'))
                || (!address.starts_with('[') && address.matches(':').count() == 1)
        });
        if !explicit_port {
            hop.port = resolved.port.unwrap_or(22);
        }
        if !spec.contains('@') {
            hop.username = resolved
                .user
                .clone()
                .unwrap_or_else(|| fallback_user.into());
        }
        let original = hop.host.clone();
        hop.host = expand(
            resolved.hostname.as_deref().unwrap_or(&original),
            &original,
            &original,
            &hop.username,
            hop.port,
            home,
        )?;
        validate_endpoint(&hop.host)?;
        let hop_identity = identity(
            &resolved,
            &original,
            &hop.host,
            &hop.username,
            hop.port,
            home,
        )?;
        // The transport currently shares the profile identity across hops.
        if !hop_identity.is_empty() && hop_identity != target_identity {
            return Err(format!("{original}：跳板使用独立 IdentityFile，当前接口不能保存，请配置相同密钥或等待 per-hop identity 支持"));
        }
        // In a comma-separated -J chain, the preceding hops override a later
        // hop's own ProxyJump/ProxyCommand. Only the first hop inherits its
        // independently configured route (the same nesting ssh constructs).
        if result.is_empty() {
            result.extend(resolve_jumps(
                blocks,
                &resolved,
                target_identity,
                home,
                fallback_user,
                visiting,
            )?);
        }
        result.push(hop);
        visiting.remove(&alias);
        if result.len() > 16 {
            return Err("ProxyJump 链超过 16 个跳板".into());
        }
    }
    Ok(result)
}

fn identity(
    config: &Effective,
    alias: &str,
    host: &str,
    user: &str,
    port: u16,
    home: Option<&Path>,
) -> Result<String, String> {
    let mut identities = BTreeSet::new();
    for raw in &config.identities {
        if !raw.eq_ignore_ascii_case("none") {
            identities.insert(expand(raw, alias, host, user, port, home)?);
        }
    }
    if identities.len() > 1 {
        return Err(format!("{alias}：多个 IdentityFile 无法保存到单一密钥字段"));
    }
    Ok(identities.into_iter().next().unwrap_or_default())
}

fn expand(
    raw: &str,
    alias: &str,
    host: &str,
    user: &str,
    port: u16,
    home: Option<&Path>,
) -> Result<String, String> {
    if raw.contains("${")
        || (raw.starts_with('~') && !raw.starts_with("~/") && !raw.starts_with("~\\"))
    {
        return Err("暂不支持该环境变量或 ~user 路径展开".into());
    }
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('h') => out.push_str(host),
            Some('n') => out.push_str(alias),
            Some('r') => out.push_str(user),
            Some('p') => out.push_str(&port.to_string()),
            Some('d') => out.push_str(
                home.ok_or("无法展开 %d：缺少主目录")?
                    .to_str()
                    .ok_or("主目录编码无效")?,
            ),
            _ => return Err("不支持的 SSH config % 标记".into()),
        }
    }
    if let Some(rest) = out.strip_prefix("~/").or_else(|| out.strip_prefix("~\\")) {
        return Ok(home
            .ok_or("无法展开 ~：缺少主目录")?
            .join(rest)
            .to_string_lossy()
            .into_owned());
    }
    Ok(out)
}

fn validate_endpoint(host: &str) -> Result<(), String> {
    if host.is_empty()
        || host.starts_with('-')
        || host.chars().any(|c| c.is_whitespace() || c.is_control())
        || host.contains(['/', '\\', '@', ',', '%'])
    {
        return Err("无效的主机名或 ProxyJump 别名".into());
    }
    if (host.contains(':') && host.parse::<std::net::Ipv6Addr>().is_err())
        || host.contains(['[', ']'])
    {
        return Err("IPv6 主机地址格式无效".into());
    }
    Ok(())
}

fn matches_patterns(patterns: &[String], alias: &str) -> bool {
    let mut positive = false;
    for pattern in patterns {
        if let Some(negative) = pattern.strip_prefix('!') {
            if glob(negative, alias) {
                return false;
            }
        } else if glob(pattern, alias) {
            positive = true;
        }
    }
    positive
}

// Iterative wildcard matching avoids recursion on adversarial Host patterns.
fn glob(pattern: &str, value: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let value = value.to_ascii_lowercase();
    let (p, v) = (pattern.as_bytes(), value.as_bytes());
    let (mut pi, mut vi, mut star, mut retry) = (0, 0, None, 0);
    while vi < v.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            retry = vi;
        } else if let Some(index) = star {
            retry += 1;
            vi = retry;
            pi = index + 1;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn tokens(raw: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = None;
    let mut present = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '#' if quoted.is_none() => break,
            '"' | '\'' if quoted == Some(c) => quoted = None,
            '"' | '\'' if quoted.is_none() => {
                quoted = Some(c);
                present = true;
            }
            '\\' if chars
                .peek()
                .is_some_and(|next| matches!(next, '"' | '\'' | '\\' | '#' | ' ' | '\t')) =>
            {
                word.push(chars.next().expect("peeked"));
                present = true;
            }
            c if c.is_whitespace() && quoted.is_none() => {
                if present {
                    words.push(std::mem::take(&mut word));
                    present = false;
                }
            }
            _ => {
                word.push(c);
                present = true;
            }
        }
    }
    if quoted.is_some() {
        return Err("引号未闭合".into());
    }
    if present {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_value_wins_and_wildcards_and_negation_apply_without_creating_profiles() {
        let hosts = parse_hosts(
            "User global\nHost web1 web2\n HostName %h.test\n Port=2222 # comment\n\
             Host !web2 web*\n IdentityFile \"~/keys/%n key\"\nHost *\n Port 22\n",
            Some(Path::new("fixture-home")),
            "",
        )
        .unwrap();
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].user, "global");
        assert_eq!(hosts[0].port, 2222);
        assert_eq!(hosts[0].hostname, "web1.test");
        assert_eq!(
            hosts[0].identity_file,
            Path::new("fixture-home")
                .join("keys/web1 key")
                .to_string_lossy()
        );
        assert!(hosts[1].identity_file.is_empty());
    }

    #[test]
    fn jump_alias_uses_its_own_user_port_and_hostname() {
        let hosts = parse_hosts(
            "Host dest\n ProxyJump jump\nHost jump\n HostName real.test\n User ops\n Port 2022",
            None,
            "local",
        )
        .unwrap();
        assert_eq!(
            hosts[0].jumps,
            vec![JumpHop {
                host: "real.test".into(),
                username: "ops".into(),
                port: 2022,
            }]
        );
    }

    #[test]
    fn proxy_none_and_first_proxy_directive_follow_openssh_precedence() {
        let hosts = parse_hosts(
            "Host dest\n ProxyJump none\n ProxyCommand should-never-run\n",
            None,
            "",
        )
        .unwrap();
        assert!(hosts[0].jumps.is_empty());
        assert!(parse_hosts(
            "Host dest\n ProxyCommand should-never-run\n ProxyJump none\n",
            None,
            "",
        )
        .is_err());
    }

    #[test]
    fn nested_jump_routes_are_explicit_and_cannot_loop() {
        let hosts = parse_hosts(
            "Host dest\n ProxyJump middle\nHost middle\n ProxyJump first\nHost first\n Port 2222",
            None,
            "local",
        )
        .unwrap();
        assert_eq!(
            hosts[0]
                .jumps
                .iter()
                .map(|h| h.host.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "middle"]
        );
        assert_eq!(hosts[0].jumps[0].port, 2222);
    }

    #[test]
    fn an_explicit_chain_overrides_a_later_hops_separate_route() {
        let hosts = parse_hosts(
            "Host dest\n ProxyJump first,middle\nHost middle\n ProxyJump other\nHost first other\n",
            None,
            "local",
        )
        .unwrap();
        assert_eq!(
            hosts[0]
                .jumps
                .iter()
                .map(|h| h.host.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "middle"]
        );
    }

    #[test]
    fn quoting_comments_windows_paths_and_escaped_hash_are_data() {
        let hosts = parse_hosts(
            r#"Host web
HostName=web.test
IdentityFile "C:\fixture\key #1"
User 'operator' # comment
"#,
            None,
            "",
        )
        .unwrap();
        assert_eq!(hosts[0].identity_file, r"C:\fixture\key #1");
        assert_eq!(hosts[0].user, "operator");
        assert_eq!(tokens(r"a\#b # trailing").unwrap(), vec!["a#b"]);
    }
}
