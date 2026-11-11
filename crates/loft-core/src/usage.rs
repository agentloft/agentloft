//! What the agents have used, from every thread's turns: tokens and cost
//! in total, per day, per agent, model, project and thread (the Usage page).

use crate::{Core, CoreResult};
use loft_config::pricing::{Price, canonical, price};
use loft_protocol::{Response, Turn, Usage};
use serde_json::{Value, json};
use std::collections::HashMap;

const DAY_MS: i64 = 86_400_000;
/// Longer ranges are charted by the week.
const DAILY_UP_TO: i64 = 120;

/// Tokens, cost and turns, added up. Cost is the agent's own figure where
/// it gave one, else the tokens at the model's API price; the split by
/// token kind and the cache savings are always at API prices.
#[derive(Default, Clone)]
struct Sum {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    /// Turns whose cost the agent reported / that were priced from tokens /
    /// that couldn't be priced (no known model).
    reported: u32,
    estimated: u32,
    unpriced: u32,
    /// At API prices, by token kind: input, cache read, cache write, output.
    by_kind: [f64; 4],
    /// What reading from the cache saved over sending it as fresh input.
    savings: f64,
    turns: u32,
    duration_ms: u64,
}

impl Sum {
    fn add(&mut self, u: &Usage, price: Option<Price>) {
        self.input += u.input_tokens;
        self.output += u.output_tokens;
        self.cache_read += u.cache_read_tokens;
        self.cache_write += u.cache_write_tokens;
        let m = |tokens: u64, per_million: f64| tokens as f64 * per_million / 1e6;
        if let Some(p) = price {
            let kinds = [
                m(u.input_tokens, p.input),
                m(u.cache_read_tokens, p.cache_read),
                m(u.cache_write_tokens, p.cache_write),
                m(u.output_tokens, p.output),
            ];
            for (sum, k) in self.by_kind.iter_mut().zip(kinds) {
                *sum += k;
            }
            self.savings += m(u.cache_read_tokens, p.input - p.cache_read);
            if u.cost_usd.is_none() {
                self.cost += kinds.iter().sum::<f64>();
                self.estimated += 1;
            }
        }
        match u.cost_usd {
            Some(c) => {
                self.cost += c;
                self.reported += 1;
            }
            None if price.is_none() => self.unpriced += 1,
            None => {}
        }
        self.turns += 1;
        self.duration_ms += u.duration_ms.unwrap_or(0);
    }

    fn merge(&mut self, o: &Sum) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.cost += o.cost;
        self.reported += o.reported;
        self.estimated += o.estimated;
        self.unpriced += o.unpriced;
        for (a, b) in self.by_kind.iter_mut().zip(o.by_kind) {
            *a += b;
        }
        self.savings += o.savings;
        self.turns += o.turns;
        self.duration_ms += o.duration_ms;
    }

    fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    fn json(&self) -> Value {
        let cents = |x: f64| (x * 100.).round() / 100.;
        let priced = self.reported + self.estimated > 0;
        json!({
            "tokens": self.tokens(),
            "input": self.input,
            "output": self.output,
            "cache_read": self.cache_read,
            "cache_write": self.cache_write,
            "cost": priced.then(|| cents(self.cost)),
            "cost_reported": self.reported,
            "cost_estimated": self.estimated,
            "unpriced": self.unpriced,
            "cost_by_kind": {
                "input": cents(self.by_kind[0]),
                "cache_read": cents(self.by_kind[1]),
                "cache_write": cents(self.by_kind[2]),
                "output": cents(self.by_kind[3]),
            },
            "savings": cents(self.savings),
            "turns": self.turns,
            "duration_ms": self.duration_ms,
        })
    }
}

/// Groups by key, biggest first (by cost, then tokens), each with `extra`.
fn ranked(groups: HashMap<String, (Sum, Value)>, limit: usize) -> Vec<Value> {
    let mut rows: Vec<(String, (Sum, Value))> = groups.into_iter().collect();
    rows.sort_by(|a, b| {
        b.1.0
            .cost
            .total_cmp(&a.1.0.cost)
            .then(b.1.0.tokens().cmp(&a.1.0.tokens()))
    });
    rows.into_iter()
        .take(limit)
        .map(|(key, (sum, extra))| {
            let mut v = sum.json();
            v["key"] = json!(key);
            if let (Some(obj), Value::Object(more)) = (v.as_object_mut(), extra) {
                obj.extend(more);
            }
            v
        })
        .collect()
}

