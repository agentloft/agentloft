//! Views to try the renderer with: the test agent shows them (`ui:tests`,
//! `ui:all`), and the tests check that every one of them reads cleanly.

use serde_json::{Value, json};

/// Each sample's name and what it shows off.
pub const NAMES: &[(&str, &str)] = &[
    ("tests", "A test run: stats, a bar chart, a table, replies"),
    ("pricing", "A calculator: sliders drive stats and a chart"),
    (
        "deploy",
        "A form: inputs, a summary, a reply built from them",
    ),
    ("approach", "A choice between options, each with its reply"),
    ("release", "Steps, progress, tabs, details, code"),
    ("sizes", "Lines, areas and a donut"),
];

pub fn get(name: &str) -> Option<Value> {
    Some(match name {
        "tests" => tests(),
        "pricing" => pricing(),
        "deploy" => deploy(),
        "approach" => approach(),
        "release" => release(),
        "sizes" => sizes(),
        _ => return None,
    })
}

pub fn all() -> Vec<Value> {
    NAMES.iter().filter_map(|(name, _)| get(name)).collect()
}

fn tests() -> Value {
    json!({
        "title": "Test run · cargo test --workspace",
        "body": [
            { "type": "row", "children": [
                { "type": "stat", "label": "Passed", "value": "412", "tone": "success", "delta": "+9", "caption": "since main" },
                { "type": "stat", "label": "Failed", "value": "3", "tone": "danger", "delta": "+3", "delta_tone": "danger", "caption": "all in loft-core" },
                { "type": "stat", "label": "Skipped", "value": "7", "caption": "live agent tests" },
                { "type": "stat", "label": "Duration", "value": "{{ duration(48200) }}", "delta": "-12%", "delta_tone": "success", "caption": "vs last run" }
            ]},
            { "type": "heading", "text": "Slowest suites", "level": 3 },
            { "type": "chart", "kind": "bar", "x": "suite", "y": "ms", "format": "duration", "height": 200, "data": [
                { "suite": "loft-core", "ms": 18400 },
                { "suite": "loftd", "ms": 11200 },
                { "suite": "loft-git", "ms": 7600 },
                { "suite": "loft-ui", "ms": 5100 },
                { "suite": "loft-store", "ms": 3200 },
                { "suite": "loft-view", "ms": 400 }
            ]},
            { "type": "heading", "text": "Failures", "level": 3 },
            { "type": "table",
              "columns": [
                  { "key": "test", "label": "Test" },
                  { "key": "why", "label": "Why" },
                  { "key": "ms", "label": "Time", "format": "duration" }
              ],
              "rows": [
                  { "test": "thread::resume_after_restart", "why": "session id not restored", "ms": 912 },
                  { "test": "worktrees::merge_back_conflict", "why": "expected Conflict, got Ok", "ms": 340 },
                  { "test": "forks::handoff_digest", "why": "digest is missing the open todos", "ms": 88 }
              ]},
            { "type": "callout", "tone": "warning", "title": "Two of these share a cause",
              "text": "`resume_after_restart` and `handoff_digest` both read the session file before it is flushed." },
            { "type": "row", "children": [
                { "type": "button", "label": "Fix the shared cause", "style": "primary",
                  "send": "Fix the session-file flush that breaks resume_after_restart and handoff_digest, then re-run the tests." },
                { "type": "button", "label": "Explain merge_back_conflict",
                  "send": "Explain why worktrees::merge_back_conflict fails before changing anything." },
                { "type": "button", "label": "Copy the command", "style": "ghost", "copy": "cargo test --workspace" }
            ]}
        ]
    })
}

