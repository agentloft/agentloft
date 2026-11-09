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
