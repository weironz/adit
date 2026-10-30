//! Manual RDP fixture driver. Password and optional input commands arrive on stdin.
//! argv: helper-path host username [timeout-seconds]. Never pass a password in argv/env.
//! Commands: WAIT milliseconds, KEY hex-scancode extended(0|1) pressed(0|1), TEXT text, ASCII US-layout-text,
//! CLICK x y, MOVE x y, CLIP text, SNAP absolute-png-path, ACK token, CLOSE.
//! Optional --fixture-report enables clipboard
//! transport and prints only reports starting with ADIT_VNC_TEST:, never arbitrary text.
//! ACK confirms driver command processing, not remote execution. Verify the
//! remote effect separately. Remote clipboard updates can replace CLIP offers;
//! TEXT is useful for fixture control, but never proves native keyboard/IME input.
use adit_rdp_proto::{read_msg, write_msg, ClientMsg, ConnectRequest, HostMsg, InputEvent};
use std::{
    io::{self, BufRead},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 4 {
        return Err(
            "usage: rdp_probe helper host username [timeout-seconds]; password on stdin".into(),
        );
    }
    let timeout: u64 = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(120);
    let mut input = io::stdin().lock();
    let mut password = String::new();
    if input.read_line(&mut password)? == 0 {
        return Err("missing stdin password".into());
    }
    let password = password.trim_end_matches(['\r', '\n']).to_owned();
    let mut child = Command::new(&args[1])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if args.iter().any(|arg| arg == "--fixture-report") {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .env("RUST_LOG", "warn")
        .spawn()?;
    let mut wire = child.stdin.take().ok_or("helper stdin missing")?;
    write_msg(
        &mut wire,
        &ClientMsg::Connect(ConnectRequest {
            host: args[2].clone(),
            port: 3389,
            username: args[3].clone(),
            password,
            domain: None,
            width: 1280,
            height: 800,
            enable_clipboard: args.iter().any(|s| s == "--fixture-report"),
            enable_audio: false,
            quality: Default::default(),
        }),
    )?;
    let mut output = child.stdout.take().ok_or("helper stdout missing")?;
    let (events, received) = mpsc::sync_channel(8);
    std::thread::spawn(move || loop {
        match read_msg::<_, HostMsg>(&mut output) {
            Ok(Some(message)) => {
                if events.send(message).is_err() {
                    break;
                }
            }
            _ => {
                let _ = events.send(HostMsg::Closed);
                break;
            }
        }
    });
    drop(input);
    let (commands, requested) = mpsc::channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if commands.send(line).is_err() {
                break;
            }
        }
    });
    let start = Instant::now();
    let mut ready = false;
    let mut input_epoch = 0u64;
    let mut size = (0u16, 0u16);
    let mut pixels = Vec::new();
    let mut tiles = 0u64;
    let mut pause_until = Instant::now();
    let mut typed = std::collections::VecDeque::new();
    let mut next_character = Instant::now();
    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        while start.elapsed() < Duration::from_secs(timeout) {
            match received.recv_timeout(Duration::from_millis(20)) {
                Ok(HostMsg::Connected { width, height })
                | Ok(HostMsg::Resized { width, height }) => {
                    let bytes = usize::from(width)
                        .checked_mul(usize::from(height))
                        .and_then(|n| n.checked_mul(4))
                        .ok_or("invalid dimensions")?;
                    if bytes > 256 * 1024 * 1024 {
                        return Err("frame exceeds limit".into());
                    }
                    size = (width, height);
                    pixels = vec![0; bytes];
                    ready = true;
                    println!(
                        "connected={}x{} elapsed_ms={}",
                        width,
                        height,
                        start.elapsed().as_millis()
                    );
                }
                Ok(HostMsg::Tile {
                    x,
                    y,
                    width,
                    height,
                    rgba,
                })
                | Ok(HostMsg::MaskedTile {
                    x,
                    y,
                    width,
                    height,
                    rgba,
                    painted: _,
                }) => {
                    if usize::from(width) * usize::from(height) * 4 != rgba.len()
                        || u32::from(x) + u32::from(width) > u32::from(size.0)
                        || u32::from(y) + u32::from(height) > u32::from(size.1)
                    {
                        return Err("invalid tile".into());
                    }
                    for row in 0..usize::from(height) {
                        let src = row * usize::from(width) * 4;
                        let dst =
                            ((usize::from(y) + row) * usize::from(size.0) + usize::from(x)) * 4;
                        pixels[dst..dst + usize::from(width) * 4]
                            .copy_from_slice(&rgba[src..src + usize::from(width) * 4]);
                    }
                    tiles += 1;
                    if tiles == 1 {
                        println!("first_tile_ms={}", start.elapsed().as_millis());
                    }
                }
                Ok(HostMsg::EpochClipboardText { epoch, text })
                    if epoch == input_epoch
                        && text.starts_with("ADIT_VNC_TEST:")
                        && args.iter().any(|s| s == "--fixture-report") =>
                {
                    println!("{text}")
                }
                Ok(HostMsg::DisplayControlReset { epoch }) => {
                    if input_epoch.checked_add(1) != Some(epoch) {
                        return Err("invalid redirect epoch".into());
                    }
                    input_epoch = epoch;
                }
                Ok(HostMsg::Error(error)) => return Err(error.into()),
                Ok(HostMsg::Closed) => return Err("helper closed".into()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("helper pipe closed".into())
                }
                _ => {}
            }
            if !ready || Instant::now() < pause_until {
                continue;
            }
            // Interleave text with paint draining; a synchronous large stdin write
            // can otherwise stall the helper behind its own full stdout pipe.
            if !typed.is_empty() {
                if Instant::now() >= next_character {
                    let (ch, ascii) = typed.pop_front().unwrap();
                    if ascii {
                        let (code, shifted) = ascii_scancode(ch)
                            .ok_or("ASCII requires a US-layout printable character")?;
                        if shifted {
                            write_msg(
                                &mut wire,
                                &ClientMsg::input_at(
                                    input_epoch,
                                    InputEvent::Key {
                                        scancode: 0x2a,
                                        extended: false,
                                        pressed: true,
                                    },
                                ),
                            )?;
                        }
                        for pressed in [true, false] {
                            write_msg(
                                &mut wire,
                                &ClientMsg::input_at(
                                    input_epoch,
                                    InputEvent::Key {
                                        scancode: code,
                                        extended: false,
                                        pressed,
                                    },
                                ),
                            )?;
                        }
                        if shifted {
                            write_msg(
                                &mut wire,
                                &ClientMsg::input_at(
                                    input_epoch,
                                    InputEvent::Key {
                                        scancode: 0x2a,
                                        extended: false,
                                        pressed: false,
                                    },
                                ),
                            )?;
                        }
                    } else {
                        for pressed in [true, false] {
                            write_msg(
                                &mut wire,
                                &ClientMsg::input_at(
                                    input_epoch,
                                    InputEvent::Unicode { ch, pressed },
                                ),
                            )?;
                        }
                    }
                    next_character = Instant::now() + Duration::from_millis(10);
                }
                continue;
            }
            if let Ok(command) = requested.try_recv() {
                if command == "CLOSE" {
                    return Ok(());
                }
                if let Some(ms) = command.strip_prefix("WAIT ") {
                    pause_until = Instant::now() + Duration::from_millis(ms.parse()?);
                } else if let Some(text) = command.strip_prefix("TEXT ") {
                    typed.extend(text.chars().map(|ch| (ch, false)));
                } else if let Some(text) = command.strip_prefix("ASCII ") {
                    typed.extend(text.chars().map(|ch| (ch, true)));
                } else if let Some(text) = command.strip_prefix("CLIP ") {
                    if !args.iter().any(|arg| arg == "--fixture-report") {
                        return Err("CLIP requires --fixture-report".into());
                    }
                    write_msg(
                        &mut wire,
                        &ClientMsg::input_at(
                            input_epoch,
                            InputEvent::ClipboardText(text.to_owned()),
                        ),
                    )?;
                } else if let Some(key) = command.strip_prefix("KEY ") {
                    let parts: Vec<_> = key.split_whitespace().collect();
                    if parts.len() != 3 {
                        return Err(
                            "KEY requires hex-scancode, extended (0|1), pressed (0|1)".into()
                        );
                    }
                    write_msg(
                        &mut wire,
                        &ClientMsg::input_at(
                            input_epoch,
                            InputEvent::Key {
                                scancode: u8::from_str_radix(parts[0], 16)?,
                                extended: parse_key_flag(parts[1])?,
                                pressed: parse_key_flag(parts[2])?,
                            },
                        ),
                    )?;
                } else if let Some(token) = command.strip_prefix("ACK ") {
                    if token.is_empty()
                        || token.len() > 64
                        || !token
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    {
                        return Err("ACK requires a short alphanumeric token".into());
                    }
                    println!("command_ack={token}");
                } else if let Some(point) = command
                    .strip_prefix("CLICK ")
                    .or_else(|| command.strip_prefix("MOVE "))
                {
                    let parts: Vec<_> = point.split_whitespace().collect();
                    if parts.len() != 2 {
                        return Err("pointer command requires x y".into());
                    }
                    write_msg(
                        &mut wire,
                        &ClientMsg::input_at(
                            input_epoch,
                            InputEvent::MouseMove {
                                x: parts[0].parse()?,
                                y: parts[1].parse()?,
                            },
                        ),
                    )?;
                    for pressed in [true, false]
                        .into_iter()
                        .filter(|_| command.starts_with("CLICK "))
                    {
                        write_msg(
                            &mut wire,
                            &ClientMsg::input_at(
                                input_epoch,
                                InputEvent::MouseButton {
                                    button: adit_rdp_proto::MouseButton::Left,
                                    pressed,
                                },
                            ),
                        )?;
                    }
                } else if let Some(path) = command.strip_prefix("SNAP ") {
                    let path = std::path::Path::new(path);
                    if !path.is_absolute() {
                        return Err("snapshot requires absolute path".into());
                    }
                    image::save_buffer(
                        path,
                        &pixels,
                        u32::from(size.0),
                        u32::from(size.1),
                        image::ColorType::Rgba8,
                    )?;
                    println!(
                        "snapshot_saved tiles={tiles} covered_pixels={}",
                        pixels
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .filter(|p| p[3] != 0)
                            .count()
                    );
                } else {
                    return Err("unknown stdin command".into());
                }
            }
        }
        Err("probe deadline expired".into())
    })();
    let _ = write_msg(&mut wire, &ClientMsg::Close);
    drop(wire);
    drop(received);
    for _ in 0..50 {
        if let Some(status) = child.try_wait()? {
            eprintln!("helper_exit={status}");
            return result;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    if let Ok(status) = child.wait() {
        eprintln!("helper_exit={status}");
    }
    result
}

fn parse_key_flag(value: &str) -> Result<bool, &'static str> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        // Treating arbitrary text as false silently converted attempted key
        // presses into releases, invalidating native-input acceptance.
        _ => Err("KEY extended/pressed flags must be 0 or 1"),
    }
}

