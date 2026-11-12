//! Automations: a prompt that starts a thread on a schedule.

use crate::ids::*;
use crate::model::PermissionMode;
use serde::{Deserialize, Serialize};

/// What an automation does and when: everything the user (or an agent)
/// sets. Each run starts a new thread in `project_id` with `prompt` as its
/// first message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutomationSpec {
    pub name: String,
    pub prompt: String,
    pub project_id: ProjectId,
    pub schedule: Schedule,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The agent each run uses (`None`: the project's, else Claude Code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// `None`: the agent's default. Nobody watches a run as it starts, so
    /// a mode that asks first leaves it waiting for an answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// Each run in a new worktree of its own (else the project checkout).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worktree: bool,
}

fn yes() -> bool {
    true
}

/// An automation as the daemon keeps it: its spec and how its runs went.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Automation {
    pub id: AutomationId,
    #[serde(flatten)]
    pub spec: AutomationSpec,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// When it runs next (`None`: paused, or a one-off that has run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at_ms: Option<i64>,
    /// The thread its last run started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_thread_id: Option<ThreadId>,
    /// Why its last run didn't start (or was skipped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The thread whose agent made it, if an agent did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<ThreadId>,
}

/// When an automation runs. Times are the daemon machine's local time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Every `minutes` minutes, counted from the last run.
    Interval { minutes: u32 },
    /// At `time` (`HH:MM`) on `days` (0 = Sunday … 6 = Saturday; none =
    /// every day).
    Daily {
        time: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        days: Vec<u8>,
    },
    /// A five-field cron expression: minute hour day-of-month month
    /// day-of-week.
    Cron { expr: String },
    /// Once, at `at_ms`; then it pauses itself.
    Once { at_ms: i64 },
}

const DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// A weekday's name from its number (0 = Sunday).
pub fn day_name(day: u8) -> &'static str {
    DAY_NAMES[day as usize % 7]
}

impl Schedule {
    /// In words, for lists and agents: "Every 30 minutes", "Weekdays at
    /// 09:00", "Mondays and Thursdays at 18:30", "Cron 0 9 * * 1".
    /// (`Once` leaves out its date, which needs the local time zone.)
    pub fn describe(&self) -> String {
        match self {
            Schedule::Interval { minutes } => match *minutes {
                1 => "Every minute".into(),
                60 => "Every hour".into(),
                1440 => "Every day".into(),
                m if m % 1440 == 0 => format!("Every {} days", m / 1440),
                m if m % 60 == 0 => format!("Every {} hours", m / 60),
                m => format!("Every {m} minutes"),
            },
            Schedule::Daily { time, days } => {
                let mut days: Vec<u8> = days.iter().map(|d| d % 7).collect();
                days.sort_unstable();
                days.dedup();
                let when = match days.as_slice() {
                    [] | [0, 1, 2, 3, 4, 5, 6] => "Every day".to_owned(),
                    [1, 2, 3, 4, 5] => "Weekdays".to_owned(),
                    [0, 6] => "Weekends".to_owned(),
                    [one] => format!("{}s", day_name(*one)),
                    many => {
                        let names: Vec<String> =
                            many.iter().map(|d| format!("{}s", day_name(*d))).collect();
                        let (last, rest) = names.split_last().expect("two or more");
                        format!("{} and {last}", rest.join(", "))
                    }
                };
                format!("{when} at {time}")
            }
            Schedule::Cron { expr } => format!("Cron {expr}"),
            Schedule::Once { .. } => "Once".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_schedules() {
        let daily = |days: &[u8]| Schedule::Daily {
            time: "09:00".into(),
            days: days.to_vec(),
        };
        assert_eq!(daily(&[]).describe(), "Every day at 09:00");
        assert_eq!(daily(&[5, 4, 3, 2, 1]).describe(), "Weekdays at 09:00");
        assert_eq!(daily(&[1]).describe(), "Mondays at 09:00");
        assert_eq!(
            daily(&[1, 4, 5]).describe(),
            "Mondays, Thursdays and Fridays at 09:00"
        );
        assert_eq!(
            Schedule::Interval { minutes: 120 }.describe(),
            "Every 2 hours"
        );
        assert_eq!(
            Schedule::Interval { minutes: 45 }.describe(),
            "Every 45 minutes"
        );
    }

    #[test]
    fn automation_json_is_flat() {
        let a = Automation {
            id: AutomationId("a".into()),
            spec: AutomationSpec {
                name: "n".into(),
                prompt: "p".into(),
                project_id: ProjectId("p".into()),
                schedule: Schedule::Interval { minutes: 5 },
                enabled: true,
                harness: None,
                model: None,
                effort: None,
                permission_mode: None,
                worktree: false,
            },
            created_at_ms: 1,
            updated_at_ms: 1,
            next_run_at_ms: Some(2),
            last_run_at_ms: None,
            last_thread_id: None,
            last_error: None,
            created_by: None,
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["schedule"]["kind"], "interval");
        assert_eq!(v["name"], "n");
        assert_eq!(serde_json::from_value::<Automation>(v).unwrap(), a);
    }
}
