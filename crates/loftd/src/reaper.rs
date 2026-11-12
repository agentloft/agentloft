//! Startup reaper (PLAN §5.4): agent processes recorded by a previous daemon
//! that are still alive get their process group terminated.

use loft_harness::process::{SIGKILL, SIGTERM, signal_group};
use loft_store::Store;
use std::process::Command;
use std::time::Duration;

pub(crate) fn reap_orphans(store: &Store) {
    let Ok(procs) = store.processes() else { return };
    for (pid, kind) in procs {
        if owned_by_live_daemon(pid) {
            // Another daemon's (a second home on a copy of this database,
            // say): its agents are alive and well, not ours to end.
            tracing::info!(pid, %kind, "recorded agent belongs to a live daemon; leaving it");
        } else if process_matches(pid, &kind) {
            tracing::warn!(pid, %kind, "reaping orphaned agent process group");
            signal_group(pid, SIGTERM);
            std::thread::sleep(Duration::from_millis(500));
            if process_matches(pid, &kind) {
                signal_group(pid, SIGKILL);
            }
        }
        let _ = store.forget_process(pid);
    }
}

/// Whether `pid`'s parent is a running agentloftd. A real orphan's daemon
/// is gone, so it hangs off launchd/init (or a subreaper) instead.
fn owned_by_live_daemon(pid: u32) -> bool {
    let field = |pid: u32, what: &str| {
        Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", what])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let Some(ppid) = field(pid, "ppid=").and_then(|p| p.parse::<u32>().ok()) else {
        return false;
    };
    ppid > 1
        && ppid != std::process::id()
        && field(ppid, "command=").is_some_and(|c| c.contains("agentloftd"))
}

/// Alive *and* still the same kind of program (guards against PID reuse).
fn process_matches(pid: u32, kind: &str) -> bool {
    let Ok(out) = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
    else {
        return false;
    };
    let cmd = String::from_utf8_lossy(&out.stdout);
    out.status.success() && !cmd.trim().is_empty() && cmd.contains(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt as _;

    #[test]
    fn reaps_recorded_orphans_but_not_reused_pids() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        // An "agent" left behind by a previous daemon, in its own group.
        let mut orphan = std::process::Command::new("sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .unwrap();
        store.record_process(orphan.id(), "sleep", None).unwrap();
        // A recorded pid now used by a different program must survive.
        let me = std::process::id();
        store.record_process(me, "claude", None).unwrap();

        reap_orphans(&store);

        let status = (0..50).find_map(|_| {
            let s = orphan.try_wait().unwrap();
            if s.is_none() {
                std::thread::sleep(Duration::from_millis(50));
            }
            s
        });
        assert!(status.is_some(), "orphan was reaped");
        assert!(process_matches(me, ""), "the reused pid (us) is alive");
        assert!(store.processes().unwrap().is_empty(), "records cleared");
    }

    #[test]
    fn leaves_a_live_daemons_agents_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        // A stand-in "agentloftd" (a shell under that name) with its agent.
        let mut daemon = std::process::Command::new("/bin/sh")
            .arg0("agentloftd")
            .args(["-c", "sleep 300 & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(daemon.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let agent: u32 = line.trim().parse().unwrap();
        store.record_process(agent, "sleep", None).unwrap();

        reap_orphans(&store);

        assert!(
            process_matches(agent, "sleep"),
            "the live daemon's agent survives"
        );
        let _ = Command::new("kill").arg(agent.to_string()).status();
        let _ = daemon.kill();
        let _ = daemon.wait();
    }
}
