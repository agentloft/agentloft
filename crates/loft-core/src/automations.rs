//! Automations: a prompt that starts a thread on a schedule.
//!
//! The daemon keeps them (the `automations` table, `Event::Automation*`)
//! and one task starts each run when it is due. A run is a new thread in the
//! automation's project with its prompt as the first message, labelled with
//! the automation (`Thread::automation_id`) and titled after it.
//!
//! Times are local to the daemon's machine. A run missed while the daemon
//! was down (the Mac asleep or shut down) happens once when it is back, not
//! once per missed occurrence; a run due while the last one is still going
//! is skipped.

use crate::{Core, CoreResult, internal};
use chrono::{
    DateTime, Datelike as _, Duration as Span, Local, NaiveDate, NaiveTime, TimeZone as _,
};
use loft_protocol::{
    Automation, AutomationId, AutomationSpec, ErrorBody, ErrorCode, Event, HarnessId, Project,
    Request, Response, Schedule, Thread, ThreadId, ThreadStatus, ThreadWorkspace,
};
use loft_store::now_ms;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The scheduler looks at the clock at least this often: a long sleep
/// doesn't follow the wall clock across the Mac's own sleep.
const MAX_WAIT: Duration = Duration::from_secs(30);
/// The shortest interval allowed.
const MIN_INTERVAL_MIN: u32 = 1;
/// The longest: a year.
const MAX_INTERVAL_MIN: u32 = 366 * 24 * 60;

fn bad(m: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, m)
}

fn not_found() -> ErrorBody {
    ErrorBody::new(ErrorCode::NotFound, "automation not found")
}

impl Core {
    /// Create (`id: None`) or change an automation.
    pub(crate) fn save_automation(
        &self,
        id: Option<AutomationId>,
        spec: AutomationSpec,
        from: Option<ThreadId>,
    ) -> CoreResult<Response> {
        let spec = self.checked_spec(spec)?;
        let now = now_ms();
        let automation = match id {
            Some(id) => {
                let mut a = self.load_automation(&id)?;
                a.spec = spec;
                a.updated_at_ms = now;
                a
            }
            None => Automation {
                id: AutomationId::new(),
                spec,
                created_at_ms: now,
                updated_at_ms: now,
                next_run_at_ms: None,
                last_run_at_ms: None,
                last_thread_id: None,
                last_error: None,
                created_by: from,
            },
        };
        let automation = self.reschedule(automation, now)?;
        self.commit(vec![Event::AutomationUpdated {
            automation: automation.clone(),
        }])?;
        self.automation_wake.notify_one();
        Ok(Response::Automation { automation })
    }

    pub(crate) fn set_automation_enabled(
        &self,
        id: &AutomationId,
        enabled: bool,
    ) -> CoreResult<Response> {
        let mut a = self.load_automation(id)?;
        a.spec.enabled = enabled;
        a.updated_at_ms = now_ms();
        let a = self.reschedule(a, now_ms())?;
        self.commit(vec![Event::AutomationUpdated {
            automation: a.clone(),
        }])?;
        self.automation_wake.notify_one();
        Ok(Response::Automation { automation: a })
    }

    pub(crate) fn delete_automation(&self, id: &AutomationId) -> CoreResult<Response> {
        self.load_automation(id)?;
        self.commit(vec![Event::AutomationDeleted {
            automation_id: id.clone(),
        }])?;
        Ok(Response::Ok)
    }

    /// Run it now, whatever its schedule (its next run stays as it was).
    pub(crate) async fn run_automation_now(
        self: &Arc<Self>,
        id: &AutomationId,
    ) -> CoreResult<Response> {
        let a = self.load_automation(id)?;
        let thread = self.start_run(&a).await?;
        let mut a = self.load_automation(id)?;
        a.last_run_at_ms = Some(thread.created_at_ms);
        a.last_thread_id = Some(thread.id.clone());
        a.last_error = None;
        self.commit(vec![Event::AutomationUpdated { automation: a }])?;
        Ok(Response::Thread { thread })
    }

