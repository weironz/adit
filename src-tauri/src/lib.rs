use serde::{Deserialize, Serialize};
use ssh2::{Channel, Session};
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::{
        mpsc::{self, Receiver, Sender},
        Mutex,
    },
    thread,
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

const DEFAULT_COLS: u32 = 96;
const DEFAULT_ROWS: u32 = 28;

#[derive(Default)]
struct SessionRegistry {
    sessions: Mutex<HashMap<String, SessionHandle>>,
}

struct SessionHandle {
    tx: Sender<SessionCommand>,
}

enum SessionCommand {
    Input(String),
    Resize { cols: u32, rows: u32 },
    Disconnect,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SshConnectRequest {
    label: Option<String>,
    host: String,
    port: u16,
    username: String,
    password: String,
    terminal_cols: Option<u32>,
    terminal_rows: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SshConnectResponse {
    session_id: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalDataEvent {
    session_id: String,
    data: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalStatusEvent {
    session_id: String,
    state: String,
    message: String,
}

#[tauri::command]
fn ssh_connect(
    app: AppHandle,
    registry: State<'_, SessionRegistry>,
    request: SshConnectRequest,
) -> Result<SshConnectResponse, String> {
    validate_connect_request(&request)?;

    let session_id = Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::channel();

    registry
        .sessions
        .lock()
        .map_err(|_| "Session registry is unavailable".to_string())?
        .insert(session_id.clone(), SessionHandle { tx });

    let worker_app = app.clone();
    let worker_session_id = session_id.clone();
    thread::spawn(move || {
        run_ssh_session(worker_app, worker_session_id, request, rx);
    });

    Ok(SshConnectResponse { session_id })
}

#[tauri::command]
fn ssh_write(
    registry: State<'_, SessionRegistry>,
    session_id: String,
    data: String,
) -> Result<(), String> {
    send_session_command(&registry, &session_id, SessionCommand::Input(data))
}

#[tauri::command]
fn ssh_resize(
    registry: State<'_, SessionRegistry>,
    session_id: String,
    cols: u32,
    rows: u32,
) -> Result<(), String> {
    if cols == 0 || rows == 0 {
        return Ok(());
    }

    send_session_command(
        &registry,
        &session_id,
        SessionCommand::Resize { cols, rows },
    )
}

#[tauri::command]
fn ssh_disconnect(registry: State<'_, SessionRegistry>, session_id: String) -> Result<(), String> {
    let handle = registry
        .sessions
        .lock()
        .map_err(|_| "Session registry is unavailable".to_string())?
        .remove(&session_id);

    if let Some(handle) = handle {
        let _ = handle.tx.send(SessionCommand::Disconnect);
    }

    Ok(())
}

fn validate_connect_request(request: &SshConnectRequest) -> Result<(), String> {
    if request.host.trim().is_empty() {
        return Err("Host is required".to_string());
    }

    if request.username.trim().is_empty() {
        return Err("Username is required".to_string());
    }

    if request.password.is_empty() {
        return Err("Password is required for this MVP".to_string());
    }

    if request.port == 0 {
        return Err("Port must be between 1 and 65535".to_string());
    }

    Ok(())
}

fn send_session_command(
    registry: &State<'_, SessionRegistry>,
    session_id: &str,
    command: SessionCommand,
) -> Result<(), String> {
    let tx = registry
        .sessions
        .lock()
        .map_err(|_| "Session registry is unavailable".to_string())?
        .get(session_id)
        .map(|handle| handle.tx.clone())
        .ok_or_else(|| "Session is not active".to_string())?;

    tx.send(command)
        .map_err(|_| "Session is no longer accepting input".to_string())
}

fn run_ssh_session(
    app: AppHandle,
    session_id: String,
    request: SshConnectRequest,
    rx: Receiver<SessionCommand>,
) {
    let label = request
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(request.host.as_str())
        .to_string();

    emit_status(
        &app,
        &session_id,
        "connecting",
        format!("Connecting to {label}"),
    );

    let result = connect_and_stream(&app, &session_id, request, rx);
    if let Err(error) = result {
        emit_status(&app, &session_id, "error", error);
    }

    remove_session(&app, &session_id);
    emit_status(&app, &session_id, "disconnected", "Disconnected");
}

fn connect_and_stream(
    app: &AppHandle,
    session_id: &str,
    request: SshConnectRequest,
    rx: Receiver<SessionCommand>,
) -> Result<(), String> {
    let address = resolve_address(&request.host, request.port)?;
    let tcp = TcpStream::connect_timeout(&address, Duration::from_secs(12))
        .map_err(|error| format!("Unable to connect to {}: {}", address, error))?;
    tcp.set_read_timeout(Some(Duration::from_millis(80)))
        .map_err(|error| format!("Unable to configure socket read timeout: {error}"))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("Unable to configure socket write timeout: {error}"))?;

    let mut session =
        Session::new().map_err(|error| format!("Unable to create SSH session: {error}"))?;
    session.set_tcp_stream(tcp);
    session
        .handshake()
        .map_err(|error| format!("SSH handshake failed: {error}"))?;
    session
        .userauth_password(&request.username, &request.password)
        .map_err(|error| format!("SSH authentication failed: {error}"))?;

    if !session.authenticated() {
        return Err("SSH authentication failed".to_string());
    }

    let mut channel = session
        .channel_session()
        .map_err(|error| format!("Unable to open SSH channel: {error}"))?;

    let cols = request.terminal_cols.unwrap_or(DEFAULT_COLS).max(1);
    let rows = request.terminal_rows.unwrap_or(DEFAULT_ROWS).max(1);
    channel
        .request_pty("xterm-256color", None, Some((cols, rows, 0, 0)))
        .map_err(|error| format!("Unable to request remote PTY: {error}"))?;
    channel
        .shell()
        .map_err(|error| format!("Unable to start remote shell: {error}"))?;

    session.set_blocking(false);
    emit_status(app, session_id, "connected", "Connected");

    pump_terminal(app, session_id, &mut channel, rx)
}

fn resolve_address(host: &str, port: u16) -> Result<std::net::SocketAddr, String> {
    (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("Unable to resolve host: {error}"))?
        .next()
        .ok_or_else(|| "Host did not resolve to an address".to_string())
}

fn pump_terminal(
    app: &AppHandle,
    session_id: &str,
    channel: &mut Channel,
    rx: Receiver<SessionCommand>,
) -> Result<(), String> {
    let mut buffer = [0_u8; 8192];

    loop {
        while let Ok(command) = rx.try_recv() {
            match command {
                SessionCommand::Input(data) => write_channel(channel, data.as_bytes())
                    .map_err(|error| format!("Unable to write to remote shell: {error}"))?,
                SessionCommand::Resize { cols, rows } => channel
                    .request_pty_size(cols, rows, None, None)
                    .map_err(|error| format!("Unable to resize remote PTY: {error}"))?,
                SessionCommand::Disconnect => {
                    let _ = channel.close();
                    return Ok(());
                }
            }
        }

        match channel.read(&mut buffer) {
            Ok(0) => {
                if channel.eof() {
                    return Ok(());
                }
            }
            Ok(count) => emit_data(app, session_id, &buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("Unable to read from remote shell: {error}")),
        }

        if channel.eof() {
            return Ok(());
        }

        thread::sleep(Duration::from_millis(12));
    }
}

fn write_channel(channel: &mut Channel, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        match channel.write(data) {
            Ok(0) => {
                thread::sleep(Duration::from_millis(8));
            }
            Ok(count) => data = &data[count..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(8));
            }
            Err(error) => return Err(error),
        }
    }

    match channel.flush() {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(error) => Err(error),
    }
}

fn emit_data(app: &AppHandle, session_id: &str, data: &[u8]) {
    let payload = TerminalDataEvent {
        session_id: session_id.to_string(),
        data: String::from_utf8_lossy(data).to_string(),
    };
    let _ = app.emit("terminal-data", payload);
}

fn emit_status(
    app: &AppHandle,
    session_id: &str,
    state: impl Into<String>,
    message: impl Into<String>,
) {
    let payload = TerminalStatusEvent {
        session_id: session_id.to_string(),
        state: state.into(),
        message: message.into(),
    };
    let _ = app.emit("terminal-status", payload);
}

fn remove_session(app: &AppHandle, session_id: &str) {
    if let Some(registry) = app.try_state::<SessionRegistry>() {
        if let Ok(mut sessions) = registry.sessions.lock() {
            sessions.remove(session_id);
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(SessionRegistry::default())
        .invoke_handler(tauri::generate_handler![
            ssh_connect,
            ssh_write,
            ssh_resize,
            ssh_disconnect
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
