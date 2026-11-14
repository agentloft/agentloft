//! Reaching Agentloft on another computer over SSH.
//!
//! `ssh <host> agentloftd bridge` joins the remote daemon's socket to the
//! SSH session's stdin and stdout, and the frames go over that exactly as
//! over the local socket. SSH does the rest: it encrypts, it checks the
//! host's key against `known_hosts`, and it signs in with the user's own
//! keys and agent and `~/.ssh/config`. Agentloft opens no port on either
//! computer and keeps no secret of its own.
//!
//! SSH runs without a terminal (`BatchMode`), so it never stops to ask for
//! a password or to accept an unknown host key: those fail with a message
//! saying what to do in a terminal first.

pub use crate::valid_destination;
use crate::{Client, ClientEvent};
use anyhow::{Result, bail};
use loft_protocol::ClientKind;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::sync::mpsc;

/// What runs on the other computer: the first `agentloftd` found, as a
/// bridge. Plain `sh`, whatever the login shell is; no single quotes inside.
const REMOTE_COMMAND: &str = "sh -c 'for b in \
\"$HOME/.agentloft/bin/agentloftd\" \
/Applications/Agentloft.app/Contents/MacOS/agentloftd \
\"$HOME/Applications/Agentloft.app/Contents/MacOS/agentloftd\" \
/usr/local/bin/agentloftd /opt/homebrew/bin/agentloftd \"$HOME/.local/bin/agentloftd\" \
\"$(command -v agentloftd)\"; \
do if [ -n \"$b\" ] && [ -x \"$b\" ]; then exec \"$b\" bridge; fi; done; \
echo \"agentloft-not-installed\" >&2; exit 127'";

/// How long SSH and the daemon on the other end have to answer.
const HANDSHAKE: Duration = Duration::from_secs(45);

/// The `ssh` to run: `$AGENTLOFT_SSH` (tests), else the system's.
fn ssh_program() -> std::ffi::OsString {
    if let Some(program) = std::env::var_os("AGENTLOFT_SSH") {
        return program;
    }
    if std::path::Path::new("/usr/bin/ssh").exists() {
        "/usr/bin/ssh".into()
    } else {
        "ssh".into()
    }
}

/// Connect to the daemon on `destination` (anything `ssh` takes as a host),
/// starting it there if it isn't running.
pub async fn connect_ssh(
    destination: &str,
    kind: ClientKind,
    build: &str,
    capabilities: &[&str],
) -> Result<(Client, mpsc::UnboundedReceiver<ClientEvent>)> {
    connect_ssh_with(&ssh_program(), destination, kind, build, capabilities).await
}

/// [`connect_ssh`] with `program` as `ssh` (tests stand one in).
pub async fn connect_ssh_with(
    program: &std::ffi::OsStr,
    destination: &str,
    kind: ClientKind,
    build: &str,
    capabilities: &[&str],
) -> Result<(Client, mpsc::UnboundedReceiver<ClientEvent>)> {
    if !valid_destination(destination) {
        bail!("“{destination}” isn't an SSH host");
    }
    let mut child = Command::new(program)
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "--",
            destination,
            REMOTE_COMMAND,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| anyhow::anyhow!("couldn't run ssh: {err}"))?;
    let (Some(stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        bail!("couldn't talk to ssh");
    };
    // What SSH says on the way, for when it doesn't work.
    let said = Arc::new(Mutex::new(String::new()));
    {
        let said = said.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut said = said.lock().unwrap_or_else(|e| e.into_inner());
                said.push_str(&String::from_utf8_lossy(&buf[..n]));
                // Only the end matters.
                if said.len() > 8192 {
                    let cut = said.len() - 4096;
                    let cut = (cut..said.len())
                        .find(|&i| said.is_char_boundary(i))
                        .unwrap_or(0);
                    said.drain(..cut);
                }
            }
        });
    }
    let stream = tokio::io::join(stdout, stdin);
    let handshake = tokio::time::timeout(
        HANDSHAKE,
        Client::connect_over(stream, kind, build, capabilities),
    )
    .await;
    match handshake {
        Ok(Ok(conn)) => {
            // SSH lives as long as the connection: it ends when the client
            // closes its side (the bridge then exits), or the link drops.
            let host = destination.to_owned();
            tokio::spawn(async move {
                let status = child.wait().await;
                tracing::info!(%host, ?status, "ssh ended");
            });
            Ok(conn)
        }
        failed => {
            // Let SSH finish saying why before it's stopped.
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            let _ = child.kill().await;
            let said = said.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let reason = match failed {
                Ok(Err(err)) => format!("{err:#}"),
                Err(_) => "it didn't answer in time".to_owned(),
                Ok(Ok(_)) => unreachable!(),
            };
            Err(anyhow::anyhow!(explain(destination, &said, &reason)))
        }
    }
}

/// What went wrong, from what SSH said, in words a person can act on.
fn explain(destination: &str, said: &str, fallback: &str) -> String {
    let has = |needle: &str| said.contains(needle);
    if has("agentloft-not-installed") {
        format!("Agentloft isn't installed on {destination}.")
    } else if has("unrecognized subcommand 'bridge'") {
        format!("The Agentloft on {destination} is too old to connect to: update it there.")
    } else if has("Host key verification failed") || has("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        format!(
            "SSH doesn't know {destination}'s key yet, or it changed. Run `ssh {destination}` in \
             Terminal once to check it."
        )
    } else if has("Permission denied") {
        format!(
            "SSH couldn't sign in to {destination}. Agentloft signs in with your SSH keys, not a \
             password: check that `ssh {destination}` works without one."
        )
    } else if has("Could not resolve hostname") {
        format!("Couldn't find {destination}.")
    } else if has("Connection refused") {
        format!("{destination} refused the connection: is Remote Login (SSH) on there?")
    } else if has("timed out") || has("Operation timed out") {
        format!("{destination} didn't answer.")
    } else {
        let last = said
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty() && !l.starts_with("Warning: Permanently added"));
        match last {
            Some(line) => format!("Couldn't reach Agentloft on {destination}: {line}"),
            None => format!("Couldn't reach Agentloft on {destination}: {fallback}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_host_names_are_passed_to_ssh() {
        for ok in [
            "mini",
            "me@mini.local",
            "ssh://me@10.0.0.2:2222",
            "box-1.tail1234.ts.net",
        ] {
            assert!(valid_destination(ok), "{ok}");
        }
        for bad in [
            "",
            "-oProxyCommand=evil",
            "me@host; rm -rf ~",
            "host name",
            "a\nb",
            "$(evil)",
        ] {
            assert!(!valid_destination(bad), "{bad:?}");
        }
    }

    #[test]
    fn ssh_failures_say_what_to_do() {
        assert!(explain("mini", "agentloft-not-installed\n", "x").contains("isn't installed"));
        assert!(
            explain("mini", "error: unrecognized subcommand 'bridge'\n", "x").contains("too old")
        );
        assert!(explain("mini", "Host key verification failed.\r\n", "x").contains("ssh mini"));
        assert!(
            explain("mini", "me@mini: Permission denied (publickey).\n", "x")
                .contains("without one")
        );
        assert_eq!(
            explain("mini", "", "closed"),
            "Couldn't reach Agentloft on mini: closed"
        );
    }
}
