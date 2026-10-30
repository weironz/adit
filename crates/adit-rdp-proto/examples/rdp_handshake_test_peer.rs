//! Synthetic process peer for the session client's IPC negotiation tests.
//! It never opens a network connection or reads application configuration.
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use adit_rdp_proto::{
    read_client_hello, read_msg, write_helper_hello, write_msg, ClientMsg, ClipFile,
    ClipboardStamp, ConnectRequest, HostMsg, InputEvent, RemoteClipboardStamp,
    SharedClipboardPolicy,
};
use serde::Deserialize;

// This peer models a separately built helper; it must not follow the app's
// protocol constant automatically when their layouts diverge.
const IPC_PROTOCOL_VERSION: u32 = 9;

// Keep the old discriminants and affected bincode layouts for the old-helper test.
#[allow(dead_code)]
#[derive(Deserialize)]
enum LegacyClientMsg {
    Connect(ConnectRequest),
    Input(InputEvent),
    ClipboardImage(Vec<u8>),
    ClipboardFiles(Vec<ClipFile>),
    FileContents {
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
    RequestFileContents {
        generation: u64,
        stream_id: u32,
        index: u32,
        offset: u64,
        length: u32,
    },
    Close,
}

fn main() -> io::Result<()> {
    let mode = std::env::var("ADIT_RDP_IPC_TEST_MODE").unwrap_or_default();
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    if mode == "legacy" {
        return if read_msg::<_, LegacyClientMsg>(&mut input).is_err() {
            Ok(())
        } else {
            Err(io::Error::other(
                "legacy helper received an ordinary message",
            ))
        };
    }
    if read_client_hello(&mut input)? != IPC_PROTOCOL_VERSION {
        return Err(io::Error::other("unexpected client IPC version"));
    }
    if mode == "late_ack" {
        let signal = std::env::var_os("ADIT_RDP_IPC_TEST_SIGNAL")
            .ok_or_else(|| io::Error::other("missing synthetic signal"))?;
        eprintln!("hello-ready");
        let deadline = Instant::now() + Duration::from_secs(15);
        while !std::path::Path::new(&signal).exists() {
            if Instant::now() >= deadline {
                return Err(io::Error::other("synthetic signal timed out"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        write_helper_hello(&mut output, IPC_PROTOCOL_VERSION)?;
        let mut received = Vec::new();
        input.read_to_end(&mut received)?;
        return if received.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other("client wrote bytes after cancellation"))
        };
    }
    if mode == "blocked_connect" {
        write_helper_hello(&mut output, IPC_PROTOCOL_VERSION)?;
        let mut length = [0; 4];
        input.read_exact(&mut length)?;
        eprintln!("connect-started");
        // The client must terminate this process when the pending write is
        // cancelled or reaches the handshake deadline.
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    if mode == "silent" {
        return match read_msg::<_, ClientMsg>(&mut input) {
            Ok(None) => Ok(()),
            _ => Err(io::Error::other(
                "client sent a message without an acknowledgement",
            )),
        };
    }
    match mode.as_str() {
        "mismatch" => write_helper_hello(&mut output, IPC_PROTOCOL_VERSION + 1)?,
        "old_v3" => write_helper_hello(&mut output, 3)?,
        "old_v4" => write_helper_hello(&mut output, 4)?,
        "old_v5" => write_helper_hello(&mut output, 5)?,
        "old_v6" => write_helper_hello(&mut output, 6)?,
        "old_v7" => write_helper_hello(&mut output, 7)?,
        "old_v8" => write_helper_hello(&mut output, 8)?,
        "missing_ack" => write_msg(
            &mut output,
            &HostMsg::Connected {
                width: 2,
                height: 2,
            },
        )?,
        "matching" | "notice" | "suspend" | "channel" | "policy" | "flood" => {
            write_helper_hello(&mut output, IPC_PROTOCOL_VERSION)?
        }
        _ => return Err(io::Error::other("unknown synthetic peer mode")),
    }
    if mode != "matching"
        && mode != "notice"
        && mode != "suspend"
        && mode != "channel"
        && mode != "policy"
        && mode != "flood"
    {
        return match read_msg::<_, ClientMsg>(&mut input) {
            Ok(None) => Ok(()),
            _ => Err(io::Error::other(
                "client sent a message before a matching acknowledgement",
            )),
        };
    }
    let request = match read_msg::<_, ClientMsg>(&mut input)? {
        Some(ClientMsg::Connect(request))
            if request.host == "adit.invalid"
                && request.username == "synthetic-user"
                && request.password == "synthetic-password" =>
        {
            request
        }
        _ => {
            return Err(io::Error::other(
                "expected the synthetic Connect after acknowledgement",
            ))
        }
    };
    let policy = if mode == "policy" || mode == "flood" {
        Some(SharedClipboardPolicy::open_in_child(
            request
                .clipboard_policy_id
                .as_deref()
                .ok_or_else(|| io::Error::other("missing policy mapping"))?,
        )?)
    } else {
        None
    };
    match read_msg::<_, ClientMsg>(&mut input)? {
        Some(ClientMsg::ClipboardFiles {
            generation: 17,
            files,
        }) if files.len() == 1 && files[0].name == "synthetic.bin" => {}
        _ => {
            return Err(io::Error::other(
                "expected the queued synthetic file offer after Connect",
            ))
        }
    }
    write_msg(
        &mut output,
        &HostMsg::Connected {
            width: 2,
            height: 2,
        },
    )?;
    if mode == "flood" {
        let policy = policy.unwrap();
        let (epoch, enabled, content_revision) = policy.load_full();
        write_msg(
            &mut output,
            &HostMsg::ClipboardPolicyApplied {
                policy_epoch: epoch,
                enabled,
            },
        )?;
        write_msg(
            &mut output,
            &HostMsg::ClipboardChannelReady { channel_epoch: 1 },
        )?;
        for copy_revision in 1..=128 {
            let stamp = RemoteClipboardStamp {
                local: ClipboardStamp {
                    policy_epoch: epoch,
                    content_revision,
                },
                channel_epoch: 1,
                copy_revision,
            };
            write_msg(&mut output, &HostMsg::ClipboardRemoteCopy(stamp))?;
            write_msg(
                &mut output,
                &HostMsg::ClipboardImageForRevision {
                    stamp,
                    image: vec![copy_revision as u8; 256 * 1024],
                },
            )?;
        }
        write_msg(
            &mut output,
            &HostMsg::Tile {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                rgba: vec![9, 8, 7, 255],
            },
        )?;
        eprintln!("flood-flushed");
        loop {
            match read_msg::<_, ClientMsg>(&mut input)? {
                Some(ClientMsg::Input(InputEvent::Key {
                    scancode: 30,
                    pressed: true,
                    ..
                })) => break,
                Some(_) => {}
                None => {
                    return Err(io::Error::other(
                        "desktop input was lost during clipboard flood",
                    ))
                }
            }
        }
        return write_msg(&mut output, &HostMsg::Closed);
    }
    if mode == "policy" {
        let policy = policy.unwrap();
        let (initial_epoch, initial_enabled) = policy.load();
        write_msg(
            &mut output,
            &HostMsg::ClipboardPolicyApplied {
                policy_epoch: initial_epoch,
                enabled: initial_enabled,
            },
        )?;
        write_msg(
            &mut output,
            &HostMsg::ClipboardChannelReady { channel_epoch: 1 },
        )?;
        let mut old = Vec::new();
        write_msg(
            &mut old,
            &HostMsg::ClipboardFilesForPolicy {
                policy_epoch: 0,
                generation: 7,
                files: vec![ClipFile {
                    name: "old.bin".into(),
                    size: 1,
                    is_dir: false,
                }],
            },
        )?;
        output.write_all(&old[..4])?;
        output.flush()?;
        eprintln!("old-file-frame-started");
        let deadline = Instant::now() + Duration::from_secs(5);
        let epoch = loop {
            let (epoch, enabled) = policy.load();
            if epoch >= 2 && enabled {
                break epoch;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("policy mapping did not update"));
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        output.write_all(&old[4..])?;
        write_msg(
            &mut output,
            &HostMsg::ClipboardFilesForPolicy {
                policy_epoch: epoch,
                generation: 8,
                files: vec![ClipFile {
                    name: "new.bin".into(),
                    size: 1,
                    is_dir: false,
                }],
            },
        )?;
        eprintln!("new-file-before-confirmation");
        let mut saw_policy = false;
        let mut saw_key = false;
        while !saw_policy || !saw_key {
            match read_msg::<_, ClientMsg>(&mut input)? {
                Some(ClientMsg::ClipboardPolicyEpoch(received)) if received >= 2 => {
                    saw_policy = true
                }
                Some(ClientMsg::Input(InputEvent::Key { .. })) => saw_key = true,
                Some(_) => {}
                None => return Err(io::Error::other("missing policy or key")),
            }
        }
        write_msg(
            &mut output,
            &HostMsg::ClipboardPolicyApplied {
                policy_epoch: epoch,
                enabled: true,
            },
        )?;
        let mut read = false;
        while !read {
            match read_msg::<_, ClientMsg>(&mut input)? {
                Some(ClientMsg::RequestFileContents {
                    generation: 8,
                    stream_id: 91,
                    ..
                }) => {
                    write_msg(
                        &mut output,
                        &HostMsg::FileContents {
                            generation: 8,
                            stream_id: 91,
                            data: Some(vec![42]),
                        },
                    )?;
                    read = true;
                }
                Some(_) => {}
                None => return Err(io::Error::other("desktop or reverse file request missing")),
            }
        }
    }
    if mode == "channel" {
        let request = |channel_epoch| HostMsg::FileContentsNeeded {
            channel_epoch,
            snapshot_revision: 1,
            generation: 17,
            stream_id: 41,
            index: 0,
            offset: 0,
            length: 1,
        };
        write_msg(&mut output, &request(7))?;
        if !matches!(
            read_msg::<_, ClientMsg>(&mut input)?,
            Some(ClientMsg::Input(InputEvent::Key { .. }))
        ) {
            return Err(io::Error::other(
                "expected the synthetic channel-change barrier",
            ));
        }
        write_msg(
            &mut output,
            &HostMsg::ClipboardChannelChanged { channel_epoch: 8 },
        )?;
        write_msg(
            &mut output,
            &HostMsg::ClipboardLocalFileDirectionReady { channel_epoch: 9 },
        )?;
        write_msg(&mut output, &request(9))?;
        if !matches!(read_msg::<_, ClientMsg>(&mut input)?,
            Some(ClientMsg::BoundFileContents { request, data: Some(data) })
                if request.channel_epoch == 9 && request.stream_id == 41 && data == [8])
        {
            return Err(io::Error::other(
                "expected only the new channel's bound reply",
            ));
        }
    }
    if mode == "suspend" {
        for _ in 0..2 {
            write_msg(
                &mut output,
                &HostMsg::FileContentsNeeded {
                    channel_epoch: 7,
                    snapshot_revision: 1,
                    generation: 17,
                    stream_id: 41,
                    index: 0,
                    offset: 0,
                    length: 1,
                },
            )?;
        }
        if !matches!(
            read_msg::<_, ClientMsg>(&mut input)?,
            Some(ClientMsg::SuspendLocalFileDirection)
        ) {
            return Err(io::Error::other(
                "expected direction suspension, not a desktop close or guessed reply",
            ));
        }
        write_msg(
            &mut output,
            &HostMsg::LocalFileDirectionSuspended { channel_epoch: 7 },
        )?;
        write_msg(
            &mut output,
            &HostMsg::Tile {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                rgba: vec![9, 8, 7, 255],
            },
        )?;
        if !matches!(
            read_msg::<_, ClientMsg>(&mut input)?,
            Some(ClientMsg::Input(InputEvent::Key {
                scancode: 30,
                pressed: true,
                ..
            }))
        ) {
            return Err(io::Error::other("desktop input did not survive suspension"));
        }
        if !matches!(
            read_msg::<_, ClientMsg>(&mut input)?,
            Some(ClientMsg::RequestFileContents {
                generation: 77,
                stream_id: 9,
                ..
            })
        ) {
            return Err(io::Error::other(
                "opposite file direction did not survive suspension",
            ));
        }
        write_msg(
            &mut output,
            &HostMsg::FileContents {
                generation: 77,
                stream_id: 9,
                data: Some(vec![42]),
            },
        )?;
    }
    if mode == "notice" {
        write_msg(
            &mut output,
            &HostMsg::ClipboardFileDirectionUnavailable {
                channel_epoch: 7,
                message: "本地文件发送至远端已暂时禁用；桌面连接仍可使用。".into(),
            },
        )?;
        write_msg(
            &mut output,
            &HostMsg::Tile {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                rgba: vec![9, 8, 7, 255],
            },
        )?;
        match read_msg::<_, ClientMsg>(&mut input)? {
            Some(ClientMsg::Input(InputEvent::Key {
                scancode: 30,
                pressed: true,
                ..
            })) => {}
            _ => return Err(io::Error::other("desktop input did not survive notice")),
        }
    }
    write_msg(&mut output, &HostMsg::Closed)
}
