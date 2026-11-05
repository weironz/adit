//! Exercises the production iced renderer/event-loop and registration Task,
//! rather than registering an unrelated HWND on a fresh STA.
use super::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

fn claim_environment() -> Result<(), Box<dyn std::error::Error>> {
    let temp = std::fs::canonicalize(std::env::temp_dir())?;
    let config = std::path::PathBuf::from(
        std::env::var_os("ADIT_CONFIG_DIR")
            .ok_or("ADIT_CONFIG_DIR must name a fresh empty temp directory")?,
    );
    if !config.is_absolute() {
        return Err("ADIT_CONFIG_DIR must be absolute".into());
    }
    let config = std::fs::canonicalize(config)?;
    if config == temp
        || !config.starts_with(&temp)
        || std::fs::read_dir(&config)?.next().transpose()?.is_some()
    {
        return Err("ADIT_CONFIG_DIR must be fresh and empty beneath OS temp".into());
    }
    for key in ["USERPROFILE", "HOME", "APPDATA", "LOCALAPPDATA"] {
        let path = std::fs::canonicalize(
            std::env::var_os(key).ok_or("missing isolated acceptance environment directory")?,
        )?;
        if path == temp || !path.starts_with(&temp) || path == config {
            return Err(format!("{key} must be an isolated directory beneath OS temp").into());
        }
    }
    // Claim before constructing AditApp or any store; concurrent harnesses may
    // not race the empty-directory check and share a credential/config root.
    std::fs::create_dir(config.join(".native-file-drop-acceptance"))?;
    Ok(())
}

struct Harness {
    desktop: DesktopApplication,
    registered: Arc<AtomicBool>,
}
impl Harness {
    fn view(&self, id: window::Id) -> Element<'_, NativeMessage> {
        self.desktop.view(id)
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    claim_environment()?;
    let (finished, completion) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if matches!(
            completion.recv_timeout(Duration::from_secs(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            eprintln!("native-file-drop production iced registration timed out after 20 seconds");
            std::process::exit(2);
        }
    });
    let registered = Arc::new(AtomicBool::new(false));
    let result = registered.clone();
    let run = iced::daemon(move || {
        let (primary, opened) = window::open(drop_window_settings(window::Settings {
            visible: false, exit_on_close_request: false, ..Default::default()
        }));
        let mut app = AditApp { native_main_window: Some(primary), native_window_id: Some(primary),
            auto_check_updates: false, keyring_migrated: true, ..AditApp::default() };
        app.sync.auto_sync = false;
        (Harness { desktop: DesktopApplication { app, primary, focused: None,
            windows: HashMap::new(), clipboard_leases: HashMap::new(), modal_owner: None },
            registered: registered.clone() }, opened.map(NativeMessage::Opened))
    }, |harness: &mut Harness, message: NativeMessage| {
        if let NativeMessage::DropRegistered(id, ok) = &message {
            let ok = *ok;
            let id = *id;
            let task = harness.desktop.update(message);
            harness.registered.store(ok, Ordering::Release);
            eprintln!("native-file-drop production iced registration: window={id:?}, registered={ok}, refusal_visible={}",
                harness.desktop.app.last_error.is_some());
            return Task::batch([task, iced::exit()]);
        }
        harness.desktop.update(message)
    }, Harness::view)
    .subscription(|harness: &Harness| harness.desktop.subscription())
    .theme(|harness: &Harness, _| app_theme(&harness.desktop.app))
    .run();
    let _ = finished.send(());
    watchdog.join().expect("native drop watchdog panicked");
    run?;
    assert!(
        result.load(Ordering::Acquire),
        "production iced OLE registration failed"
    );
    Ok(())
}
