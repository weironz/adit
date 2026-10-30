//! Public synthetic IPC peer for the UI's post-frame Error/Closed acceptance.
//! This performs no network connection and is not a native ClearCodec capture.
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

use adit_rdp_proto::{
    read_client_hello, read_msg, write_helper_hello, write_msg, ClientMsg, HostMsg, InputEvent,
    IPC_PROTOCOL_VERSION,
};

fn main() -> io::Result<()> {
    let mode = std::env::var("ADIT_RDP_FAILURE_MODE")
        .map_err(|_| io::Error::other("requires a fixture mode"))?;
    let live = match mode.as_str() {
        "live_closed" | "live_eof" => true,
        "fatal_closed" | "fatal_eof" => false,
        _ => return Err(io::Error::other("requires a live/fatal closed/eof mode")),
    };
    let report = PathBuf::from(
        std::env::var_os("ADIT_RDP_FAILURE_REPORT")
            .ok_or_else(|| io::Error::other("requires an owned public fixture report path"))?,
    );
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    if read_client_hello(&mut input)? != IPC_PROTOCOL_VERSION {
        return Err(io::Error::other("incompatible IPC version"));
    }
    write_helper_hello(&mut output, IPC_PROTOCOL_VERSION)?;
    match read_msg::<_, ClientMsg>(&mut input)? {
        Some(ClientMsg::Connect(request))
            if request.host == "adit.invalid"
                && request.username.is_empty()
                && request.password.is_empty() => {}
        _ => return Err(io::Error::other("requires the public synthetic UI request")),
    }
    write_msg(
        &mut output,
        &HostMsg::Connected {
            width: 2,
            height: 2,
        },
    )?;
    write_msg(
        &mut output,
        &HostMsg::Tile {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
            rgba: [10, 20, 30, 255].repeat(4),
        },
    )?;
    // Wait until the test has observed a production UI tile. Timing a sleep
    // here would let a fast error skip the established-frame regression.
    while let Some(message) = read_msg::<_, ClientMsg>(&mut input)? {
        match message {
            ClientMsg::Input(InputEvent::Key {
                scancode: 0x7f,
                pressed: true,
                ..
            }) => {
                let evidence = if live {
                    // First prove every tested input reaches this same live
                    // writer while connected; otherwise zero after Error could
                    // be caused by an unrelated focus or routing gate.
                    let before = read_to_barrier(&mut input)?;
                    let preflight = report.with_extension("preflight");
                    write_report(&preflight, &before.join(","))?;
                    let after = read_to_barrier(&mut input)?;
                    format!(
                        "mode={mode}\ninputs_after_error={}\nbarrier=true\npipe_held_open=true\n",
                        after.len()
                    )
                } else {
                    write_msg(&mut output, &HostMsg::Error(
                        "ClearCodec decode failed: synthetic IPC acceptance fault (no native RDP decode)".into(),
                    ))?;
                    let mut inputs_after_error = 0;
                    loop {
                        match read_msg::<_, ClientMsg>(&mut input)? {
                            Some(ClientMsg::Input(_)) => inputs_after_error += 1,
                            Some(ClientMsg::Close) => break,
                            None => return Err(io::Error::other("missing fatal-error Close")),
                            _ => {}
                        }
                    }
                    // A Close is terminal for the writer, not a FIFO barrier
                    // for commands the UI might try to send afterwards.
                    if read_msg::<_, ClientMsg>(&mut input)?.is_some() {
                        return Err(io::Error::other("message after fatal-error Close"));
                    }
                    format!(
                        "mode={mode}\ninputs_after_error={inputs_after_error}\nclose_received=true\nstdin_eof=true\nstdout_held_open=true\n"
                    )
                };
                write_report(&report, &evidence)?;
                let deadline = Instant::now() + Duration::from_secs(8);
                while !report.with_extension("release").is_file() {
                    if Instant::now() >= deadline {
                        return Err(io::Error::other("post-error hold release deadline"));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                let sends_closed = mode.ends_with("_closed");
                if sends_closed {
                    write_msg(&mut output, &HostMsg::Closed)?;
                }
                // EOF mode sends no Closed; the actual pipe reader must take
                // its Ok(None) path. Keep separate evidence for this branch.
                write_report(
                    &report.with_extension("finished"),
                    &format!("mode={mode}\nclosed_message_sent={sends_closed}\n"),
                )?;
                return Ok(());
            }
            ClientMsg::Close => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}

fn write_report(path: &std::path::Path, evidence: &str) -> io::Result<()> {
    let temp = path.with_extension("pending");
    std::fs::write(&temp, evidence)?;
    std::fs::rename(temp, path)
}

fn read_to_barrier(input: &mut impl io::Read) -> io::Result<Vec<&'static str>> {
    let mut inputs = Vec::new();
    loop {
        match read_msg::<_, ClientMsg>(input)? {
            Some(ClientMsg::Input(InputEvent::Key {
                scancode: 0x7e,
                extended: false,
                pressed: true,
            })) => return Ok(inputs),
            Some(ClientMsg::Input(event)) => inputs.push(match event {
                InputEvent::Key {
                    scancode: 0x1e,
                    extended: false,
                    pressed: true,
                } => "key_down",
                InputEvent::Key {
                    scancode: 0x1e,
                    extended: false,
                    pressed: false,
                } => "key_up",
                InputEvent::MouseMove { x: 1, y: 1 } => "move",
                InputEvent::MouseButton {
                    button: adit_rdp_proto::MouseButton::Left,
                    pressed: true,
                } => "button_down",
                InputEvent::MouseButton {
                    button: adit_rdp_proto::MouseButton::Left,
                    pressed: false,
                } => "button_up",
                InputEvent::Wheel {
                    vertical: true,
                    delta: 120,
                } => "wheel",
                InputEvent::Unicode {
                    ch: 'a',
                    pressed: true,
                } => "unicode_down",
                InputEvent::Unicode {
                    ch: 'a',
                    pressed: false,
                } => "unicode_up",
                InputEvent::Resize {
                    width: 4,
                    height: 4,
                } => "resize",
                _ => "other",
            }),
            Some(ClientMsg::ClipboardFiles { files, .. }) if files.is_empty() => return Ok(inputs),
            Some(ClientMsg::Close) | None => {
                return Err(io::Error::other("missing live-writer FIFO barrier"));
            }
            _ => {}
        }
    }
}
