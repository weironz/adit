use adit_domain::{ConnectionProfile, Protocol};
use adit_rdp_proto::{
    read_ipc_hello, read_msg, write_ipc_hello, write_msg, ClientMsg, ClipFile, HostMsg,
    IPC_PROTOCOL_VERSION,
};
use adit_storage::ProfileStore;
use std::io::{self, Write};
use std::path::Path;

const ROOT: &str = "RDP-Promise-Fixture";
const NAME: &str = "pattern.bin";
const SIZE: usize = 1_048_613;
const GENERATION: u64 = 1;
const COMPANION: &[u8] = b"second top-level file\n";

fn payload() -> Vec<u8> {
    (0..SIZE).map(|index| (index % 251) as u8).collect()
}

fn seed(config: &Path) {
    std::fs::create_dir_all(config).unwrap();
    let mut profile =
        ConnectionProfile::new("Synthetic macOS RDP", "127.0.0.1", 3389, "fixture");
    profile.protocol = Protocol::Rdp;
    ProfileStore::new(config.join("profiles.json"))
        .save_profiles(&[profile])
        .unwrap();
}

fn helper() -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    assert_eq!(read_ipc_hello(&mut input)?, IPC_PROTOCOL_VERSION);
    write_ipc_hello(&mut output, IPC_PROTOCOL_VERSION)?;
    output.flush()?;
    let Some(ClientMsg::Connect(connect)) = read_msg(&mut input)? else {
        panic!("expected Connect");
    };
    assert_eq!(connect.host, "127.0.0.1");
    assert!(connect.enable_clipboard);
    eprintln!("HELLO: production GUI connected to synthetic helper; clipboard enabled");
    let mut send = |message| {
        write_msg(&mut output, &message)?;
        output.flush()
    };
    send(HostMsg::Connected { width: 640, height: 480 })?;
    send(HostMsg::Tile {
        x: 0,
        y: 0,
        width: 640,
        height: 480,
        rgba: [42, 71, 103, 255].repeat(640 * 480),
    })?;
    let scenario = std::env::var("ADIT_PROMISE_FIXTURE_SCENARIO").unwrap_or_default();
    let fail_read = scenario == "read-failure";
    let mut files = vec![
        ClipFile { name: format!("{ROOT}\\{NAME}"), size: SIZE as u64, is_dir: false },
        ClipFile { name: format!("{ROOT}\\empty.txt"), size: 0, is_dir: false },
    ];
    if scenario == "multi-file" {
        files.push(ClipFile {
            name: "companion.txt".into(),
            size: COMPANION.len() as u64,
            is_dir: false,
        });
    }
    send(HostMsg::ClipboardFiles {
        generation: GENERATION,
        files,
    })?;
    eprintln!("OFFER: generation={GENERATION} folder={ROOT} bytes={SIZE}");
    let data = payload();
    while let Some(message) = read_msg(&mut input)? {
        match message {
            ClientMsg::RequestFileContents { generation, stream_id, index, offset, length } => {
                assert_eq!(generation, GENERATION);
                let bytes = match index {
                    0 => data.as_slice(),
                    2 if scenario == "multi-file" => COMPANION,
                    _ => panic!("unexpected file contents request for index {index}"),
                };
                let start = usize::try_from(offset).unwrap();
                let end = start.checked_add(length as usize).unwrap();
                assert!(start < end && end <= bytes.len());
                eprintln!(
                    "REQUEST: generation={generation} stream={stream_id} index={index} offset={offset} length={length}"
                );
                let data = if fail_read && start > 0 {
                    eprintln!("REFUSED: scripted read failure after the first range");
                    None
                } else {
                    Some(bytes[start..end].to_vec())
                };
                send(HostMsg::FileContents {
                    generation,
                    stream_id,
                    data,
                })?;
            }
            ClientMsg::ClipboardFiles(files) | ClientMsg::EpochClipboardFiles { files, .. } => {
                assert!(files.is_empty(), "local file metadata must not be sent");
            }
            ClientMsg::Close => break,
            _ => {}
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    match args.next().as_deref().and_then(std::ffi::OsStr::to_str) {
        Some("--seed") => {
            seed(Path::new(&args.next().expect("config directory")));
            Ok(())
        }
        Some("--fixture") => std::fs::write(args.next().expect("reference file"), payload()),
        None => helper(),
        Some(_) => panic!("unexpected argument"),
    }
}