fn pricing() -> Value {
    json!({
        "title": "Team plan estimate",
        "state": { "seats": 12, "annual": true, "tier": "pro" },
        "body": [
            { "type": "row", "align": "end", "children": [
                { "type": "slider", "bind": "seats", "label": "Seats", "min": 1, "max": 100 },
                { "type": "choice", "bind": "tier", "label": "Tier", "options": [
                    { "label": "Starter", "value": "starter" },
                    { "label": "Pro", "value": "pro" },
                    { "label": "Scale", "value": "scale" }
                ]},
                { "type": "toggle", "bind": "annual", "label": "Bill yearly (−20%)" }
            ]},
            { "type": "row", "children": [
                { "type": "stat", "label": "Per seat",
                  "value": "{{ usd((tier == 'starter' ? 8 : tier == 'pro' ? 20 : 45) * (annual ? 0.8 : 1)) }}",
                  "caption": "per month" },
                { "type": "stat", "label": "Monthly", "tone": "accent",
                  "value": "{{ usd(seats * (tier == 'starter' ? 8 : tier == 'pro' ? 20 : 45) * (annual ? 0.8 : 1)) }}",
                  "caption": "{{seats}} seats" },
                { "type": "stat", "label": "Yearly",
                  "value": "{{ usd(12 * seats * (tier == 'starter' ? 8 : tier == 'pro' ? 20 : 45) * (annual ? 0.8 : 1)) }}",
                  "delta": "{{ annual ? '-20%' : '' }}", "delta_tone": "success" }
            ]},
            { "type": "chart", "kind": "area", "x": "seats", "y": [
                    { "key": "monthly", "label": "Billed monthly" },
                    { "key": "yearly", "label": "Billed yearly" }
              ], "format": "usd", "height": 200,
              "data": { "for": "n", "from": 0, "to": 100, "step": 10, "row": {
                  "seats": "{{n}}",
                  "monthly": "{{ n * (tier == 'starter' ? 8 : tier == 'pro' ? 20 : 45) }}",
                  "yearly": "{{ n * (tier == 'starter' ? 8 : tier == 'pro' ? 20 : 45) * 0.8 }}"
              }}},
            { "type": "callout", "when": "seats >= 50", "tone": "info",
              "text": "From 50 seats up, volume pricing applies: ask sales for a quote." },
            { "type": "button", "label": "Draft the purchase request", "style": "primary",
              "send": "Draft a purchase request for {{seats}} {{tier}} seats, billed {{ annual ? 'yearly' : 'monthly' }}." }
        ]
    })
}

fn deploy() -> Value {
    json!({
        "title": "Deploy",
        "state": { "env": "staging", "tag": "", "migrate": true, "notify": ["#deploys"], "confirm": false },
        "body": [
            { "type": "grid", "columns": 2, "children": [
                { "type": "select", "bind": "env", "label": "Environment", "options": ["staging", "production"] },
                { "type": "input", "bind": "tag", "label": "Image tag", "placeholder": "v1.4.2 (empty: latest main)" }
            ]},
            { "type": "choice", "bind": "notify", "label": "Tell", "multiple": true,
              "options": ["#deploys", "#eng", "on-call"] },
            { "type": "toggle", "bind": "migrate", "label": "Run database migrations first" },
            { "type": "callout", "tone": "danger", "when": "env == 'production'", "title": "Production",
              "text": "This reaches users. The last production deploy was 3 days ago." },
            { "type": "checkbox", "bind": "confirm", "label": "I checked the staging smoke tests", "when": "env == 'production'" },
            { "type": "key_value", "items": [
                { "label": "Target", "value": "{{env}}" },
                { "label": "Image", "value": "{{ tag == '' ? 'latest main' : tag }}" },
                { "label": "Migrations", "value": "{{ migrate ? 'run first' : 'skipped' }}" },
                { "label": "Announce in", "value": "{{ len(notify) == 0 ? 'nowhere' : join(notify, ', ') }}" }
            ]},
            { "type": "row", "children": [
                { "type": "button", "label": "Deploy to {{env}}", "style": "primary",
                  "when": "env != 'production' || confirm",
                  "send": "Deploy {{ tag == '' ? 'the latest main' : tag }} to {{env}}{{ migrate ? ', running migrations first' : ' without migrations' }}{{ len(notify) > 0 ? ', and announce it in ' + join(notify, ', ') : '' }}." },
                { "type": "button", "label": "Show the plan only", "send": "Show me the deploy plan for {{env}} without running anything." }
            ]}
        ]
    })
}

fn approach() -> Value {
    json!({
        "title": "Three ways to make search faster",
        "body": [
            { "type": "grid", "columns": 3, "children": [
                { "type": "card", "title": "Index on write", "children": [
                    { "type": "row", "gap": "sm", "children": [
                        { "type": "badge", "text": "Fastest reads", "tone": "success" },
                        { "type": "badge", "text": "~2 days" }
                    ]},
                    "Keep an FTS5 index current as items arrive. Reads drop to **~4 ms**; writes cost a little more.",
                    { "type": "button", "label": "Go with this", "style": "primary", "send": "Go with indexing on write (FTS5)." }
                ]},
                { "type": "card", "title": "Cache results", "children": [
                    { "type": "row", "gap": "sm", "children": [
                        { "type": "badge", "text": "Smallest change", "tone": "info" },
                        { "type": "badge", "text": "~3 hours" }
                    ]},
                    "Remember the last 200 queries. Repeats are instant; first searches stay at **~180 ms**.",
                    { "type": "button", "label": "Go with this", "send": "Go with caching recent search results." }
                ]},
                { "type": "card", "title": "Search in a worker", "children": [
                    { "type": "row", "gap": "sm", "children": [
                        { "type": "badge", "text": "No speedup", "tone": "warning" },
                        { "type": "badge", "text": "~1 day" }
                    ]},
                    "Move the scan off the main thread. Just as slow, but typing never stutters.",
                    { "type": "button", "label": "Go with this", "send": "Go with moving search into a worker." }
                ]}
            ]},
            { "type": "button", "label": "Compare them in more depth first", "style": "ghost",
              "send": "Compare the three search approaches in more depth before I pick." }
        ]
    })
}

