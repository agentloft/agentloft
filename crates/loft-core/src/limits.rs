//! Threads their agent's usage limit stopped (`Thread::limit`) carry on
//! when the limit resets, if they were told to: each on its own
//! (`Request::ResumeAtReset`) or all of them (`threads.resume_at_reset`).
//!
//! What is waiting is on the threads themselves, so it survives a daemon
//! restart; a reset that passed while the daemon was down (the Mac asleep
//! or shut down) is acted on when it is back.

use crate::thread::ThreadCmd;
use crate::{Core, lock};
use loft_protocol::{LimitStop, ThreadId, ThreadStatus};
use loft_store::now_ms;
use std::sync::Arc;
use std::time::Duration;

/// The scheduler looks at the clock at least this often: a long sleep
/// doesn't follow the wall clock across the Mac's own sleep.
const MAX_WAIT: Duration = Duration::from_secs(30);
/// How long after the stated reset a thread carries on: clocks differ, and
/// a turn sent a moment early is refused again.
const AFTER_RESET_MS: i64 = 60_000;

impl Core {
    /// A thread's limit stop changed (set, told to carry on or not, or
    /// gone): keep what the scheduler waits for in step.
    pub(crate) fn limit_changed(&self, thread_id: &ThreadId, limit: Option<LimitStop>) {
        let at = limit.filter(|l| l.resume).and_then(|l| l.resets_at_ms);
        let mut resumes = lock(&self.limit_resumes);
        let changed = match at {
            Some(at) => resumes.insert(thread_id.clone(), at) != Some(at),
            None => resumes.remove(thread_id).is_some(),
        };
        drop(resumes);
        if changed {
            self.limit_wake.notify_one();
        }
    }

    /// Start the scheduler: it sleeps until the next reset (or a change
    /// wakes it) and tells the threads whose limit has reset to carry on.
    pub fn spawn_limit_resumes(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        // What was waiting when the daemon last stopped.
        match self.with_store(|s| s.threads()) {
            Ok(threads) => {
                let mut resumes = lock(&self.limit_resumes);
                for t in threads {
                    if t.status != ThreadStatus::Archived
                        && let Some(at) = t.limit.filter(|l| l.resume).and_then(|l| l.resets_at_ms)
                    {
                        resumes.insert(t.id, at);
                    }
                }
            }
            Err(err) => tracing::warn!(err = %err.message, "limit resumes: read failed"),
        }
        let core = Arc::downgrade(self);
        let wake = self.limit_wake.clone();
        tokio::spawn(async move {
            loop {
                let Some(c) = core.upgrade() else { return };
                let next = c.resume_due(now_ms());
                drop(c);
                let wait = next
                    .map(|at| Duration::from_millis(at.saturating_sub(now_ms()).max(0) as u64))
                    .unwrap_or(MAX_WAIT)
                    .min(MAX_WAIT);
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = wake.notified() => {}
                }
            }
        })
    }

    /// Tell every thread whose limit has reset to carry on; returns when
    /// the next one is due.
    pub(crate) fn resume_due(self: &Arc<Self>, now: i64) -> Option<i64> {
        let due: Vec<ThreadId> = {
            let mut resumes = lock(&self.limit_resumes);
            let due: Vec<ThreadId> = resumes
                .iter()
                .filter(|(_, at)| **at + AFTER_RESET_MS <= now)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &due {
                resumes.remove(id);
            }
            due
        };
        for id in due {
            // Archived or deleted since: nothing carries on.
            let live = self
                .with_store(|s| s.thread(&id))
                .ok()
                .flatten()
                .is_some_and(|t| t.status != ThreadStatus::Archived);
            if !live {
                continue;
            }
            tracing::info!(thread = %id, "usage limit reset: carrying on");
            match self.thread_handle(&id) {
                Ok(handle) => {
                    let _ = handle.send(ThreadCmd::LimitReset);
                }
                Err(err) => tracing::warn!(thread = %id, err = %err.message, "limit resume"),
            }
        }
        lock(&self.limit_resumes)
            .values()
            .map(|at| at + AFTER_RESET_MS)
            .min()
    }
}