/// The report for `turns` (with usage), days local to `offset_min`: all
/// time in `total`; everything else over local days `from..=to` (`from` no
/// earlier than the first turn; unset: since the first turn, up to today).
/// Ranges longer than [`DAILY_UP_TO`] days get a bar a week.
fn report(
    turns: &[Turn],
    threads: &HashMap<String, loft_protocol::Thread>,
    projects: &HashMap<String, String>,
    now: i64,
    offset_min: i32,
    (from, to): (Option<i64>, Option<i64>),
    (prices, catalog): (
        &std::collections::BTreeMap<String, loft_config::ModelPrice>,
        Option<&loft_config::pricing::Catalog>,
    ),
) -> Value {
    use std::collections::HashSet;
    let offset = i64::from(offset_min) * 60_000;
    let day_of = |ms: i64| (ms + offset).div_euclid(DAY_MS);
    let today = day_of(now);
    let first_turn = turns
        .iter()
        .filter(|t| t.usage.is_some())
        .map(|t| day_of(t.started_at_ms))
        .min()
        .unwrap_or(today);
    let last = to.unwrap_or(today);
    let first = from.unwrap_or(first_turn).max(first_turn).min(last);
    let bucket = if last - first + 1 > DAILY_UP_TO { 7 } else { 1 };

    let mut total = Sum::default();
    let mut range = Sum::default();
    let mut range_threads = HashSet::new();
    let mut active_days = HashSet::new();
    // Bucket → agent → sum.
    let mut per_day: HashMap<i64, HashMap<String, Sum>> = HashMap::new();
    let mut agents: HashMap<String, (Sum, Value)> = HashMap::new();
    let mut agent_threads: HashMap<String, HashSet<&str>> = HashMap::new();
    let mut models: HashMap<String, (Sum, Value)> = HashMap::new();
    let mut by_project: HashMap<String, (Sum, Value)> = HashMap::new();
    let mut by_thread: HashMap<String, (Sum, Value)> = HashMap::new();
    let mut first_ms: Option<i64> = None;
    for turn in turns {
        let Some(u) = &turn.usage else { continue };
        let at = turn.started_at_ms;
        first_ms = Some(first_ms.map_or(at, |f| f.min(at)));
        let thread = threads.get(turn.thread_id.as_str());
        // The model that ran the turn; older turns: the thread's setting.
        let model = turn
            .model
            .clone()
            .or_else(|| thread.and_then(|t| t.model.clone()))
            .map(|m| canonical(&m));
        let context = turn.context.map(|c| c.tokens);
        let cost = model
            .as_deref()
            .and_then(|m| price(m, context, prices, catalog));
        total.add(u, cost);
        let day = day_of(at);
        if day < first || day > last {
            continue;
        }
        range.add(u, cost);
        range_threads.insert(turn.thread_id.as_str());
        active_days.insert(day);
        let agent = thread.map_or("unknown", |t| t.harness.as_str()).to_owned();
        per_day
            .entry((day - first) / bucket)
            .or_default()
            .entry(agent.clone())
            .or_default()
            .add(u, cost);
        agents
            .entry(agent.clone())
            .or_insert_with(|| (Sum::default(), json!({})))
            .0
            .add(u, cost);
        agent_threads
            .entry(agent.clone())
            .or_default()
            .insert(turn.thread_id.as_str());
        // `default`: the agent's own choice, never named.
        let model = model.unwrap_or_else(|| "default".into());
        models
            .entry(format!("{agent}/{model}"))
            .or_insert_with(|| (Sum::default(), json!({ "agent": agent, "model": model })))
            .0
            .add(u, cost);
        if let Some(t) = thread {
            let pid = t.project_id.as_str().to_owned();
            let name = projects.get(&pid).cloned().unwrap_or_default();
            by_project
                .entry(pid)
                .or_insert_with(|| (Sum::default(), json!({ "name": name })))
                .0
                .add(u, cost);
            by_thread
                .entry(t.id.as_str().to_owned())
                .or_insert_with(|| {
                    (
                        Sum::default(),
                        json!({ "title": t.title, "agent": t.harness, "project": name }),
                    )
                })
                .0
                .add(u, cost);
        }
    }
    for (agent, (_, extra)) in agents.iter_mut() {
        extra["threads"] = json!(agent_threads.get(agent).map_or(0, HashSet::len));
    }
    let days_json: Vec<Value> = (0..=(last - first) / bucket)
        .map(|b| {
            let split = per_day.remove(&b).unwrap_or_default();
            let mut sum = Sum::default();
            for s in split.values() {
                sum.merge(s);
            }
            let mut v = sum.json();
            // Days since the epoch, local; the client names the date.
            v["day"] = json!(first + b * bucket);
            v["span"] = json!(bucket);
            v["agents"] = split.into_iter().map(|(a, s)| (a, s.json())).collect();
            v
        })
        .collect();
    json!({
        "today": today,
        "from_day": first,
        "to_day": last,
        "first_ms": first_ms,
        "total": total.json(),
        "range": range.json(),
        "threads": range_threads.len(),
        "active_days": active_days.len(),
        "days": days_json,
        "agents": ranked(agents, 16),
        "models": ranked(models, 50),
        "projects": ranked(by_project, 50),
        "top_threads": ranked(by_thread, 50),
    })
}