fn ascii_scancode(ch: char) -> Option<(u8, bool)> {
    if ch.is_ascii_alphabetic() {
        let letters = [
            0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31,
            0x18, 0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c,
        ];
        return Some((
            letters[(ch.to_ascii_lowercase() as u8 - b'a') as usize],
            ch.is_ascii_uppercase(),
        ));
    }
    let normal = r#"1234567890-=[]\;',./` "#;
    let shifted = r#"!@#$%^&*()_+{}|:"<>?~ "#;
    let codes = [
        0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x1a, 0x1b, 0x2b,
        0x27, 0x28, 0x33, 0x34, 0x35, 0x29, 0x39,
    ];
    normal
        .chars()
        .position(|c| c == ch)
        .map(|i| (codes[i], false))
        .or_else(|| {
            shifted
                .chars()
                .position(|c| c == ch)
                .map(|i| (codes[i], true))
        })
}

#[cfg(test)]
mod tests {
    use super::parse_key_flag;

    #[test]
    fn key_flags_accept_only_numeric_zero_and_one() {
        assert_eq!(parse_key_flag("0"), Ok(false));
        assert_eq!(parse_key_flag("1"), Ok(true));
    }

    #[test]
    fn key_flags_reject_boolean_words_and_other_invalid_values() {
        for value in ["true", "false", "", "2", "-1", "01"] {
            assert!(
                parse_key_flag(value).is_err(),
                "accepted invalid KEY flag {value:?}"
            );
        }
    }
}
