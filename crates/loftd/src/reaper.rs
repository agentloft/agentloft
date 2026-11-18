//! Startup reaper (PLAN §5.4): agent processes recorded by a previous daemon
//! that are still alive get their process group terminated.

use loft_harness::process::{SIGKILL, SIGTERM, signal_group};
use loft_store::Store;
use std::process::Command;
use std::time::Duration;

pub(crate) fn reap_orphans(store: &Store) {
    let Ok(procs) = store.processes() else { return };
    for (pid, kind, recorded_ms) in procs {
        if started_after(pid, recorded_ms) {
            // Not the process recorded (it was running when recorded): the
            // pid went to another program since, maybe the user's own
            // `claude` in a terminal.
            tracing::info!(pid, %kind, "recorded pid is another process now; leaving it");
        } else if owned_by_live_daemon(pid) {
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

/// Whether `pid`'s process started after `recorded_ms` (with a few
/// seconds' slack for `ps`'s whole seconds): then it isn't the one that
/// was recorded, which was already running.
fn started_after(pid: u32, recorded_ms: i64) -> bool {
    let Some(elapsed) = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "etime="])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| elapsed_secs(String::from_utf8_lossy(&o.stdout).trim()))
    else {
        return false;
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let started_ms = now_ms - elapsed * 1000;
    started_ms > recorded_ms + 5_000
}

/// `ps`'s elapsed time, `[[dd-]hh:]mm:ss`, in seconds.
fn elapsed_secs(text: &str) -> Option<i64> {
    let (days, rest) = match text.split_once('-') {
        Some((d, rest)) => (d.parse::<i64>().ok()?, rest),
        None => (0, text),
    };
    let mut secs = 0;
    for part in rest.split(':') {
        secs = secs * 60 + part.parse::<i64>().ok()?;
    }
    Some(days * 86_400 + secs)
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

// `ps`, `sleep` and process groups.
#[cfg(all(test, unix))]
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
        // So must one of the same kind started after the record (the
        // user's own agent, which got the pid of one that is gone).
        let mut reused = std::process::Command::new("sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .unwrap();
        let an_hour_ago = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
            - 3_600_000;
        store
            .record_process_at(reused.id(), "sleep", an_hour_ago)
            .unwrap();

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
        assert!(
            reused.try_wait().unwrap().is_none(),
            "a later process of the same kind is alive"
        );
        let _ = reused.kill();
        assert_eq!(elapsed_secs("01:02"), Some(62));
        assert_eq!(
            elapsed_secs("2-03:04:05"),
            Some(2 * 86_400 + 3 * 3600 + 4 * 60 + 5)
        );
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
