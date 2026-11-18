//! The daemon's local socket, on every desktop platform: a Unix socket on
//! macOS and Linux, a named pipe on Windows (`Paths::socket_file` names
//! either). The daemon listens with [`Listener`]; the app, the CLI, the MCP
//! bridge and `agentloftd bridge` connect with [`connect`]. Both ends carry
//! the same length-prefixed frames.

use std::io;
use std::path::Path;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

/// Start `cmd` apart from this process, as the daemon is: its own process
/// group (signals for this one's terminal don't reach it) and, on Windows,
/// no console window. The daemon's children (git, agents, shells) share its
/// hidden console, so none of them flashes a window either.
pub fn detach(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
}

#[cfg(unix)]
mod unix {
    use super::*;

    /// The daemon's end of one connection.
    pub type ServerStream = tokio::net::UnixStream;
    /// A client's end of the connection.
    pub type ClientStream = tokio::net::UnixStream;

    pub async fn connect(socket: &Path) -> io::Result<ClientStream> {
        tokio::net::UnixStream::connect(socket).await
    }

    pub struct Listener(tokio::net::UnixListener);

    impl Listener {
        /// Listen at `socket`, replacing a stale socket file left there.
        pub async fn bind(socket: &Path) -> io::Result<Self> {
            let _ = std::fs::remove_file(socket);
            let listener = tokio::net::UnixListener::bind(socket)?;
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self(listener))
        }

        pub async fn accept(&mut self) -> io::Result<ServerStream> {
            self.0.accept().await.map(|(stream, _)| stream)
        }
    }

    /// Remove what [`Listener::bind`] left behind.
    pub fn unbind(socket: &Path) {
        let _ = std::fs::remove_file(socket);
    }

    /// Only the user who owns the data home may connect.
    pub fn peer_allowed(stream: &ServerStream, home_dir: &Path) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        let Ok(cred) = stream.peer_cred() else {
            return false;
        };
        let Ok(meta) = std::fs::metadata(home_dir) else {
            return false;
        };
        cred.uid() == meta.uid()
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    pub type ServerStream = NamedPipeServer;
    pub type ClientStream = NamedPipeClient;

    /// All of the pipe's instances are taken: wait for one to free up.
    const ERROR_PIPE_BUSY: i32 = 231;

    pub async fn connect(socket: &Path) -> io::Result<ClientStream> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match ClientOptions::new().open(socket.as_os_str()) {
                // Anyone may make a pipe of a name nobody holds: another
                // user's program could be serving ours while the daemon
                // isn't running, and hear everything the app says.
                Ok(client) if !same_user(pipe_process(&client, false)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "another user's program is serving Agentloft's pipe",
                    ));
                }
                Ok(client) => return Ok(client),
                Err(err)
                    if err.raw_os_error() == Some(ERROR_PIPE_BUSY)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// A named pipe serves one client per instance: the listener keeps
    /// one instance waiting, and makes the next as each client arrives.
    pub struct Listener {
        name: std::ffi::OsString,
        next: NamedPipeServer,
    }

    impl Listener {
        pub async fn bind(socket: &Path) -> io::Result<Self> {
            let name = socket.as_os_str().to_owned();
            // The first instance: refused while any instance of this pipe
            // exists, so nobody else can be serving it under our name.
            // Remote (SMB) clients are refused by default; the default
            // security lets only this user (and administrators) write to it.
            // A daemon that just stopped may still be closing its instances
            // (the home's lock is already free): give them a moment.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match ServerOptions::new().first_pipe_instance(true).create(&name) {
                    Ok(next) => return Ok(Self { name, next }),
                    Err(err)
                        if err.kind() == io::ErrorKind::PermissionDenied
                            && tokio::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        pub async fn accept(&mut self) -> io::Result<ServerStream> {
            self.next.connect().await?;
            let next = ServerOptions::new().create(&self.name)?;
            Ok(std::mem::replace(&mut self.next, next))
        }
    }

    /// A pipe disappears with its last handle.
    pub fn unbind(_socket: &Path) {}

    /// Only this user's programs (the pipe's default security already lets
    /// only them, administrators and the system write to it).
    pub fn peer_allowed(stream: &ServerStream, _home_dir: &Path) -> bool {
        same_user(pipe_process(stream, true))
    }

    /// The process at the other end of a pipe: its client (`client`) or
    /// its server.
    #[allow(unsafe_code)]
    fn pipe_process(pipe: &impl std::os::windows::io::AsRawHandle, client: bool) -> Option<u32> {
        use windows_sys::Win32::System::Pipes::{
            GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
        };
        let handle = pipe.as_raw_handle();
        let mut pid = 0u32;
        // SAFETY: a live pipe handle, and a place for the id.
        let ok = unsafe {
            if client {
                GetNamedPipeClientProcessId(handle, &mut pid)
            } else {
                GetNamedPipeServerProcessId(handle, &mut pid)
            }
        };
        (ok != 0).then_some(pid)
    }

    /// Whether process `pid` runs as the user this one does.
    fn same_user(pid: Option<u32>) -> bool {
        let Some(pid) = pid else { return false };
        match (process_user(Some(pid)), process_user(None)) {
            (Some(theirs), Some(mine)) => theirs == mine,
            _ => false,
        }
    }

    /// The account (SID, as bytes) process `pid` runs as; `None`: this one.
    #[allow(unsafe_code)]
    fn process_user(pid: Option<u32>) -> Option<Vec<u8>> {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{
            GetLengthSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
        };
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: every handle is checked before use and closed after;
        // the token's user is read from a buffer aligned for it, within
        // the length the system wrote.
        unsafe {
            let process: HANDLE = match pid {
                Some(pid) => OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid),
                None => GetCurrentProcess(),
            };
            if process.is_null() {
                return None;
            }
            let mut token: HANDLE = std::ptr::null_mut();
            let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token) != 0;
            if pid.is_some() {
                CloseHandle(process);
            }
            if !opened {
                return None;
            }
            // u64s: aligned for the pointers in TOKEN_USER.
            let mut buf = vec![0u64; 64];
            let mut len = 0u32;
            let read = GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut len,
            ) != 0;
            CloseHandle(token);
            if !read {
                return None;
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let sid = user.User.Sid;
            let n = GetLengthSid(sid) as usize;
            Some(std::slice::from_raw_parts(sid as *const u8, n).to_vec())
        }
    }
}
