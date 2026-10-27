use adit_domain::{ConnectionProfile, JumpHop, ProfileId, Protocol, TunnelDef, TunnelKind};
use adit_storage::{
    decode_import_text, export_catalog, import_catalog, parse_putty_reg, parse_ssh_config_report,
    read_import_file, ProfileCatalog, ProfileStore, MAX_IMPORT_BYTES,
};

#[test]
fn catalog_transfer_preserves_public_policy_and_never_exports_environment_values() {
    let mut profile = ConnectionProfile::new("fixture", "example.test", 22, "fixture");
    profile.session_options.environment.insert("API_TOKEN".into(), "fixture-secret-value".into());
    profile.session_options.encoding = adit_domain::TerminalEncoding::Gbk;
    profile.session_options.reconnect = Some(adit_domain::ReconnectPolicy {
        enabled: true, max_attempts: 4, initial_delay_secs: 2, max_delay_secs: 20,
    });
    profile.session_options.logging = Some(adit_domain::LogPolicy {
        auto_start: true, record_input: true, ..Default::default()
    });
    let exported = export_catalog(&ProfileCatalog::from_profiles(vec![profile])).unwrap();
    assert!(!exported.contains("fixture-secret-value"));
    let imported = import_catalog(&exported).unwrap();
    let options = &imported.catalog.profiles[0].session_options;
    assert!(options.environment.is_empty());
    assert_eq!(options.encoding, adit_domain::TerminalEncoding::Gbk);
    assert_eq!(options.reconnect.as_ref().unwrap().max_attempts, 4);
    assert!(!options.logging.as_ref().unwrap().auto_start);
    assert!(!options.logging.as_ref().unwrap().record_input);
    assert!(!imported.warnings.is_empty());
}

const PUTTY: &str = include_str!("fixtures/putty-sessions.reg");

#[test]
fn putty_export_is_data_only_and_preserves_encoded_names_ports_and_key_paths() {
    let report = parse_putty_reg(PUTTY).unwrap();
    assert_eq!(report.catalog.profiles.len(), 3);
    let profile = &report.catalog.profiles[0];
    assert_eq!(profile.name, "prod web%");
    assert_eq!(profile.host, "web.example.test");
    assert_eq!(profile.port, 2222);
    assert_eq!(profile.username, "deploy");
    assert_eq!(profile.identity_file, r"C:\fixture\keys\id_ed25519");
    assert!(profile.startup_command.is_empty());
    assert_eq!(report.catalog.profiles[1].name, "测试");
    assert_eq!(report.catalog.profiles[1].host, "2001:db8::1");
    assert_eq!(
        report.catalog.profiles[2].identity_file,
        r"C:\fixture\putty.ppk"
    );
    assert_eq!(report.warnings.len(), 3);
    let exported = export_catalog(&report.catalog).unwrap();
    for forbidden in [
        "fixture-secret",
        "fixture-proxy-secret",
        "should-never-run",
        "Password",
        "ProxyTelnetCommand",
    ] {
        assert!(!exported.contains(forbidden), "{forbidden} leaked");
    }
}

#[test]
fn registry_names_are_case_insensitive_so_proxy_settings_cannot_be_bypassed() {
    let report = parse_putty_reg(
        &PUTTY
            .replace("\"ProxyMethod\"", "\"pRoXyMeThOd\"")
            .replace("\"HostName\"", "\"hostname\""),
    )
    .unwrap();
    assert_eq!(report.catalog.profiles.len(), 3);
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("PuTTY 代理")));
}

#[test]
fn regedit_utf16_and_utf8_bom_and_regedit4_are_supported() {
    let mut utf16 = vec![0xff, 0xfe];
    for unit in PUTTY.encode_utf16() {
        utf16.extend(unit.to_le_bytes());
    }
    let decoded = decode_import_text(&utf16).unwrap();
    assert_eq!(parse_putty_reg(&decoded).unwrap().catalog.profiles.len(), 3);
    let mut utf8 = vec![0xef, 0xbb, 0xbf];
    utf8.extend(PUTTY.as_bytes());
    assert_eq!(decode_import_text(&utf8).unwrap(), PUTTY);
    assert!(
        parse_putty_reg(&PUTTY.replace("Windows Registry Editor Version 5.00", "REGEDIT4")).is_ok()
    );
}

