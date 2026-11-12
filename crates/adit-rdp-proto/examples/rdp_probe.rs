//! Manual RDP fixture driver. Password and optional input commands arrive on stdin.
//! argv: helper-path host username [timeout-seconds]. Never pass a password in argv/env.
//! Commands: WAIT milliseconds, KEY hex-scancode extended(0|1) pressed(0|1), TEXT text, ASCII US-layout-text,
//! CLICK x y, MOVE x y, CLIP text, SNAP absolute-png-path, ACK token, CLOSE.
//! Optional --fixture-report enables clipboard
//! transport and prints only reports starting with ADIT_VNC_TEST:, never arbitrary text.
//! ACK confirms driver command processing, not remote execution. Verify the
//! remote effect separately. Remote clipboard updates can replace CLIP offers;
//! TEXT is useful for fixture control, but never proves native keyboard/IME input.
use adit_rdp_proto::{
    read_ipc_hello, read_msg, write_ipc_hello, write_msg, ClientMsg, ConnectRequest, HostMsg,
    InputEvent, IPC_PROTOCOL_VERSION,
};
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
    // The fixture's port is not always 3389, and the wire request carries it
    // explicitly, so `host:port` is accepted rather than hiding the port in
    // an environment variable.
    let (host, port) = match args[2].split_once(':') {
        Some((host, port)) => (host.to_owned(), port.parse::<u16>()?),
        None => (args[2].clone(), 3389),
    };
    let timeout: u64 = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(120);
    // A local account needs its domain spelled out: NTLM folds the domain
    // into the response hash, and an empty domain does not match what the
    // server computes for the account it knows about.
    let domain = args
        .iter()
        .position(|arg| arg == "--domain")
        .and_then(|index| args.get(index + 1))
        .cloned();

    // The requested desktop size. A server that grants whatever is asked for
    // hides the "refused resize" case the app has to survive, so the probe has
    // to be able to ask for a size the server cannot give.
    let requested_size = args
        .iter()
        .position(|arg| arg == "--size")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.split_once('x'))
        .and_then(|(w, h)| Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?)))
        .unwrap_or((1280, 800));

    // The pointer bitmap is the server's own drawing, so the acceptance needs
    // it as pixels: dumping each distinct shape turns "the server sent a
    // pointer" into a template the GUI screenshot can be matched against.
    let pointer_dump = args
        .iter()
        .position(|arg| arg == "--pointer-dump")
        .and_then(|index| args.get(index + 1))
        .cloned();

    // The monitor layout to advertise at connect time, as `WIDTHxHEIGHT` pairs
    // separated by commas: `--monitors 1280x800,1280x800`. The first entry is
    // the primary and sits at the origin; the rest are laid out left to right.
    // A real server parses this block and rejects a malformed one, so driving
    // it is what shows the wire form is accepted rather than merely encoded.
    let monitor_sizes: Vec<(u16, u16)> = args
        .iter()
        .position(|arg| arg == "--monitors")
        .and_then(|index| args.get(index + 1))
        .map(|value| {
            value
                .split(',')
                .filter_map(|entry| {
                    let (w, h) = entry.trim().split_once('x')?;
                    let (width, height) = (w.parse::<u16>().ok()?, h.parse::<u16>().ok()?);
                    Some((width, height))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut monitors: Vec<adit_rdp_proto::Monitor> = monitor_sizes
        .iter()
        .enumerate()
        .map(|(index, (width, height))| adit_rdp_proto::Monitor {
            left: (index as i32) * i32::from(*width),
            top: 0,
            width: *width,
            height: *height,
            primary: index == 0,
            scale_percent: 100,
        })
        .collect();
    if !monitors.is_empty() {
        println!(
            "advertising_monitors={}",
            monitors
                .iter()
                .map(|m| format!("{}x{}+{}+{}", m.width, m.height, m.left, m.top))
                .collect::<Vec<_>>()
                .join(",")
        );
    }

    // A negative control for the monitor block: `--corrupt-monitors` gives the
    // second monitor a zero width, which the helper turns into an inverted
    // rectangle on the wire (`right` is `left + width - 1`, so a zero width
    // yields `right < left`). A real server must reject that — FreeRDP's
    // `gcc_read_client_monitor_data` fails the connection with
    // `rdpMonitor::rect ... invalid`.
    //
    // This exists because "the server logged no error" is not evidence that
    // the server read the block at all: it is equally consistent with the
    // block never arriving. A deliberately broken block that the server
    // *does* complain about turns the silence into proof, because the same
    // code path that stayed quiet on the well-formed block is the one that
    // speaks up on the malformed one.
    if args.iter().any(|arg| arg == "--corrupt-monitors") && monitors.len() >= 2 {
        monitors[1].width = 0;
        println!("corrupted_monitors=true");
    }

    // The desktop the server is asked for. With a layout, that is the
    // bounding box of the whole set, not the primary monitor's size.
    //
    // The bounding box is computed here rather than through
    // `adit_rdp_proto::monitor_bounds`, because that validator is the app's
    // gate, not the wire's: this probe exists to put bytes on the wire that
    // the app would refuse to send, so that a real server's parser can be
    // observed rejecting them.
    let mut desktop_size = if monitors.len() < 2 {
        requested_size
    } else {
        let left = monitors.iter().map(|m| m.left).min().unwrap_or_default();
        let top = monitors.iter().map(|m| m.top).min().unwrap_or_default();
        let right = monitors
            .iter()
            .map(|m| m.left + i32::from(m.width))
            .max()
            .unwrap_or_default();
        let bottom = monitors
            .iter()
            .map(|m| m.top + i32::from(m.height))
            .max()
            .unwrap_or_default();
        (
            (right - left) as u16,
            (bottom - top) as u16,
        )
    };

    // `--desktop-size WxH` decouples the requested desktop from the advertised
    // layout. That separation is what makes the experiment meaningful: if the
    // layout is honoured, the server's desktop follows the *layout* and not
    // the size that was asked for, so asking for a small desktop while
    // advertising a wide layout must still produce a wide desktop. Without
    // this switch the two always agree and the observation proves nothing.
    if let Some(index) = args.iter().position(|arg| arg == "--desktop-size") {
        let value = args
            .get(index + 1)
            .ok_or("--desktop-size requires WIDTHxHEIGHT")?;
        let (w, h) = value.split_once('x').ok_or("--desktop-size requires WIDTHxHEIGHT")?;
        desktop_size = (w.parse()?, h.parse()?);
        println!("desktop_size_override={}x{}", desktop_size.0, desktop_size.1);
    }

    println!(
        "requested_size={}x{}",
        requested_size.0, requested_size.1
    );

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
        // An inherited RUST_LOG wins: the helper's own log is the only
        // window into a failed connection, so the driver must not silence it.
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_owned()),
        )

        .spawn()?;
    let mut wire = child.stdin.take().ok_or("helper stdin missing")?;
    // The helper speaks a versioned IPC envelope: it reads a fixed hello before
    // it will look at a Connect request. Without it the helper rejects the
    // stream as a version mismatch and exits before it opens a socket.
    write_ipc_hello(&mut wire, IPC_PROTOCOL_VERSION)?;
    write_msg(
        &mut wire,
        &ClientMsg::Connect(ConnectRequest {
            host,
            port,
            username: args[3].clone(),
            password,
            domain: domain.clone(),
            // A multi-screen session must ask for a desktop big enough to
            // hold the whole layout: the server sizes its virtual desktop
            // from what we advertise, and a smaller request would clip the
            // outer monitors. This mirrors `rdp_desktop_size` in adit-session.
            width: desktop_size.0,
            height: desktop_size.1,
            enable_clipboard: args.iter().any(|s| s == "--fixture-report"),
            // Some servers (the FreeRDP shadow server among them) open an
            // AUDIN dynamic channel and mis-frame the stream when the client
            // refuses it, so the driver can be told to accept it.
            enable_audio: std::env::var_os("ADIT_PROBE_AUDIO").is_some(),
            quality: Default::default(),
            monitors,
        }),
    )?;
    let mut output = child.stdout.take().ok_or("helper stdout missing")?;
    let (events, received) = mpsc::sync_channel(8);
    std::thread::spawn(move || {
        // The helper answers with the same versioned envelope. An unversioned
        // peer would decode those 20 bytes as a HostMsg and see a stream
        // error instead of the session.
        if read_ipc_hello(&mut output).ok() != Some(IPC_PROTOCOL_VERSION) {
            let _ = events.send(HostMsg::Closed);
            return;
        }
        loop {
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
    // Each layout request carries its own ID so the server's answer can be
    // matched to the request that caused it.
    let mut layout_request_id = 0u64;
    let mut size = (0u16, 0u16);
    let mut pixels = Vec::new();
    let mut tiles = 0u64;
    // The server pointer is its own wire channel: a shape change arrives
    // independently of the framebuffer, so the acceptance has to count the
    // pointer messages rather than infer a shape from the pixels.
    let mut pointer_events = 0u64;
    let mut pointer_shapes: Vec<(u16, u16, u16, u16)> = Vec::new();
    // The server sends shape changes without a position, so the position that
    // caused each one is only recoverable from the move that preceded it.
    let mut last_move: Option<(u16, u16)> = None;
    let mut dumped_shapes: Vec<(u16, u16, u16, u16)> = Vec::new();
    let mut pause_until = Instant::now();
    let mut typed = std::collections::VecDeque::new();
    let mut next_character = Instant::now();
    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        while start.elapsed() < Duration::from_secs(timeout) {
            match received.recv_timeout(Duration::from_millis(20)) {
                Ok(HostMsg::GraphicsReset { epoch, .. }) if epoch != input_epoch => {
                    return Err("invalid graphics reset epoch".into());
                }
                Ok(HostMsg::Connected { width, height })
                | Ok(HostMsg::Resized { width, height })
                | Ok(HostMsg::GraphicsReset { width, height, .. }) => {
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
                Ok(HostMsg::MonitorLayoutObserved { monitors, .. }) => {
                    // The server's own view of the monitor topology. This is the
                    // only direct evidence of what the server actually decided
                    // the desktop is, so it is printed rather than summarised:
                    // a single entry at (0,0) means the server ignored the
                    // advertised layout and opened one screen.
                    println!(
                        "observed_monitors count={} {}",
                        monitors.len(),
                        monitors
                            .iter()
                            .map(|m| {
                                format!(
                                    "{}x{}+{}+{}",
                                    m.width, m.height, m.left, m.top
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                }
                Ok(HostMsg::MonitorLayoutResult { epoch, request_id, result }) => {
                    // The helper only reports that it wrote the layout; the
                    // server's acceptance is not observable on this channel,
                    // so the result is printed verbatim and the topology that
                    // follows in `MonitorLayoutObserved` is what says whether
                    // the server acted on it.
                    println!(
                        "layout_result request={request_id} epoch={epoch} result={}",
                        match result {
                            Ok(()) => "ok".to_owned(),
                            Err(error) => format!("error: {error}"),
                        }
                    );
                }
                Ok(HostMsg::Pointer(update)) => {
                    pointer_events += 1;
                    let description = match &update {
                        adit_rdp_proto::PointerUpdate::Default => "default".to_owned(),
                        adit_rdp_proto::PointerUpdate::Hidden => "hidden".to_owned(),
                        adit_rdp_proto::PointerUpdate::Position { x, y } => {
                            format!("position {x},{y}")
                        }
                        adit_rdp_proto::PointerUpdate::Bitmap {
                            width,
                            height,
                            hotspot_x,
                            hotspot_y,
                            rgba,
                        } => {
                            let shape = (*width, *height, *hotspot_x, *hotspot_y);
                            if !pointer_shapes.contains(&shape) {
                                pointer_shapes.push(shape);
                            }
                            format!(
                                "bitmap {width}x{height} hotspot {hotspot_x},{hotspot_y} bytes {}",
                                rgba.len()
                            )
                        }
                    };
                    match last_move {
                        Some((x, y)) => {
                            println!("pointer_event={pointer_events} client_pos={x},{y} {description}")
                        }
                        None => {
                            println!("pointer_event={pointer_events} client_pos=none {description}")
                        }
                    }
                    if let (Some(dir), adit_rdp_proto::PointerUpdate::Bitmap {
                        width,
                        height,
                        hotspot_x,
                        hotspot_y,
                        rgba,
                    }) = (pointer_dump.as_deref(), &update)
                    {
                        let shape = (*width, *height, *hotspot_x, *hotspot_y);
                        if !dumped_shapes.contains(&shape) {
                            dumped_shapes.push(shape);
                            let index = dumped_shapes.len();
                            let path = std::path::Path::new(dir).join(format!(
                                "shape-{index:02}-{width}x{height}-hot{hotspot_x},{hotspot_y}.png"
                            ));
                            if let Some(parent) = path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let _ = image::save_buffer(
                                &path,
                                rgba,
                                u32::from(*width),
                                u32::from(*height),
                                image::ColorType::Rgba8,
                            );
                            println!("pointer_shape_dumped={}", path.display());
                        }
                    }
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
                } else if let Some(layout) = command.strip_prefix("LAYOUT ") {
                    // Drive a DisplayControl layout change on a live session,
                    // the way the app's layout switcher does. The syntax
                    // matches `--monitors`: `WIDTHxHEIGHT` pairs separated by
                    // commas, the first one primary and at the origin, the
                    // rest laid out left to right.
                    //
                    // This is the post-connect half of multi-monitor support.
                    // The connect-time GCC block asks the server for a layout;
                    // this asks the server to *change* to one, which is the
                    // only path that can show whether the server honours a
                    // layout it did not start with.
                    let mut entries: Vec<adit_rdp_proto::Monitor> = Vec::new();
                    for (index, entry) in layout.split(',').enumerate() {
                        let (w, h) = entry
                            .trim()
                            .split_once('x')
                            .ok_or("LAYOUT requires WIDTHxHEIGHT entries")?;
                        let width: u16 = w.parse()?;
                        let height: u16 = h.parse()?;
                        entries.push(adit_rdp_proto::Monitor {
                            left: (index as i32) * i32::from(width),
                            top: 0,
                            width,
                            height,
                            primary: index == 0,
                            scale_percent: 100,
                        });
                    }
                    if entries.is_empty() {
                        return Err("LAYOUT requires at least one monitor".into());
                    }
                    layout_request_id += 1;
                    write_msg(
                        &mut wire,
                        &ClientMsg::input_at(
                            input_epoch,
                            InputEvent::MonitorLayout {
                                request_id: layout_request_id,
                                monitors: entries,
                            },
                        ),
                    )?;
                    println!("layout_requested request={layout_request_id}");
                } else if let Some(point) = command
                    .strip_prefix("CLICK ")
                    .or_else(|| command.strip_prefix("MOVE "))
                {
                    let parts: Vec<_> = point.split_whitespace().collect();
                    if parts.len() != 2 {
                        return Err("pointer command requires x y".into());
                    }
                    let move_x: u16 = parts[0].parse()?;
                    let move_y: u16 = parts[1].parse()?;
                    last_move = Some((move_x, move_y));
                    write_msg(
                        &mut wire,
                        &ClientMsg::input_at(
                            input_epoch,
                            InputEvent::MouseMove {
                                x: move_x,
                                y: move_y,
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
    println!(
        "pointer_summary events={pointer_events} distinct_shapes={}",
        pointer_shapes.len()
    );
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
