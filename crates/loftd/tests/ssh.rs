//! Agentloft on another computer over SSH, end to end: a real daemon, the
//! real `agentloftd bridge`, and the client's real remote command, with a
//! stand-in `ssh` that runs that command here (as sshd would there).

use loft_client::ssh::connect_ssh_with;
use loft_config::Paths;
use loft_harness::Harness;
use loft_protocol::*;
use loftd::{Daemon, DaemonOptions};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

/// A stand-in `ssh`: runs its last argument (the remote command) with `sh`,
/// in a "remote" home `home` (and Agentloft's data in `agentloft_home`).
fn fake_ssh(dir: &Path, home: &Path, agentloft_home: &Path) -> std::path::PathBuf {
    let script = dir.join("ssh");
    std::fs::write(
        &script,
        format!(
            // The command also looks in fixed places (/Applications, …):
            // moved out of reach, so an Agentloft installed on the computer
            // running the tests is never the one found.
            "#!/bin/sh\nfor last; do :; done\n\
             last=$(printf '%s' \"$last\" | sed 's#/Applications/#/nonexistent/#g; \
             s#/usr/local/bin/#/nonexistent/#g; s#/opt/homebrew/bin/#/nonexistent/#g')\n\
             HOME='{}' AGENTLOFT_HOME='{}' PATH=/usr/bin:/bin exec /bin/sh -c \"$last\"\n",
            home.display(),
            agentloft_home.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[tokio::test]
async fn a_daemon_elsewhere_is_reached_through_its_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("agentloft"));
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(loft_harness::fake::FakeHarness)];
    let daemon = Daemon::start(DaemonOptions {
        paths: paths.clone(),
        harnesses,
        capture_login_env: false,
        mcp_bridge: None,
        web_bundle: None,
    })
    .await
    .unwrap();

    // Agentloft installed "there" where the remote command looks first.
    let home = dir.path().join("home");
    let bin = home.join(".agentloft/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_agentloftd"), bin.join("agentloftd")).unwrap();
    let ssh = fake_ssh(dir.path(), &home, &paths.home);

    let (client, _events) = connect_ssh_with(
        ssh.as_os_str(),
        "me@there",
        ClientKind::RemoteUi,
        "test",
        &[],
    )
    .await
    .expect("connected through the bridge");
    // The same connection the desktop app has there: not a web session.
    assert!(client.welcome().remote.is_none());
    match client.request(Request::UiFiles { known: None }).await {
        Ok(Response::UiFiles { files: Some(files) }) => {
            assert_eq!(Path::new(&files.home), paths.home);
        }
        other => panic!("unexpected: {other:?}"),
    }
    drop(client);
    daemon.shutdown().await;
}

#[tokio::test]
async fn a_computer_without_agentloft_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ssh = fake_ssh(dir.path(), &home, &dir.path().join("agentloft"));
    let err = connect_ssh_with(ssh.as_os_str(), "mini", ClientKind::RemoteUi, "test", &[])
        .await
        .err()
        .expect("nothing to connect to");
    assert_eq!(err.to_string(), "Agentloft isn't installed on mini.");
}