#[test]
fn malicious_or_corrupt_registry_input_is_rejected() {
    for text in [
        PUTTY.replace("prod%20web%25", "bad%0Aname"),
        PUTTY.replace("prod%20web%25", "bad%QZ"),
        PUTTY.replace("dword:000008ae", "dword:00000000"),
        PUTTY.replace("dword:000008ae", "dword:00010000"),
        PUTTY.replace(
            "\"HostName\"=\"web.example.test\"",
            "\"HostName\"=\"web.example.test\"\n\"HostName\"=\"again\"",
        ),
        PUTTY.replace("[HKEY_CURRENT_USER", "[-HKEY_CURRENT_USER"),
        PUTTY.replace(r"C:\\fixture\\keys\\id_ed25519", r"C:\bad-escape"),
    ] {
        assert!(parse_putty_reg(&text).is_err());
    }
    assert!(decode_import_text(&[0xff, 0xfe, 1]).is_err());
    assert!(decode_import_text(&[0xff, 0xfe, 0, 0xd8]).is_err());
    assert!(decode_import_text(b"a\0b").is_err());
    assert!(decode_import_text(&vec![b'a'; MAX_IMPORT_BYTES + 1]).is_err());
}

#[test]
fn export_roundtrip_preserves_catalog_metadata_but_never_commands_or_credentials() {
    let mut profile = ConnectionProfile::with_group("prod", "web", "example.test", 2222, "ops");
    profile.startup_command = "TOKEN=fixture-secret do-not-run".into();
    profile.identity_file = "fixture/id_ed25519".into();
    profile.jumps = vec![JumpHop::parse("jump@bastion.test:2200").unwrap()];
    profile.tunnels = vec![TunnelDef {
        kind: TunnelKind::Local,
        bind_address: "127.0.0.2".into(),
        bind_port: 8080,
        target_host: "database.test".into(),
        target_port: 5432,
    }];
    let old_id = profile.id;
    let mut catalog = ProfileCatalog::new(vec!["empty".into()], vec![profile]);
    catalog.group_icons.insert("prod".into(), "server".into());
    let export = export_catalog(&catalog).unwrap();
    assert!(!export.contains("fixture-secret"));
    assert!(!export.contains("startup_command"));
    assert!(!export.contains("credentials"));
    assert!(!export.contains("password"));
    let imported = import_catalog(&export).unwrap().catalog;
    assert_eq!(imported.groups, catalog.groups);
    assert_eq!(imported.group_icons, catalog.group_icons);
    assert_ne!(imported.profiles[0].id, old_id);
    assert_eq!(imported.profiles[0].jumps, catalog.profiles[0].jumps);
    assert_eq!(imported.profiles[0].tunnels, catalog.profiles[0].tunnels);
    assert_eq!(imported.profiles[0].identity_file, "fixture/id_ed25519");
    assert!(imported.profiles[0].startup_command.is_empty());
}

#[test]
fn legacy_catalog_folder_alias_and_missing_defaults_are_compatible() {
    let json = serde_json::json!({
        "profiles": [{
            "id": ProfileId::new(),
            "folder": "legacy",
            "name": "old", "host": "old.test", "port": 22, "username": "ops"
        }]
    })
    .to_string();
    let report = import_catalog(&json).unwrap();
    assert_eq!(report.catalog.groups, vec!["legacy"]);
    assert!(report.catalog.profiles[0].jumps.is_empty());
    let mut future: serde_json::Value = serde_json::from_str(&json).unwrap();
    future["version"] = 999.into();
    assert!(import_catalog(&future.to_string())
        .unwrap_err()
        .contains("newer Adit"));
}

#[test]
fn untrusted_catalog_commands_are_removed_and_invalid_profiles_are_rejected() {
    let mut profile = ConnectionProfile::new("web", "web.test", 22, "ops");
    profile.startup_command = "fixture-secret command".into();
    let mut value = serde_json::json!({"version": 2, "profiles": [profile]});
    let imported = import_catalog(&value.to_string()).unwrap();
    assert_eq!(imported.warnings.len(), 1);
    assert!(imported.catalog.profiles[0].startup_command.is_empty());
    value["profiles"][0]["port"] = 0.into();
    assert!(import_catalog(&value.to_string()).is_err());
    value["profiles"][0]["port"] = 22.into();
    value["profiles"][0]["proxy_command"] = "cmd.exe /c never-run".into();
    assert!(import_catalog(&value.to_string()).is_err());
}

#[test]
fn unknown_nested_route_or_credential_fields_are_not_silently_dropped() {
    let profile = ConnectionProfile::new("web", "web.test", 22, "ops");
    let value = serde_json::json!({"version": 2, "profiles": [profile]});
    for (field, nested) in [
        (
            "jumps",
            serde_json::json!([{"host": "jump", "username": "ops", "identity_file": "hop-key"}]),
        ),
        (
            "tunnels",
            serde_json::json!([{"kind": "local", "bind_port": 42, "password": "fixture-secret"}]),
        ),
        (
            "vnc",
            serde_json::json!({"proxy_command": "cmd.exe /c never-run"}),
        ),
    ] {
        let mut incoming = value.clone();
        incoming["profiles"][0][field] = nested;
        assert!(import_catalog(&incoming.to_string()).is_err(), "{field}");
    }
}

