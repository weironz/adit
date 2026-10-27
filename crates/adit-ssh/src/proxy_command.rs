//! A child owns only its configured argv and stdio; SSH credentials never enter
//! its command line, inherited environment, stderr, or diagnostic messages.
use super::*;
use std::{
    io,
    pin::Pin,
    process::{Child, Command, Stdio},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(test)]
mod tests;

pub(super) struct Process {
    owner: Arc<Owner>,
    io: Option<ProcessIo>,
}

struct Owner {
    child: Mutex<Option<Child>>,
    #[cfg(windows)]
    job: job::Job,
    #[cfg(unix)]
    group: u32,
}

impl Owner {
    fn exited(&self) -> bool {
        let mut slot = self.child.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_mut()
            .is_none_or(|child| !matches!(child.try_wait(), Ok(None)))
    }

    fn stop(&self) {
        let mut slot = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut child) = slot.take() {
            #[cfg(windows)]
            self.job.terminate();
            #[cfg(unix)]
            // The child starts a fresh process group. Do not signal Adit's group.
            unsafe {
                libc::kill(-(self.group as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            // Reap before dropping the runtime; relying on Tokio's best-effort
            // orphan reaper leaves cancelled handshakes dependent on a live loop.
            let _ = child.wait();
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // russh may already own the IO in its background kex task when a
        // connect future is cancelled. Its copy must not keep the child alive.
        self.owner.stop();
    }
}

struct ProcessIo {
    input: Option<tokio::fs::File>,
    output: tokio::fs::File,
    _owner: Arc<Owner>,
}

impl AsyncRead for ProcessIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.output).poll_read(cx, buf) {
            Poll::Ready(Ok(())) if buf.filled().len() == before && buf.remaining() > 0 => {
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "ProxyCommand 已退出或关闭输出",
                )))
            }
            result => result,
        }
    }
}

impl AsyncWrite for ProcessIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.input.as_mut() {
            Some(input) => Pin::new(input).poll_write(cx, buf),
            None => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.input.as_mut() {
            Some(input) => Pin::new(input).poll_flush(cx),
            None => Poll::Ready(Ok(())),
        }
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = match self.input.as_mut() {
            Some(input) => Pin::new(input).poll_shutdown(cx),
            None => return Poll::Ready(Ok(())),
        };
        if result.is_ready() {
            // Tokio's File shutdown flushes but does not close a pipe. Drop
            // our write end explicitly so the child receives stdin EOF.
            self.input.take();
        }
        result
    }
}

#[cfg(windows)]
fn pipe_file(pipe: impl Into<std::os::windows::io::OwnedHandle>) -> std::fs::File {
    std::fs::File::from(pipe.into())
}
#[cfg(unix)]
fn pipe_file(pipe: impl Into<std::os::fd::OwnedFd>) -> std::fs::File {
    std::fs::File::from(pipe.into())
}

