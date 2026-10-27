use adit_domain::{ConnectionProfile, ProxyCommand};
use adit_storage::{parse_ssh_config, ProfileStore};

#[test]
fn portable_catalogs_refuse_configured_process_routes_without_disclosing_arguments() {
    use adit_storage::{export_catalog, import_catalog, ProfileCatalog};
    for enabled in [false, true] {
        let mut profile = ConnectionProfile::new("synthetic", "example.invalid", 22, "test");
        profile.proxy_command = ProxyCommand {
            enabled,
            program: "synthetic-private-program".into(),
            args: vec!["synthetic-private-argument".into()],
        };
        let catalog = ProfileCatalog::from_profiles(vec![profile.clone()]);
        let error = export_catalog(&catalog).unwrap_err();
        assert!(error.contains("proxy_command"));
        assert!(!error.contains("synthetic-private"));
        let incoming = serde_json::json!({
            "version": 0,
            "profiles": [profile],
            "groups": [],
            "group_icons": {},
        });
        let error = import_catalog(&incoming.to_string()).unwrap_err();
        assert!(error.contains("proxy_command"));
        assert!(!error.contains("synthetic-private"));
    }
}

#[test]
fn old_and_ssh_imported_profiles_never_enable_external_processes() {
    let profile = ConnectionProfile::new("synthetic", "example.invalid", 22, "test");
    let value = serde_json::to_value(&profile).unwrap();
    assert!(
        value.get("proxy_command").is_none(),
        "unused routes must not break export allowlists"
    );
    let loaded: ConnectionProfile = serde_json::from_value(value).unwrap();
    assert!(!loaded.proxy_command.enabled);

    let command_config =
        "Host synthetic\n HostName example.invalid\n ProxyCommand cmd /c unwanted\n";
    let error = adit_storage::parse_ssh_config_report(command_config).unwrap_err();
    assert!(!error.contains("unwanted"));
    assert!(
        parse_ssh_config(command_config).is_empty(),
        "an unsupported required proxy must never silently turn into a direct route"
    );
    let hosts = parse_ssh_config("Host synthetic\n HostName example.invalid\n");
    assert_eq!(hosts.len(), 1);
    // Importers create profiles through this constructor, rather than importing
    // OpenSSH's shell command string as executable configuration.
    let imported =
        ConnectionProfile::new(&hosts[0].alias, &hosts[0].hostname, hosts[0].port, "test");
    assert!(imported.proxy_command.is_empty());
}

#[test]
fn explicitly_configured_route_round_trips_in_local_profile_store_and_debug_redacts_it() {
    let dir = std::env::temp_dir().join(format!(
        "adit-proxy-profile-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let mut profile = ConnectionProfile::new("synthetic", "example.invalid", 22, "test");
    profile.proxy_command = ProxyCommand {
        enabled: true,
        program: if cfg!(windows) {
            "C:\\test proxy\\proxy.exe"
        } else {
            "/test proxy/proxy"
        }
        .into(),
        args: vec![
            "synthetic-sensitive-argument".into(),
            "%h".into(),
            "%p".into(),
        ],
    };
    let store = ProfileStore::new(dir.join("profiles.json"));
    store.save_profiles(&[profile.clone()]).unwrap();
    let loaded = store.load_profiles().unwrap();
    assert_eq!(loaded[0].proxy_command, profile.proxy_command);
    assert!(!format!("{:?}", loaded[0]).contains("synthetic-sensitive-argument"));
    // Non-default command fields remain identifiable, so portable exporters
    // with a field allowlist refuse them rather than leaking argv or silently
    // turning a required proxy route into a direct connection.
    assert!(serde_json::to_value(&loaded[0])
        .unwrap()
        .get("proxy_command")
        .is_some());
    std::fs::remove_dir_all(dir).unwrap();
}