fn release() -> Value {
    json!({
        "title": "Release 0.4.0",
        "state": { "done": 5 },
        "body": [
            { "type": "progress", "label": "{{done}} of 8 checks done", "value": "{{done}}", "max": 8 },
            { "type": "tabs", "tabs": [
                { "label": "Checklist", "children": [
                    { "type": "steps", "items": [
                        { "title": "Version bumped", "text": "Cargo.toml and the changelog", "status": "done" },
                        { "title": "Tests green on main", "status": "done" },
                        { "title": "Bundle notarized", "text": "Waiting on Apple: usually 5 minutes", "status": "active" },
                        { "title": "Homebrew cask updated", "status": "pending" },
                        { "title": "Linux packages", "text": "The arm64 build ran out of disk", "status": "failed" }
                    ]}
                ]},
                { "label": "Changes", "children": [
                    { "type": "list", "items": [
                        { "title": "Views agents draw in the chat", "text": "show_ui: charts, tables, forms", "badge": "new", "tone": "success" },
                        { "title": "Archived threads delete themselves", "text": "after a while, if you want", "badge": "new", "tone": "success" },
                        { "title": "Pull requests push with the GitHub CLI's sign-in", "badge": "fix", "tone": "info" },
                        { "title": "crates/loft-ui/src/items.rs", "text": "Open the timeline renderer", "open": "crates/loft-ui/src/items.rs" }
                    ]}
                ]},
                { "label": "Build", "children": [
                    { "type": "key_value", "items": [
                        { "label": "Commit", "value": "6f0b9c5" },
                        { "label": "Toolchain", "value": "rustc 1.99.0" },
                        { "label": "Bundle", "value": "{{ bytes(48234496) }}" },
                        { "label": "Web client", "value": "{{ bytes(9227469) }} gzipped" }
                    ]},
                    { "type": "code", "language": "bash", "text": "cargo xtask bundle\ncargo xtask web --release" }
                ]}
            ]},
            { "type": "details", "summary": "Why the arm64 build failed", "children": [
                "The runner had **1.8 GB** free and the release profile needs about 6. Clearing `target/` before the build, or a larger runner, fixes it."
            ]},
            { "type": "row", "children": [
                { "type": "button", "label": "Mark one more done", "set": { "done": "{{ min(done + 1, 8) }}" } },
                { "type": "button", "label": "Retry the arm64 build", "style": "primary",
                  "send": "Clear target/ on the arm64 runner and retry the Linux package build." }
            ]}
        ]
    })
}

fn sizes() -> Value {
    json!({
        "title": "Web client size",
        "body": [
            { "type": "grid", "columns": 2, "children": [
                { "type": "card", "title": "Gzipped, by release", "children": [
                    { "type": "chart", "kind": "line", "x": "release", "y": "gzip", "format": "bytes", "height": 190, "data": [
                        { "release": "0.1", "gzip": 6710886 },
                        { "release": "0.2", "gzip": 7969177 },
                        { "release": "0.3", "gzip": 9856614 },
                        { "release": "0.3.1", "gzip": 9017753 },
                        { "release": "0.4", "gzip": 9227469 }
                    ]}
                ]},
                { "type": "card", "title": "What it is made of", "children": [
                    { "type": "chart", "kind": "donut", "x": "part", "y": "bytes", "format": "bytes", "height": 190, "data": [
                        { "part": "UI toolkit", "bytes": 3900000 },
                        { "part": "Terminal", "bytes": 1900000 },
                        { "part": "Highlighting", "bytes": 1500000 },
                        { "part": "Agentloft", "bytes": 1200000 },
                        { "part": "Other", "bytes": 727469 }
                    ]}
                ]}
            ]},
            { "type": "table",
              "columns": [
                  { "key": "part", "label": "Part" },
                  { "key": "bytes", "label": "Size", "format": "bytes" },
                  { "key": "share", "label": "Share", "format": "percent" }
              ],
              "rows": [
                  { "part": "UI toolkit", "bytes": 3900000, "share": 0.423 },
                  { "part": "Terminal", "bytes": 1900000, "share": 0.206 },
                  { "part": "Highlighting", "bytes": 1500000, "share": 0.163 },
                  { "part": "Agentloft", "bytes": 1200000, "share": 0.13 },
                  { "part": "Other", "bytes": 727469, "share": 0.079 }
              ]}
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sample_reads_without_notes() {
        for (name, _) in NAMES {
            let spec = get(name).unwrap();
            match crate::parse(&spec) {
                Ok((_, notes)) => assert!(notes.is_empty(), "{name}: {notes:?}"),
                Err(e) => panic!("{name}: {e}"),
            }
        }
        assert_eq!(all().len(), NAMES.len());
    }
}
