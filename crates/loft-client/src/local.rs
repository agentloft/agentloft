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

    /// The pipe's security already keeps other users out.
    pub fn peer_allowed(_stream: &ServerStream, _home_dir: &Path) -> bool {
        true
    }
}