impl Process {
    async fn spawn(request: &LiveShellRequest) -> Result<Self, SshError> {
        let args = request
            .proxy_command
            .expanded_args(&request.host, request.port, &request.username)
            .map_err(SshError::ProxyCommand)?;
        let program = request.proxy_command.program.clone();
        tokio::task::spawn_blocking(move || {
            let mut command = Command::new(program);
            command
                .args(args)
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                // Keep only the OS loader's directories, never application
                // secrets, SSH_AUTH_SOCK, or profile environment overrides.
                for name in ["SystemRoot", "WINDIR"] {
                    if let Some(value) = env::var_os(name) {
                        command.env(name, value);
                    }
                }
                // Suspend until assigned to our job; otherwise a fast launcher
                // could create an untracked descendant before assignment.
                command.creation_flags(0x0800_0004); // CREATE_NO_WINDOW | CREATE_SUSPENDED
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            let mut child = command.spawn().map_err(|_| {
                SshError::ProxyCommand("ProxyCommand 程序启动失败；请检查路径和执行权限")
            })?;
            #[cfg(windows)]
            let job = match job::Job::attach(&child) {
                Ok(job) => job,
                Err(()) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SshError::ProxyCommand(
                        "无法为 ProxyCommand 建立进程清理边界",
                    ));
                }
            };
            // Piped handles are guaranteed after a successful spawn.
            let input =
                tokio::fs::File::from_std(pipe_file(child.stdin.take().expect("piped stdin")));
            let output =
                tokio::fs::File::from_std(pipe_file(child.stdout.take().expect("piped stdout")));
            let owner = Arc::new(Owner {
                #[cfg(unix)]
                group: child.id(),
                child: Mutex::new(Some(child)),
                #[cfg(windows)]
                job,
            });
            // Reap a peer that exits while SFTP is idle waiting for a command,
            // and terminate any descendants even if a remote handle lingers.
            let weak = Arc::downgrade(&owner);
            thread::Builder::new()
                .name("adit-proxy-reaper".into())
                .spawn(move || {
                    while let Some(owner) = weak.upgrade() {
                        if owner.exited() {
                            owner.stop();
                            break;
                        }
                        drop(owner);
                        thread::sleep(Duration::from_millis(50));
                    }
                })
                .map_err(|_| SshError::ProxyCommand("无法启动 ProxyCommand 清理任务"))?;
            Ok(Self {
                io: Some(ProcessIo {
                    input: Some(input),
                    output,
                    _owner: owner.clone(),
                }),
                owner,
            })
        })
        .await
        .map_err(|_| SshError::ProxyCommand("ProxyCommand 启动任务失败"))?
    }
}

/// The caller retains `process` across authentication, channel use and shared
/// session claims. Dropping the caller also kills a process held by a kex task.
pub(super) async fn connect_endpoint(
    request: &LiveShellRequest,
    config: Arc<client::Config>,
    handler: KnownHostsClient,
    process: &mut Option<Process>,
) -> Result<client::Handle<KnownHostsClient>, SshError> {
    request
        .proxy_command
        .validate_route(
            &request.host,
            request.port,
            &request.username,
            !request.jumps.is_empty(),
        )
        .map_err(SshError::ProxyCommand)?;
    if !request.proxy_command.enabled {
        return client::connect(config, (request.host.as_str(), request.port), handler).await;
    }
    *process = Some(Process::spawn(request).await?);
    let stream = process
        .as_mut()
        .and_then(|p| p.io.take())
        .expect("fresh process stdio");
    client::connect_stream(config, stream, handler).await
}

#[cfg(windows)]
mod job {
    use std::{mem, os::windows::io::AsRawHandle, process::Child};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };
    pub(super) struct Job(isize);
    impl Job {
        pub(super) fn attach(child: &Child) -> Result<Self, ()> {
            // A HANDLE can be used across threads; keeping its integer form
            // avoids claiming Send/Sync for an arbitrary raw pointer.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(());
            }
            let job = Self(handle as isize);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    mem::size_of_val(&info) as u32,
                ) != 0
                    && AssignProcessToJobObject(handle, child.as_raw_handle()) != 0
            };
            if ok && resume_child(child.id()) {
                Ok(job)
            } else {
                Err(())
            }
        }
        pub(super) fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.0 as HANDLE, 1);
            }
        }
    }
    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0 as HANDLE);
            }
        }
    }

    fn resume_child(pid: u32) -> bool {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return false;
            }
            let mut entry: THREADENTRY32 = mem::zeroed();
            entry.dwSize = mem::size_of_val(&entry) as u32;
            let mut has_entry = Thread32First(snapshot, &mut entry) != 0;
            let mut resumed = false;
            while has_entry {
                if entry.th32OwnerProcessID == pid {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        resumed = ResumeThread(thread) != u32::MAX;
                        CloseHandle(thread);
                    }
                    break;
                }
                has_entry = Thread32Next(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
            resumed
        }
    }
}