#[test]
fn catalog_protocol_metadata_roundtrips_and_startup_commands_are_never_restored() {
    for protocol in Protocol::ALL {
        let mut profile = ConnectionProfile::new("fixture", "fixture-host-or-program", 22, "ops");
        profile.protocol = protocol;
        profile.startup_command = "never-run".into();
        if matches!(protocol, Protocol::LocalShell | Protocol::Serial) {
            profile.port = 0;
        }
        let report =
            import_catalog(&export_catalog(&ProfileCatalog::from_profiles(vec![profile])).unwrap())
                .unwrap();
        assert_eq!(report.catalog.profiles[0].protocol, protocol);
        assert!(report.catalog.profiles[0].startup_command.is_empty());
    }
}

#[test]
fn gateway_ids_are_remapped_and_missing_or_duplicate_ids_are_rejected() {
    let gateway = ConnectionProfile::new("gateway", "gateway.test", 22, "ops");
    let mut desktop = ConnectionProfile::new("desktop", "desktop.test", 5900, "ops");
    desktop.vnc.gateway_profile = Some(gateway.id);
    let mut catalog = ProfileCatalog::from_profiles(vec![gateway, desktop]);
    let imported = import_catalog(&export_catalog(&catalog).unwrap())
        .unwrap()
        .catalog;
    assert_eq!(
        imported.profiles[1].vnc.gateway_profile,
        Some(imported.profiles[0].id)
    );
    catalog.profiles[1].vnc.gateway_profile = Some(ProfileId::new());
    assert!(import_catalog(&export_catalog(&catalog).unwrap()).is_err());
    catalog.profiles[1].id = catalog.profiles[0].id;
    assert!(import_catalog(&export_catalog(&catalog).unwrap()).is_err());
}

#[test]
fn imported_catalog_persists_and_reloads_in_a_unique_fixture_directory() {
    let scratch = std::env::temp_dir().join(format!("adit-catalog-fixture-{}", ProfileId::new()));
    std::fs::create_dir(&scratch).unwrap();
    let input = scratch.join("input.reg");
    std::fs::write(&input, PUTTY).unwrap();
    let report = parse_putty_reg(&read_import_file(&input).unwrap()).unwrap();
    let store = ProfileStore::new(scratch.join("profiles.json"));
    store.save_catalog(&report.catalog).unwrap();
    let loaded = store.load_catalog().unwrap();
    assert_eq!(loaded.profiles.len(), 3);
    assert_eq!(loaded.profiles[0].port, 2222);
    assert_eq!(
        loaded.profiles[0].identity_file,
        r"C:\fixture\keys\id_ed25519"
    );
    for path in [input, scratch.join("profiles.json")] {
        std::fs::remove_file(path).unwrap();
    }
    std::fs::remove_dir(scratch).unwrap();
}

#[test]
fn ssh_proxyjump_aliases_ipv6_ports_users_and_identity_are_resolved() {
    let report = parse_ssh_config_report(
        r#"
Host target
    HostName target.test
    User deploy
    IdentityFile "C:/fixture/keys/shared key"
    ProxyJump operator@jump:2022,[2001:db8::2]:2223
Host jump
    HostName bastion.test
    User ignored_by_explicit_user
    Port 2200
    IdentityFile "C:/fixture/keys/shared key"
Host *
    Port 22
"#,
    )
    .unwrap();
    let target = &report.catalog.profiles[0];
    assert_eq!(target.host, "target.test");
    assert_eq!(target.jumps[0].host, "bastion.test");
    assert_eq!(target.jumps[0].username, "operator");
    assert_eq!(target.jumps[0].port, 2022);
    assert_eq!(target.jumps[1].host, "2001:db8::2");
    assert_eq!(target.jumps[1].port, 2223);
    let loaded = import_catalog(&export_catalog(&report.catalog).unwrap())
        .unwrap()
        .catalog;
    assert_eq!(loaded.profiles[0].jumps, target.jumps);
}

#[test]
fn unsupported_or_invalid_ssh_routes_never_become_direct_connections() {
    for directive in [
        "ProxyJump jump:0",
        "ProxyJump jump:70000",
        "ProxyJump jump,,other",
        "ProxyJump @jump",
        "ProxyJump -oBad",
        "ProxyJump target",
        "ProxyCommand cmd.exe /c should-never-run",
        "Port wrong",
        "Port 0",
        "IdentityFile \"unterminated",
        "Include other",
        "Match exec \"never-run\"",
        "LocalCommand should-never-run",
    ] {
        assert!(
            parse_ssh_config_report(&format!("Host target\n {directive}\n")).is_err(),
            "{directive}"
        );
    }
    assert!(parse_ssh_config_report(
        "Host target\n ProxyJump jump\nHost jump\n IdentityFile different-key\n"
    )
    .is_err());
    assert!(parse_ssh_config_report(
        "Host target\n ProxyJump jump\nHost jump\n ProxyJump target\n"
    )
    .is_err());
}
