//! Public synthetic IPC peer for the UI's post-frame Error/Closed acceptance.
//! This performs no network connection and is not a native ClearCodec capture.
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

use adit_rdp_proto::{read_msg, write_msg, ClientMsg, HostMsg, InputEvent};

fn main() -> io::Result<()> {
    let mode = std::env::var("ADIT_RDP_FAILURE_MODE").unwrap_or_else(|_| "closed".into());
    if mode != "closed" && mode != "eof" {
        return Err(io::Error::other("requires closed or eof fixture mode"));
    }
    let report = PathBuf::from(
        std::env::var_os("ADIT_RDP_FAILURE_REPORT")
            .ok_or_else(|| io::Error::other("requires an owned public fixture report path"))?,
    );
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
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
                write_msg(&mut output, &HostMsg::Error(
                    "ClearCodec decode failed: synthetic IPC acceptance fault (no native RDP decode)".into(),
                ))?;
                // Read through the same FIFO barrier as the ordinary inputs.
                // The pipe stays open after Error, so a broken client cannot
                // pass simply because input was dropped by process shutdown.
                let mut inputs_after_error = 0;
                loop {
                    match read_msg::<_, ClientMsg>(&mut input)? {
                        Some(ClientMsg::Input(_)) => inputs_after_error += 1,
                        Some(ClientMsg::ClipboardFiles(files)) if files.is_empty() => break,
                        Some(ClientMsg::Close) | None => {
                            return Err(io::Error::other("missing post-error FIFO barrier"));
                        }
                        _ => {}
                    }
                }
                let evidence = format!(
                    "mode={mode}\ninputs_after_error={inputs_after_error}\nbarrier=true\npipe_held_open=true\n"
                );
                let temp = report.with_extension("pending");
                std::fs::write(&temp, evidence)?;
                std::fs::rename(temp, &report)?;
                let deadline = Instant::now() + Duration::from_secs(8);
                while !report.with_extension("release").is_file() {
                    if Instant::now() >= deadline {
                        return Err(io::Error::other("post-error hold release deadline"));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                if mode == "closed" {
                    write_msg(&mut output, &HostMsg::Closed)?;
                }
                // EOF mode sends no Closed; the actual pipe reader must take
                // its Ok(None) path. Keep separate evidence for this branch.
                std::fs::write(
                    report.with_extension("finished"),
                    format!("mode={mode}\nclosed_message_sent={}\n", mode == "closed"),
                )?;
                return Ok(());
            }
            ClientMsg::Close => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}
