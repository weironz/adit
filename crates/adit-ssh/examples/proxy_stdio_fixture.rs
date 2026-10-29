//! Synthetic ProxyCommand peer. Build with `cargo build -p adit-ssh --example
//! proxy_stdio_fixture`, then set ADIT_PROXY_FIXTURE to its absolute path and run
//! `cargo test -p adit-ssh proxy_command::tests -- --include-ignored --test-threads=1`.
//! It only uses loopback TCP and test-owned files, never SSH credentials.
use std::{
    env, fs,
    io::{self, Read, Write},
    net::TcpStream,
    process, thread,
    time::Duration,
};

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    let pid_file = args
        .get(2)
        .ok_or_else(|| io::Error::other("missing fixture pid path"))?;
    fs::write(pid_file, process::id().to_string())?;
    match mode {
        "inspect" => {
            assert!(env::var_os("ADIT_PROXY_PARENT_SECRET").is_none());
            assert!(env::var_os("SSH_AUTH_SOCK").is_none());
            let mut out = io::stdout().lock();
            for arg in &args[3..] {
                out.write_all(&(arg.len() as u32).to_le_bytes())?;
                out.write_all(arg.as_bytes())?;
            }
            out.flush()?;
        }
        "echo" => {
            let mut input = io::stdin().lock();
            let mut output = io::stdout().lock();
            let mut bytes = [0; 4096];
            loop {
                let len = input.read(&mut bytes)?;
                if len == 0 {
                    break;
                }
                output.write_all(&bytes[..len])?;
                output.flush()?;
            }
        }
        "exit" => process::exit(7),
        "stall" => thread::sleep(Duration::from_secs(60)),
        "descendant" => {
            let mut child = process::Command::new(env::current_exe()?)
                .args(["stall", &format!("{pid_file}.child")])
                .stdin(process::Stdio::null())
                .stdout(process::Stdio::null())
                .stderr(process::Stdio::null())
                .spawn()?;
            let _ = child.wait();
        }
        "bridge" | "bridge-close-output-hold" => {
            if args.get(3).map(String::as_str) != Some("127.0.0.1") {
                return Err(io::Error::other("fixture only connects to IPv4 loopback"));
            }
            let socket = TcpStream::connect(("127.0.0.1", args[4].parse::<u16>().unwrap()))?;
            let mut writer = socket.try_clone()?;
            thread::spawn(move || {
                let _ = io::copy(&mut io::stdin().lock(), &mut writer);
            });
            let mut reader = socket;
            let mut out = io::stdout().lock();
            let mut bytes = [0; 4096];
            loop {
                let len = reader.read(&mut bytes)?;
                if len == 0 {
                    break;
                }
                out.write_all(&bytes[..len])?;
                out.flush()?;
            }
            if mode == "bridge-close-output-hold" {
                drop(out);
                close_stdout()?;
                thread::sleep(Duration::from_secs(60));
            }
        }
        _ => return Err(io::Error::other("invalid fixture mode")),
    }
    Ok(())
}

// Exercise a child that closes only its output pipe but keeps running. The
// production owner must terminate it even while a Tokio pipe read is pending.
#[cfg(windows)]
fn close_stdout() -> io::Result<()> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(kind: u32) -> *mut std::ffi::c_void;
    }
    let handle = unsafe { GetStdHandle((-11_i32) as u32) };
    if handle.is_null() || handle as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    drop(unsafe { OwnedHandle::from_raw_handle(handle) });
    Ok(())
}

#[cfg(unix)]
fn close_stdout() -> io::Result<()> {
    if unsafe { libc::close(1) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