impl Core {
    pub(crate) fn usage_report(
        &self,
        utc_offset_min: i32,
        from_day: Option<i64>,
        to_day: Option<i64>,
    ) -> CoreResult<Response> {
        let prices = self.config().pricing;
        let catalog = self.price_catalog();
        let (turns, threads, projects) =
            self.with_store(|s| Ok((s.turns_with_usage()?, s.threads()?, s.projects()?)))?;
        let threads = threads
            .into_iter()
            .map(|t| (t.id.as_str().to_owned(), t))
            .collect();
        let projects = projects
            .into_iter()
            .map(|p| (p.id.as_str().to_owned(), p.name))
            .collect();
        let value = report(
            &turns,
            &threads,
            &projects,
            loft_store::now_ms(),
            utc_offset_min,
            (from_day, to_day),
            (&prices, catalog.as_deref()),
        );
        Ok(Response::Json { value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::{HarnessId, ProjectId, ThreadId, TurnId, TurnStatus};

    fn turn(thread: &ThreadId, at: i64, tokens: u64, cost: Option<f64>) -> Turn {
        Turn {
            id: TurnId::new(),
            thread_id: thread.clone(),
            ordinal: 1,
            status: TurnStatus::Completed,
            started_at_ms: at,
            ended_at_ms: Some(at + 1),
            usage: Some(Usage {
                input_tokens: tokens,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: cost,
                duration_ms: Some(1000),
            }),
            error: None,
            checkpoint_before: None,
            checkpoint_after: None,
            native_turn: None,
            changes: None,
            context: None,
            model: None,
        }
    }

    #[test]
    fn adds_up_by_day_agent_and_thread() {
        let project = ProjectId::new();
        let mk = |harness: &str, title: &str| {
            let mut t: loft_protocol::Thread = serde_json::from_value(json!({
                "id": ThreadId::new(), "project_id": project, "title": title,
                "harness": harness, "cwd": "/", "model": null, "status": "idle",
                "created_at_ms": 0, "updated_at_ms": 0,
                "workspace": {"kind": "local"},
            }))
            .unwrap();
            t.harness = HarnessId(harness.into());
            t
        };
        let a = mk("claude", "A");
        let mut b = mk("codex", "B");
        // Codex reports no cost: priced from its model.
        b.model = Some("gpt-5.5".into());
        let none = std::collections::BTreeMap::new();
        let now = 100 * DAY_MS + 3_600_000;
        let turns = vec![
            turn(&a.id, now - 2 * DAY_MS, 100, Some(0.5)),
            turn(&a.id, now, 200, Some(0.25)),
            turn(&b.id, now, 1000, None),
            // Long before the window: in the total, not in the days.
            turn(&b.id, now - 90 * DAY_MS, 5, None),
        ];
        let threads = [&a, &b]
            .iter()
            .map(|t| (t.id.as_str().to_owned(), (*t).clone()))
            .collect();
        let projects = [(project.as_str().to_owned(), "proj".to_owned())].into();
        let r = report(
            &turns,
            &threads,
            &projects,
            now,
            0,
            (Some(100 - 6), None),
            (&none, None),
        );
        assert_eq!(r["total"]["turns"], 4);
        assert_eq!(r["total"]["tokens"], 100 + 200 + 1000 + 5 + 40);
        assert_eq!(r["total"]["cost"], 0.76);
        assert_eq!(r["range"]["turns"], 3);
        assert_eq!(r["days"][6]["agents"]["codex"]["tokens"], 1010);
        assert_eq!(r["agents"][0]["threads"], 1);
        assert_eq!(r["days"].as_array().unwrap().len(), 7);
        assert_eq!(r["days"][6]["tokens"], 200 + 1000 + 20);
        assert_eq!(r["days"][4]["tokens"], 110);
        // Cost first: Claude's thread leads though Codex used more tokens.
        assert_eq!(r["agents"][0]["key"], "claude");
        assert_eq!(r["top_threads"][0]["title"], "A");
        let codex = &r["top_threads"][1];
        assert_eq!(codex["cost_estimated"], 1);
        // 1,000 input at $5/M and 10 output at $30/M: half a cent.
        assert_eq!(codex["cost_by_kind"]["input"], 0.01);
        assert_eq!(r["range"]["cost_reported"], 2);
        assert_eq!(r["models"][1]["model"], "gpt-5.5");
        assert_eq!(r["projects"][0]["name"], "proj");
        assert_eq!(r["active_days"], 2);
        assert_eq!(r["threads"], 2);
        // All time starts at the first turn, a bar a day up to 120 days…
        let all = report(
            &turns,
            &threads,
            &projects,
            now,
            0,
            (None, None),
            (&none, None),
        );
        assert_eq!(all["from_day"], 10);
        assert_eq!(all["days"].as_array().unwrap().len(), 91);
        assert_eq!(all["agents"][1]["turns"], 2);
        // …and a bar a week past that.
        let mut longer = turns.clone();
        longer.push(turn(&b.id, now - 200 * DAY_MS, 5, None));
        let weekly = report(
            &longer,
            &threads,
            &projects,
            now,
            0,
            (Some(-5000), None),
            (&none, None),
        );
        assert_eq!(weekly["from_day"], -100);
        assert_eq!(weekly["days"][0]["span"], 7);
        assert_eq!(weekly["days"].as_array().unwrap().len(), 29);
    }
}