    /// The automations as agents see them: each with its schedule in
    /// words and its times in local time.
    pub(crate) fn automations_json(&self) -> CoreResult<serde_json::Value> {
        let (list, projects) = self.with_store(|s| Ok((s.automations()?, s.projects()?)))?;
        Ok(serde_json::Value::Array(
            list.iter()
                .map(|a| {
                    let mut v = serde_json::to_value(a).unwrap_or_default();
                    v["schedule_text"] = describe(&a.spec.schedule).into();
                    v["project"] = projects
                        .iter()
                        .find(|p| p.id == a.spec.project_id)
                        .map(|p| p.name.clone())
                        .into();
                    v["next_run"] = a.next_run_at_ms.map(local_text).into();
                    v["last_run"] = a.last_run_at_ms.map(local_text).into();
                    v
                })
                .collect(),
        ))
    }

    /// Start the scheduler. It sleeps until the next run is due (or a
    /// change wakes it) and starts what is due.
    pub fn spawn_automations(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let core = Arc::downgrade(self);
        let wake = self.automation_wake.clone();
        tokio::spawn(async move {
            loop {
                let Some(c) = core.upgrade() else { return };
                let next = c.run_due().await;
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

    /// Start every run that is due; returns when the next one is.
    pub(crate) async fn run_due(self: &Arc<Self>) -> Option<i64> {
        let now = now_ms();
        let (list, threads) = match self.with_store(|s| Ok((s.automations()?, s.threads()?))) {
            Ok(x) => x,
            Err(err) => {
                tracing::warn!(err = %err.message, "automations: read failed");
                return None;
            }
        };
        for a in list.iter().filter(|a| a.spec.enabled) {
            if a.next_run_at_ms.is_none_or(|at| at > now) {
                continue;
            }
            let mut next = a.clone();
            next.next_run_at_ms = next_run(&a.spec.schedule, now, Some(now));
            if matches!(a.spec.schedule, Schedule::Once { .. }) {
                next.spec.enabled = false;
                next.next_run_at_ms = None;
            }
            // Its last run still going: this one is skipped.
            let busy = a.last_thread_id.as_ref().is_some_and(|id| {
                threads.iter().any(|t| {
                    &t.id == id
                        && (matches!(
                            t.status,
                            ThreadStatus::Running | ThreadStatus::WaitingApproval
                        ) || t.is_waiting())
                })
            });
            if busy {
                next.last_error = Some("Skipped a run: the last one was still going".into());
                let _ = self.commit(vec![Event::AutomationUpdated { automation: next }]);
                continue;
            }
            // Moved on before the run starts, so it can't start twice.
            next.last_run_at_ms = Some(now);
            next.last_error = None;
            if let Err(err) = self.commit(vec![Event::AutomationUpdated {
                automation: next.clone(),
            }]) {
                tracing::warn!(err = %err.message, "automations: save failed");
                continue;
            }
            tracing::info!(id = %a.id, name = %a.spec.name, "automation run");
            let core = self.clone();
            tokio::spawn(async move {
                let started = core.start_run(&next).await;
                let Ok(mut a) = core.load_automation(&next.id) else {
                    return;
                };
                match started {
                    Ok(thread) => {
                        a.last_thread_id = Some(thread.id);
                        a.last_error = None;
                    }
                    Err(err) => {
                        tracing::warn!(id = %a.id, err = %err.message, "automation run failed");
                        a.last_error = Some(err.message);
                    }
                }
                let _ = core.commit(vec![Event::AutomationUpdated { automation: a }]);
            });
        }
        self.with_store(|s| s.automations())
            .ok()?
            .iter()
            .filter(|a| a.spec.enabled)
            .filter_map(|a| a.next_run_at_ms)
            .min()
    }

    /// A run: a new thread in the automation's project, sent its prompt.
    async fn start_run(self: &Arc<Self>, a: &Automation) -> CoreResult<Thread> {
        let project = self
            .with_store(|s| s.project(&a.spec.project_id))?
            .ok_or_else(|| bad("its project is gone"))?;
        let harness = self.run_harness(&a.spec, &project)?;
        let (model, mode, effort) = crate::thread_defaults(&self.config(), &harness);
        let own = project.defaults.agent.as_ref() == Some(&harness);
        let id = ThreadId::new();
        let (cwd, workspace) = if self.is_chats(&project) {
            // A chat: a folder of its own.
            let own = Path::new(&project.root_path).join(id.as_str());
            std::fs::create_dir_all(&own).map_err(internal)?;
            (own.to_string_lossy().into_owned(), ThreadWorkspace::Local)
        } else if a.spec.worktree && loft_git::repo_root(Path::new(&project.root_path)).is_some() {
            (
                project.root_path.clone(),
                ThreadWorkspace::NewWorktree { base: None },
            )
        } else {
            (project.root_path.clone(), ThreadWorkspace::Local)
        };
        let now = now_ms();
        let thread = Thread {
            id: id.clone(),
            project_id: project.id.clone(),
            title: a.spec.name.clone(),
            harness,
            cwd,
            model: a
                .spec
                .model
                .clone()
                .or_else(|| project.defaults.model.clone().filter(|_| own))
                .or(model),
            permission_mode: a.spec.permission_mode.or(mode),
            effort: a
                .spec
                .effort
                .clone()
                .or_else(|| project.defaults.effort.clone().filter(|_| own))
                .or(effort),
            native_session_id: None,
            status: ThreadStatus::Idle,
            created_at_ms: now,
            updated_at_ms: now,
            workspace,
            parent: None,
            slash_commands: Vec::new(),
            command_info: Vec::new(),
            started_at_ms: None,
            draft: None,
            waiting_on: Vec::new(),
            automation_id: Some(a.id.clone()),
        };
        self.commit(vec![Event::ThreadCreated {
            thread: thread.clone(),
        }])?;
        // The worktree's branch is named after the automation, not its
        // prompt; then the message starts the turn.
        let sent = async {
            self.ensure_workspace(&id, &a.spec.name).await?;
            Box::pin(self.handle(Request::SendMessage {
                thread_id: id.clone(),
                text: a.spec.prompt.clone(),
                attachments: Vec::new(),
            }))
            .await
        }
        .await;
        if let Err(err) = sent {
            // Nothing happened in it: don't leave an empty thread behind.
            let _ = self.delete_thread(&id).await;
            return Err(err);
        }
        Ok(self.with_store(|s| s.thread(&id))?.unwrap_or(thread))
    }

    /// The agent a run uses: its own, the project's, Claude Code, or any.
    fn run_harness(&self, spec: &AutomationSpec, project: &Project) -> CoreResult<HarnessId> {
        if let Some(h) = &spec.harness {
            return if self.harness(h).is_some() {
                Ok(h.clone())
            } else {
                Err(bad(format!("unknown agent `{h}`")))
            };
        }
        [project.defaults.agent.clone(), Some(HarnessId::claude())]
            .into_iter()
            .flatten()
            .find(|h| self.harness(h).is_some())
            .or_else(|| {
                let mut ids: Vec<&String> = self.harnesses.keys().collect();
                ids.sort();
                ids.first().map(|id| HarnessId((*id).clone()))
            })
            .ok_or_else(|| bad("no agent is set up"))
    }

    fn is_chats(&self, project: &Project) -> bool {
        std::fs::canonicalize(&self.opts().chats_dir)
            .is_ok_and(|c| Path::new(&project.root_path) == c)
    }

    fn load_automation(&self, id: &AutomationId) -> CoreResult<Automation> {
        self.with_store(|s| s.automation(id))?.ok_or_else(not_found)
    }

    /// A spec as given, checked and tidied.
    fn checked_spec(&self, mut spec: AutomationSpec) -> CoreResult<AutomationSpec> {
        spec.name = spec.name.trim().to_owned();
        spec.prompt = spec.prompt.trim().to_owned();
        if spec.name.is_empty() {
            return Err(bad("the automation needs a name"));
        }
        if spec.prompt.is_empty() {
            return Err(bad("the automation needs a prompt"));
        }
        if self.with_store(|s| s.project(&spec.project_id))?.is_none() {
            return Err(bad("no such project"));
        }
        if let Some(h) = &spec.harness
            && self.harness(h).is_none()
        {
            return Err(bad(format!("unknown agent `{h}`")));
        }
        let blank = |v: Option<String>| v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        spec.model = blank(spec.model);
        spec.effort = blank(spec.effort);
        spec.schedule = checked_schedule(spec.schedule)?;
        Ok(spec)
    }

    /// Its next run, as its schedule and on/off now say.
    fn reschedule(&self, mut a: Automation, now: i64) -> CoreResult<Automation> {
        a.next_run_at_ms = if a.spec.enabled {
            match &a.spec.schedule {
                // Counted from the last run, if that leaves one ahead.
                Schedule::Interval { minutes } => Some(
                    a.last_run_at_ms
                        .map(|l| l + i64::from(*minutes) * 60_000)
                        .filter(|n| *n > now)
                        .unwrap_or(now + i64::from(*minutes) * 60_000),
                ),
                Schedule::Once { at_ms } if *at_ms <= now => {
                    return Err(bad("that time has passed"));
                }
                s => next_run(s, now, a.last_run_at_ms),
            }
        } else {
            None
        };
        Ok(a)
    }
}

/// A schedule checked, with its parts in a standard form.
fn checked_schedule(schedule: Schedule) -> CoreResult<Schedule> {
    Ok(match schedule {
        Schedule::Interval { minutes } => {
            if !(MIN_INTERVAL_MIN..=MAX_INTERVAL_MIN).contains(&minutes) {
                return Err(bad(format!(
                    "an interval is {MIN_INTERVAL_MIN} minute to {MAX_INTERVAL_MIN} minutes"
                )));
            }
            Schedule::Interval { minutes }
        }
        Schedule::Daily { time, mut days } => {
            let t = parse_time(&time)
                .ok_or_else(|| bad(format!("`{time}` isn't a time (HH:MM, 24-hour)")))?;
            if days.iter().any(|d| *d > 6) {
                return Err(bad("days are 0 (Sunday) to 6 (Saturday)"));
            }
            days.sort_unstable();
            days.dedup();
            if days.len() == 7 {
                days.clear();
            }
            Schedule::Daily {
                time: t.format("%H:%M").to_string(),
                days,
            }
        }
        Schedule::Cron { expr } => {
            let expr = expr.split_whitespace().collect::<Vec<_>>().join(" ");
            Cron::parse(&expr).map_err(|e| bad(format!("cron `{expr}`: {e}")))?;
            Schedule::Cron { expr }
        }
        Schedule::Once { at_ms } => Schedule::Once { at_ms },
    })
}

/// `H:MM` or `HH:MM`, 24-hour.
fn parse_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

/// When `schedule` next runs after `now` (all in ms). `last`: its last run.
pub(crate) fn next_run(schedule: &Schedule, now: i64, last: Option<i64>) -> Option<i64> {
    let after = Local.timestamp_millis_opt(now).single()?;
    match schedule {
        Schedule::Interval { minutes } => Some(now + i64::from(*minutes) * 60_000),
        Schedule::Once { at_ms } => {
            (*at_ms > now && last.is_none_or(|l| l < *at_ms)).then_some(*at_ms)
        }
        Schedule::Daily { time, days } => {
            let t = parse_time(time)?;
            (0..=8).find_map(|d| {
                let day = after.date_naive() + Span::days(d);
                let weekday = day.weekday().num_days_from_sunday() as u8;
                if !days.is_empty() && !days.contains(&weekday) {
                    return None;
                }
                let at = local_at(day, t)?;
                (at > after).then(|| at.timestamp_millis())
            })
        }
        Schedule::Cron { expr } => Cron::parse(expr).ok()?.next_after(after),
    }
}

/// A local date and time as an instant: the earlier one where the clocks go
/// back, none where they skip it.
fn local_at(day: NaiveDate, t: NaiveTime) -> Option<DateTime<Local>> {
    Local.from_local_datetime(&day.and_time(t)).earliest()
}

/// A time in ms as local time, for agents ("2026-11-12 09:00 (Thursday)").
pub fn local_text(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M (%A)").to_string(),
        None => ms.to_string(),
    }
}

/// A local date and time (`2026-11-12 09:00`, `2026-11-12T09:00`) in ms.
pub fn parse_local(s: &str) -> Option<i64> {
    let s = s.trim().replace('T', " ");
    ["%Y-%m-%d %H:%M", "%Y-%m-%d %H:%M:%S"]
        .iter()
        .find_map(|f| chrono::NaiveDateTime::parse_from_str(&s, f).ok())
        .and_then(|dt| Local.from_local_datetime(&dt).earliest())
        .map(|t| t.timestamp_millis())
}

/// A schedule in words, with a one-off's date.
pub fn describe(schedule: &Schedule) -> String {
    match schedule {
        Schedule::Once { at_ms } => format!("Once, at {}", local_text(*at_ms)),
        s => s.describe(),
    }
}

/// A five-field cron expression (minute hour day-of-month month
/// day-of-week), with `*`, lists, ranges, steps, month and day names, and
/// the `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly` shorthands.
#[derive(Debug)]
pub(crate) struct Cron {
    minutes: u64,
    hours: u32,
    /// Bits 1–31.
    month_days: u32,
    /// Bits 1–12.
    months: u16,
    /// Bits 0–6 (Sunday = 0).
    week_days: u8,
    /// Day-of-month or day-of-week was `*`: with both restricted, a day
    /// matching either runs (as cron does).
    any_month_day: bool,
    any_week_day: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

impl Cron {
    pub(crate) fn parse(expr: &str) -> Result<Self, String> {
        let expr = match expr.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            e => e,
        };
        let fields: Vec<&str> = expr.split_whitespace().collect();
        let [min, hour, dom, mon, dow] = fields.as_slice() else {
            return Err(format!(
                "five fields (minute hour day month weekday), not {}",
                fields.len()
            ));
        };
        let week_days = field(dow, 0, 7, &DAYS, 0)?;
        // 7 is Sunday too.
        let week_days = ((week_days | (week_days >> 7)) & 0x7f) as u8;
        Ok(Self {
            minutes: field(min, 0, 59, &[], 0)?,
            hours: field(hour, 0, 23, &[], 0)? as u32,
            month_days: field(dom, 1, 31, &[], 0)? as u32,
            months: field(mon, 1, 12, &MONTHS, 1)? as u16,
            week_days,
            any_month_day: dom.starts_with('*'),
            any_week_day: dow.starts_with('*'),
        })
    }

    fn day_matches(&self, day: NaiveDate) -> bool {
        if self.months & (1 << day.month()) == 0 {
            return false;
        }
        let dom = self.month_days & (1 << day.day()) != 0;
        let dow = self.week_days & (1 << day.weekday().num_days_from_sunday()) != 0;
        match (self.any_month_day, self.any_week_day) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }

    /// The first matching minute after `after`, within about five years.
    pub(crate) fn next_after(&self, after: DateTime<Local>) -> Option<i64> {
        let start = after.naive_local();
        let mut day = start.date();
        for _ in 0..(366 * 5) {
            if self.day_matches(day) {
                for hour in 0..24u32 {
                    if self.hours & (1 << hour) == 0 {
                        continue;
                    }
                    for minute in 0..60u32 {
                        if self.minutes & (1 << minute) == 0 {
                            continue;
                        }
                        let t = NaiveTime::from_hms_opt(hour, minute, 0)?;
                        if day.and_time(t) <= start {
                            continue;
                        }
                        if let Some(at) = local_at(day, t)
                            && at > after
                        {
                            return Some(at.timestamp_millis());
                        }
                    }
                }
            }
            day = day.succ_opt()?;
        }
        None
    }
}

/// One cron field as a bit set over `lo..=hi`. `names`: what `offset`,
/// `offset + 1`, … may also be called.
fn field(s: &str, lo: u32, hi: u32, names: &[&str], offset: u32) -> Result<u64, String> {
    let value = |v: &str| -> Result<u32, String> {
        let lower = v.to_ascii_lowercase();
        if let Some(i) = names.iter().position(|n| *n == lower) {
            return Ok(i as u32 + offset);
        }
        let n: u32 = v.parse().map_err(|_| format!("`{v}` isn't a number"))?;
        if n < lo || n > hi {
            return Err(format!("{n} is outside {lo}–{hi}"));
        }
        Ok(n)
    };
    let mut bits = 0u64;
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => {
                let st: u32 = st.parse().map_err(|_| format!("`{st}` isn't a step"))?;
                if st == 0 {
                    return Err("a step of 0".into());
                }
                (r, st)
            }
            None => (part, 1),
        };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let a = value(range)?;
            // `5/15`: from 5 to the end, every 15.
            (a, if part.contains('/') { hi } else { a })
        };
        if a > b {
            return Err(format!("`{range}` runs backwards"));
        }
        let mut n = a;
        while n <= b {
            bits |= 1 << n;
            n += step;
        }
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> i64 {
        parse_local(s).unwrap()
    }

    fn text(ms: Option<i64>) -> String {
        Local
            .timestamp_millis_opt(ms.unwrap())
            .unwrap()
            .format("%Y-%m-%d %H:%M %a")
            .to_string()
    }

    #[test]
    fn cron_fields() {
        let c = Cron::parse("*/15 9-17 * * mon-fri").unwrap();
        assert_eq!(c.minutes, 1 | 1 << 15 | 1 << 30 | 1 << 45);
        assert_eq!(c.week_days, 0b0111110);
        assert!(Cron::parse("0 9 * *").is_err());
        assert!(Cron::parse("61 * * * *").is_err());
        assert!(Cron::parse("0 0 * * 7").unwrap().week_days == 1);
        assert!(Cron::parse("@daily").is_ok());
    }

    #[test]
    fn next_runs() {
        // 2026-11-11 is a Wednesday.
        let now = at("2026-11-11 10:30");
        let cron = |e: &str| text(next_run(&Schedule::Cron { expr: e.into() }, now, None));
        assert_eq!(cron("0 9 * * *"), "2026-11-12 09:00 Thu");
        assert_eq!(cron("45 10 * * *"), "2026-11-11 10:45 Wed");
        assert_eq!(cron("0 9 * * 1"), "2026-11-16 09:00 Mon");
        assert_eq!(cron("0 0 1 * *"), "2026-12-01 00:00 Tue");
        // Both day fields restricted: either matches.
        assert_eq!(cron("0 8 13 * 5"), "2026-11-13 08:00 Fri");

        let daily = |time: &str, days: &[u8]| {
            text(next_run(
                &Schedule::Daily {
                    time: time.into(),
                    days: days.to_vec(),
                },
                now,
                None,
            ))
        };
        assert_eq!(daily("09:00", &[]), "2026-11-12 09:00 Thu");
        assert_eq!(daily("11:00", &[]), "2026-11-11 11:00 Wed");
        assert_eq!(daily("09:00", &[1, 2, 3, 4, 5]), "2026-11-12 09:00 Thu");
        assert_eq!(daily("09:00", &[6]), "2026-11-14 09:00 Sat");
        // Today's day, but already past: next week.
        assert_eq!(daily("10:00", &[3]), "2026-11-18 10:00 Wed");

        assert_eq!(
            next_run(&Schedule::Interval { minutes: 30 }, now, None),
            Some(now + 30 * 60_000)
        );
        let once = Schedule::Once { at_ms: now + 1000 };
        assert_eq!(next_run(&once, now, None), Some(now + 1000));
        assert_eq!(next_run(&once, now + 2000, Some(now + 1000)), None);
    }

    #[test]
    fn schedules_are_checked() {
        let daily = |t: &str, d: &[u8]| {
            checked_schedule(Schedule::Daily {
                time: t.into(),
                days: d.to_vec(),
            })
        };
        assert_eq!(
            daily("9:05", &[3, 1, 1]).unwrap(),
            Schedule::Daily {
                time: "09:05".into(),
                days: vec![1, 3]
            }
        );
        assert_eq!(
            daily("09:00", &[0, 1, 2, 3, 4, 5, 6]).unwrap(),
            Schedule::Daily {
                time: "09:00".into(),
                days: vec![]
            }
        );
        assert!(daily("25:00", &[]).is_err());
        assert!(daily("09:00", &[7]).is_err());
        assert!(checked_schedule(Schedule::Interval { minutes: 0 }).is_err());
        assert!(checked_schedule(Schedule::Cron { expr: "x".into() }).is_err());
    }
}
