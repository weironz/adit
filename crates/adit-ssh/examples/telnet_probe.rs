//! Drives Adit's production telnet client ([`adit_ssh::spawn_telnet`]) from a
//! script on stdin, so it can be pointed at a real telnet daemon or device.
//!
//! `cargo run -p adit-ssh --example telnet_probe -- HOST PORT [COLS ROWS]`
//!
//! Commands, one per line, payloads hex-encoded so CR/NUL/0xFF survive:
//! `SEND <hex>`, `WAIT <hex> <ms>` (until the accumulated output contains the
//! needle), `RESIZE <cols> <rows>`, `SLEEP <ms>`, `CLOSE`. Every event is
//! printed as one line: `OUT <hex>`, `STATUS ...`, `MATCH`, `TIMEOUT`, `CLOSED`.
//! Use `-` as a payload to read one secret line from stdin for the next SEND
//! without it ever appearing in argv, env or the output log.
use std::{
    env,
    io::{self, BufRead, Write},
    thread,
    time::{Duration, Instant},
};

use adit_ssh::{spawn_telnet, LiveShellCommand, LiveShellEvent, LiveShellHandle};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex"))
        .collect()
}

fn drain(handle: &LiveShellHandle, seen: &mut Vec<u8>, out: &mut impl Write) -> bool {
    let mut open = true;
    while let Some(event) = handle.try_recv() {
        match event {
            LiveShellEvent::Output(bytes) => {
                let _ = writeln!(out, "OUT {}", hex(&bytes));
                seen.extend_from_slice(&bytes);
            }
            LiveShellEvent::Status(text) => {
                let _ = writeln!(out, "STATUS {text}");
            }
            LiveShellEvent::Error(text) => {
                let _ = writeln!(out, "ERROR {text}");
            }
            LiveShellEvent::Closed | LiveShellEvent::ShellExited => {
                let _ = writeln!(out, "CLOSED");
                open = false;
            }
            _ => {}
        }
    }
    let _ = out.flush();
    open
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let host = args.get(1).cloned().unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(23);
    let cols: u16 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(100);
    let rows: u16 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(30);

    let handle = spawn_telnet(host, port, cols, rows).expect("spawn telnet");
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut seen = Vec::new();
    let mut lines = io::stdin().lock().lines();

    while let Some(Ok(line)) = lines.next() {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (Some("SEND"), Some("-"), suffix) => {
                let mut secret = lines
                    .next()
                    .and_then(Result::ok)
                    .unwrap_or_default()
                    .into_bytes();
                secret.extend(unhex(suffix.unwrap_or("")));
                let _ = handle.send(LiveShellCommand::Input(secret));
                let _ = writeln!(out, "SENT <secret>");
            }
            (Some("SEND"), Some(payload), _) => {
                let _ = handle.send(LiveShellCommand::Input(unhex(payload)));
                let _ = writeln!(out, "SENT {payload}");
            }
            (Some("WAIT"), Some(needle), ms) => {
                let needle = unhex(needle);
                let deadline = Instant::now()
                    + Duration::from_millis(ms.and_then(|m| m.parse().ok()).unwrap_or(5000));
                let mut matched = false;
                while Instant::now() < deadline {
                    let open = drain(&handle, &mut seen, &mut out);
                    if let Some(at) = seen
                        .windows(needle.len())
                        .position(|w| w == needle.as_slice())
                    {
                        seen.drain(..at + needle.len());
                        matched = true;
                        break;
                    }
                    if !open {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                let _ = writeln!(out, "{}", if matched { "MATCH" } else { "TIMEOUT" });
            }
            (Some("RESIZE"), Some(c), Some(r)) => {
                let _ = handle.send(LiveShellCommand::Resize {
                    cols: c.parse().unwrap_or(cols),
                    rows: r.parse().unwrap_or(rows),
                });
            }
            (Some("SLEEP"), Some(ms), _) => {
                thread::sleep(Duration::from_millis(ms.parse().unwrap_or(0)));
                drain(&handle, &mut seen, &mut out);
            }
            (Some("CLOSE"), _, _) => break,
            _ => {}
        }
        let _ = out.flush();
    }
    let _ = handle.send(LiveShellCommand::Disconnect);
    thread::sleep(Duration::from_millis(200));
    drain(&handle, &mut seen, &mut out);
}
