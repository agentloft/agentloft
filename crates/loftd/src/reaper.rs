//! Startup reaper (PLAN §5.4): agent processes recorded by a previous daemon
//! that are still alive get their process group terminated.

use loft_harness::process::{SIGKILL, SIGTERM, signal_group};
use loft_store::Store;
use std::process::Command;
use std::time::Duration;

pub(crate) fn reap_orphans(store: &Store) {
    let Ok(procs) = store.processes() else { return };
    for (pid, kind) in procs {
        if process_matches(pid, &kind) {
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
}
