//! What an agent hands `show_ui`, read into a checked tree.
//!
//! The reading is strict where a mistake would show the user something
//! wrong (an unknown block, a misspelt variable, a broken expression) and
//! lenient where the meaning is plain (a string where a block goes, a
//! common other name for a block, a property nobody knows: that one is
//! reported back as a note and otherwise ignored).

use crate::expr::{self, Expr, Format, Scope, Template};
use serde_json::{Map, Value};

/// The largest spec taken (bytes of JSON).
pub const MAX_BYTES: usize = 200_000;
/// The most blocks in one view.
pub const MAX_BLOCKS: usize = 400;
/// How deep blocks may nest.
pub const MAX_DEPTH: usize = 8;
/// The most rows a table or chart draws.
pub const MAX_ROWS: usize = 500;

/// Every block type, by the name agents write.
pub const TYPES: &[&str] = &[
    "row",
    "column",
    "grid",
    "card",
    "tabs",
    "details",
    "divider",
    "text",
    "heading",
    "markdown",
    "badge",
    "stat",
    "progress",
    "list",
    "key_value",
    "steps",
    "callout",
    "code",
    "table",
    "chart",
    "slider",
    "input",
    "select",
    "choice",
    "checkbox",
    "toggle",
    "button",
];

#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub title: Option<Template>,
    /// Where the state starts: what the agent gave, plus a value for every
    /// input bound to a name it left out.
    pub state: Map<String, Value>,
    pub body: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// Its place in the view (stable for as long as the spec is): what
    /// element ids and per-block UI state are keyed by.
    pub id: u32,
    /// Shown only while this holds.
    pub when: Option<Template>,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Row {
        children: Vec<Block>,
        gap: Gap,
        align: Option<Align>,
        justify: Justify,
        wrap: bool,
    },
    Column {
        children: Vec<Block>,
        gap: Gap,
    },
    Grid {
        columns: usize,
        children: Vec<Block>,
    },
    Card {
        title: Option<Template>,
        children: Vec<Block>,
    },
    Tabs {
        tabs: Vec<Tab>,
    },
    Details {
        summary: Template,
        open: bool,
        children: Vec<Block>,
    },
    Divider,
    Text {
        text: Template,
        size: Size,
        weight: Weight,
        tone: Tone,
        mono: bool,
    },
    Heading {
        text: Template,
        level: u8,
    },
    Markdown {
        text: Template,
    },
    Badge {
        text: Template,
        tone: Tone,
    },
    Stat {
        label: Template,
        value: Template,
        delta: Option<Template>,
        delta_tone: Option<Tone>,
        caption: Option<Template>,
        tone: Tone,
    },
    Progress {
        value: Template,
        max: Template,
        label: Option<Template>,
        tone: Tone,
    },
    List {
        items: Vec<ListItem>,
    },
    KeyValue {
        items: Vec<(Template, Template)>,
    },
    Steps {
        items: Vec<Step>,
    },
    Callout {
        title: Option<Template>,
        text: Template,
        tone: Tone,
    },
    Code {
        text: Template,
        language: Option<String>,
    },
    Table {
        columns: Vec<Column>,
        rows: Data,
    },
    Chart(Chart),
    Slider {
        bind: String,
        min: f64,
        max: f64,
        step: f64,
        label: Option<Template>,
        format: Option<Format>,
    },
    Input {
        bind: String,
        label: Option<Template>,
        placeholder: Option<String>,
        number: bool,
    },
    Select {
        bind: String,
        label: Option<Template>,
        options: Vec<Choice>,
    },
    Choice {
        bind: String,
        label: Option<Template>,
        options: Vec<Choice>,
        multiple: bool,
    },
    Checkbox {
        bind: String,
        label: Template,
    },
    Toggle {
        bind: String,
        label: Template,
    },
    Button {
        label: Template,
        action: Action,
        style: ButtonStyle,
    },
}

impl Kind {
    /// The name agents write for it.
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Row { .. } => "row",
            Kind::Column { .. } => "column",
            Kind::Grid { .. } => "grid",
            Kind::Card { .. } => "card",
            Kind::Tabs { .. } => "tabs",
            Kind::Details { .. } => "details",
            Kind::Divider => "divider",
            Kind::Text { .. } => "text",
            Kind::Heading { .. } => "heading",
            Kind::Markdown { .. } => "markdown",
            Kind::Badge { .. } => "badge",
            Kind::Stat { .. } => "stat",
            Kind::Progress { .. } => "progress",
            Kind::List { .. } => "list",
            Kind::KeyValue { .. } => "key_value",
            Kind::Steps { .. } => "steps",
            Kind::Callout { .. } => "callout",
            Kind::Code { .. } => "code",
            Kind::Table { .. } => "table",
            Kind::Chart(_) => "chart",
            Kind::Slider { .. } => "slider",
            Kind::Input { .. } => "input",
            Kind::Select { .. } => "select",
            Kind::Choice { .. } => "choice",
            Kind::Checkbox { .. } => "checkbox",
            Kind::Toggle { .. } => "toggle",
            Kind::Button { .. } => "button",
        }
    }

    /// Whether it takes the width it is given (a tile, a chart) rather
    /// than the width of its own content (a badge, a button): in a row,
    /// the first kind share the space evenly.
    pub fn fills(&self) -> bool {
        !matches!(
            self,
            Kind::Text { .. }
                | Kind::Heading { .. }
                | Kind::Badge { .. }
                | Kind::Button { .. }
                | Kind::Select { .. }
                | Kind::Choice { .. }
                | Kind::Checkbox { .. }
                | Kind::Toggle { .. }
                | Kind::Divider
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub label: Template,
    pub children: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub title: Template,
    pub text: Option<Template>,
    pub badge: Option<Template>,
    pub tone: Tone,
    pub action: Option<Action>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub title: Template,
    pub text: Option<Template>,
    pub status: StepStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Done,
    Active,
    Pending,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub key: String,
    pub label: Template,
    pub align: Option<ColumnAlign>,
    pub format: Option<Format>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chart {
    pub kind: ChartKind,
    pub data: Data,
    /// The key of each row's label (a bar's name, a point on the x axis).
    pub x: String,
    pub series: Vec<Series>,
    pub height: f32,
    pub format: Option<Format>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    Bar,
    Line,
    Area,
    Pie,
    Donut,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub key: String,
    pub label: String,
}

/// The rows of a table or chart: written out, or generated from a range.
#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    Rows(Vec<Vec<(String, Template)>>),
    Generated {
        var: String,
        from: Template,
        to: Template,
        step: Template,
        row: Vec<(String, Template)>,
    },
}

impl Data {
    /// The rows as they stand for this state (at most [`MAX_ROWS`]).
    pub fn rows(&self, scope: &Scope) -> Vec<Map<String, Value>> {
        let eval = |cells: &[(String, Template)], scope: &Scope| {
            cells
                .iter()
                .map(|(key, cell)| (key.clone(), cell.eval(scope)))
                .collect::<Map<String, Value>>()
        };
        match self {
            Data::Rows(rows) => rows.iter().take(MAX_ROWS).map(|r| eval(r, scope)).collect(),
            Data::Generated {
                var,
                from,
                to,
                step,
                row,
            } => {
                let (Some(from), Some(to)) = (from.number(scope), to.number(scope)) else {
                    return Vec::new();
                };
                let step = step.number(scope).unwrap_or(1.0);
                if step <= 0.0 || to < from {
                    return Vec::new();
                }
                let count = (((to - from) / step).floor() as usize).saturating_add(1);
                let mut locals = scope.locals.to_vec();
                locals.push((var.clone(), Value::Null));
                (0..count.min(MAX_ROWS))
                    .map(|i| {
                        if let Some(last) = locals.last_mut() {
                            last.1 = expr::num_value(from + step * i as f64);
                        }
                        eval(
                            row,
                            &Scope {
                                state: scope.state,
                                locals: &locals,
                            },
                        )
                    })
                    .collect()
            }
        }
    }
}

/// One option of a `select` or `choice`.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub label: Template,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Say this to the agent, as the user.
    Send(Template),
    /// Change the view's own state.
    Set(Vec<(String, Template)>),
    Copy(Template),
    /// Open a link, or a file of the thread's folder.
    Open(Template),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonStyle {
    Primary,
    #[default]
    Secondary,
    Ghost,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    #[default]
    Default,
    Muted,
    Accent,
    Info,
    Success,
    Warning,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Size {
    Xs,
    Sm,
    #[default]
    Md,
    Lg,
    Xl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Weight {
    #[default]
    Normal,
    Medium,
    Bold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Gap {
    None,
    Sm,
    #[default]
    Md,
    Lg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
    Stretch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Justify {
    #[default]
    Start,
    Center,
    End,
    Between,
}

// -- reading -----------------------------------------------------------------

/// Read a spec. `Ok` carries notes for the agent (properties that were
/// ignored); `Err` says what to fix.
pub fn parse(spec: &Value) -> Result<(View, Vec<String>), String> {
    // Some agents send the whole spec, or its body, as JSON in a string.
    let unwrapped;
    let spec = match spec {
        Value::String(s) => {
            unwrapped = serde_json::from_str::<Value>(s).map_err(
                |_| "the view must be a JSON object: { title?, state?, body: [blocks] }",
            )?;
            &unwrapped
        }
        other => other,
    };
    let size = serde_json::to_string(spec).map_or(0, |s| s.len());
    if size > MAX_BYTES {
        return Err(format!(
            "the view is {size} bytes; the most is {MAX_BYTES}. Show less data (a table draws at most {MAX_ROWS} rows)"
        ));
    }
    let Some(root) = spec.as_object() else {
        return Err("the view must be a JSON object: { title?, state?, body: [blocks] }".into());
    };
    let mut ctx = Ctx::default();
    // A lone block given as the whole view.
    let lone = root.contains_key("type") && !root.contains_key("body");
    let state = match root.get("state") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(map)) if !lone => map.clone(),
        Some(_) if lone => Map::new(),
        Some(_) => return Err("`state` must be an object of starting values".into()),
    };
    let title = match root.get("title") {
        Some(v) if !lone => ctx.template(v, "title")?,
        _ => None,
    };
    let body_value = if lone {
        Value::Array(vec![spec.clone()])
    } else {
        match root.get("body").or_else(|| root.get("children")) {
            Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                .ok()
                .filter(|v| v.is_array() || v.is_object())
                .ok_or("`body` must be a list of blocks")?,
            Some(v) => v.clone(),
            None => return Err("`body` is missing: a list of blocks to show".into()),
        }
    };
    let body = ctx.blocks(&body_value, "body", 0)?;
    if body.is_empty() {
        return Err("`body` is empty: give it at least one block".into());
    }
    if !lone {
        for key in root.keys() {
            if !matches!(key.as_str(), "title" | "state" | "body" | "children") {
                ctx.notes
                    .push(format!("unknown property `{key}` on the view (ignored)"));
            }
        }
    }
    let mut state = state;
    for (name, default) in &ctx.binds {
        if !state.contains_key(name) {
            state.insert(name.clone(), default.clone());
        }
    }
    // Every name an expression reads must exist, or the view would show
    // blanks where the agent meant numbers.
    for (name, path) in &ctx.reads {
        if !state.contains_key(name) && !ctx.sets.contains(name) {
            let mut known: Vec<&str> = state.keys().map(String::as_str).collect();
            known.sort_unstable();
            return Err(format!(
                "{path}: `{name}` is not in `state`{}",
                if known.is_empty() {
                    " (the view has no state: add `state: { … }` or bind an input to that name)"
                        .to_owned()
                } else {
                    format!("; state has: {}", known.join(", "))
                }
            ));
        }
    }
    Ok((View { title, state, body }, ctx.notes))
}

#[derive(Default)]
struct Ctx {
    notes: Vec<String>,
    blocks: usize,
    /// Names read by expressions, and where.
    reads: Vec<(String, String)>,
    /// Names inputs write, with the value each starts from.
    binds: Vec<(String, Value)>,
    /// Names `set` actions write.
    sets: Vec<String>,
    /// Loop variables in scope.
    locals: Vec<String>,
}

/// One block's properties, remembering which were read (the rest are
/// reported as unknown).
struct Props<'a> {
    map: &'a Map<String, Value>,
    path: &'a str,
    read: Vec<&'static str>,
}

impl<'a> Props<'a> {
    fn get(&mut self, key: &'static str) -> Option<&'a Value> {
        self.read.push(key);
        self.map.get(key).filter(|v| !v.is_null())
    }

    /// The first of several names for one property.
    fn any(&mut self, keys: &[&'static str]) -> Option<&'a Value> {
        let mut found = None;
        for key in keys {
            let value = self.get(key);
            found = found.or(value);
        }
        found
    }

    fn at(&self, key: &str) -> String {
        format!("{}.{key}", self.path)
    }

    fn word<T: Copy>(
        &mut self,
        key: &'static str,
        options: &[(&str, T)],
    ) -> Result<Option<T>, String> {
        let Some(value) = self.get(key) else {
            return Ok(None);
        };
        let given = match value {
            Value::String(s) => s.trim().to_lowercase(),
            other => other.to_string(),
        };
        match options.iter().find(|(name, _)| *name == given) {
            Some((_, v)) => Ok(Some(*v)),
            None => {
                let mut names: Vec<&str> = Vec::new();
                for (name, _) in options {
                    if !names.contains(name) {
                        names.push(name);
                    }
                }
                Err(format!(
                    "{}: `{given}` is not one of {}",
                    self.at(key),
                    names.join(", ")
                ))
            }
        }
    }

    fn flag(&mut self, key: &'static str) -> Result<bool, String> {
        match self.get(key) {
            None => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(Value::String(s)) if s == "true" || s == "false" => Ok(s == "true"),
            Some(_) => Err(format!("{}: must be true or false", self.at(key))),
        }
    }

    fn number(&mut self, key: &'static str) -> Result<Option<f64>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => match expr::number(v).filter(|_| !v.is_boolean()) {
                Some(n) if n.is_finite() => Ok(Some(n)),
                _ => Err(format!("{}: must be a number", self.at(key))),
            },
        }
    }

    fn string(&mut self, key: &'static str) -> Result<Option<String>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("{}: must be a string", self.at(key))),
        }
    }
}

const TONES: &[(&str, Tone)] = &[
    ("default", Tone::Default),
    ("muted", Tone::Muted),
    ("accent", Tone::Accent),
    ("info", Tone::Info),
    ("success", Tone::Success),
    ("warning", Tone::Warning),
    ("danger", Tone::Danger),
    // What agents also say.
    ("neutral", Tone::Default),
    ("secondary", Tone::Muted),
    ("subtle", Tone::Muted),
    ("primary", Tone::Accent),
    ("blue", Tone::Info),
    ("positive", Tone::Success),
    ("good", Tone::Success),
    ("green", Tone::Success),
    ("warn", Tone::Warning),
    ("caution", Tone::Warning),
    ("yellow", Tone::Warning),
    ("orange", Tone::Warning),
    ("error", Tone::Danger),
    ("negative", Tone::Danger),
    ("bad", Tone::Danger),
    ("critical", Tone::Danger),
    ("red", Tone::Danger),
];

const GAPS: &[(&str, Gap)] = &[
    ("none", Gap::None),
    ("sm", Gap::Sm),
    ("md", Gap::Md),
    ("lg", Gap::Lg),
    ("small", Gap::Sm),
    ("medium", Gap::Md),
    ("large", Gap::Lg),
];

/// Other names agents reach for, and the block each means.
fn canonical_type(name: &str) -> &str {
    match name {
        "col" | "stack" | "vstack" | "box" | "section" | "group" | "form" => "column",
        "hstack" | "inline" | "buttons" | "button_group" => "row",
        "columns" => "grid",
        "panel" => "card",
        "accordion" | "collapsible" | "disclosure" => "details",
        "separator" | "hr" | "rule" => "divider",
        "paragraph" | "label" | "caption" => "text",
        "title" | "header" => "heading",
        "md" => "markdown",
        "tag" | "chip" | "pill" => "badge",
        "metric" | "kpi" | "statistic" => "stat",
        "progress_bar" | "meter" => "progress",
        "kv" | "keyvalue" | "description_list" | "properties" => "key_value",
        "timeline" | "stepper" => "steps",
        "alert" | "banner" | "note" => "callout",
        "code_block" | "codeblock" => "code",
        "data_table" => "table",
        "graph" | "plot" => "chart",
        "range" => "slider",
        "text_input" | "textfield" | "text_field" | "number_input" | "textarea" => "input",
        "dropdown" | "combobox" => "select",
        "radio" | "radio_group" | "segmented" | "chips" => "choice",
        "check" => "checkbox",
        "switch" => "toggle",
        other => other,
    }
}

impl Ctx {
    /// Text that may hold `{{ expressions }}`; numbers and booleans are
    /// taken as they are.
    fn template(&mut self, value: &Value, path: &str) -> Result<Option<Template>, String> {
        let template = match value {
            Value::Null => return Ok(None),
            Value::String(s) => Template::parse(s).map_err(|e| format!("{path}: {e}"))?,
            Value::Number(_) | Value::Bool(_) => Template::value(value.clone()),
            _ => return Err(format!("{path}: must be text")),
        };
        self.note_reads(&template, path);
        Ok(Some(template))
    }

    fn note_reads(&mut self, template: &Template, path: &str) {
        let mut names = Vec::new();
        template.variables(&mut names);
        for name in names {
            if !self.locals.contains(&name) && !self.reads.iter().any(|(n, _)| *n == name) {
                self.reads.push((name, path.to_owned()));
            }
        }
    }

    fn text(&mut self, props: &mut Props, key: &'static str) -> Result<Option<Template>, String> {
        let path = props.at(key);
        match props.get(key) {
            Some(v) => self.template(v, &path),
            None => Ok(None),
        }
    }

    fn required(&mut self, props: &mut Props, keys: &[&'static str]) -> Result<Template, String> {
        let path = props.at(keys[0]);
        match props.any(keys) {
            Some(v) => self
                .template(v, &path)?
                .ok_or_else(|| format!("{path} is missing")),
            None => Err(format!("{path} is missing")),
        }
    }

    /// `when`: a bare expression, or one in braces.
    fn condition(&mut self, value: &Value, path: &str) -> Result<Template, String> {
        let template = match value {
            Value::String(s) if s.contains("{{") => {
                Template::parse(s).map_err(|e| format!("{path}: {e}"))?
            }
            Value::String(s) => Template::expr(Expr::parse(s).map_err(|e| format!("{path}: {e}"))?),
            Value::Bool(_) => Template::value(value.clone()),
            _ => {
                return Err(format!(
                    "{path}: must be an expression, like \"plan == 'pro'\""
                ));
            }
        };
        self.note_reads(&template, path);
        Ok(template)
    }

    fn blocks(&mut self, value: &Value, path: &str, depth: usize) -> Result<Vec<Block>, String> {
        let list: Vec<&Value> = match value {
            Value::Array(list) => list.iter().collect(),
            // One block where a list goes.
            Value::Object(_) | Value::String(_) => vec![value],
            _ => return Err(format!("{path}: must be a list of blocks")),
        };
        list.iter()
            .enumerate()
            .map(|(ix, v)| self.block(v, &format!("{path}[{ix}]"), depth))
            .collect()
    }

    fn children(
        &mut self,
        props: &mut Props,
        depth: usize,
        required: bool,
    ) -> Result<Vec<Block>, String> {
        let path = props.at("children");
        match props.any(&["children", "content", "body", "items"]) {
            Some(v) => self.blocks(v, &path, depth + 1),
            None if required => Err(format!("{path} is missing: a list of blocks")),
            None => Ok(Vec::new()),
        }
    }

    fn block(&mut self, value: &Value, path: &str, depth: usize) -> Result<Block, String> {
        if depth > MAX_DEPTH {
            return Err(format!("{path}: blocks nest more than {MAX_DEPTH} deep"));
        }
        self.blocks += 1;
        if self.blocks > MAX_BLOCKS {
            return Err(format!("the view has more than {MAX_BLOCKS} blocks"));
        }
        let id = self.blocks as u32;
        let map = match value {
            // A bare string is a paragraph.
            Value::String(s) => {
                let text = self
                    .template(&Value::String(s.clone()), path)?
                    .unwrap_or_default();
                return Ok(Block {
                    id,
                    when: None,
                    kind: Kind::Markdown { text },
                });
            }
            Value::Object(map) => map,
            _ => return Err(format!("{path}: a block must be an object with a `type`")),
        };
        let Some(given) = map.get("type").and_then(Value::as_str) else {
            return Err(format!(
                "{path}: a block needs a `type`; types: {}",
                TYPES.join(", ")
            ));
        };
        let given = given.trim().to_lowercase().replace('-', "_");
        let name = canonical_type(&given);
        let path = format!("{path} ({name})");
        let mut props = Props {
            map,
            path: &path,
            read: vec!["type", "when", "id"],
        };
        let when = match map.get("when").filter(|v| !v.is_null()) {
            Some(v) => Some(self.condition(v, &format!("{path}.when"))?),
            None => None,
        };
        let kind = self.kind(name, &mut props, depth)?;
        for key in map.keys() {
            if !props.read.contains(&key.as_str()) {
                self.notes
                    .push(format!("{path}: unknown property `{key}` (ignored)"));
            }
        }
        Ok(Block { id, when, kind })
    }

    fn kind(&mut self, name: &str, p: &mut Props, depth: usize) -> Result<Kind, String> {
        let tone = |p: &mut Props| p.word("tone", TONES).map(Option::unwrap_or_default);
        Ok(match name {
            "row" => Kind::Row {
                children: self.children(p, depth, true)?,
                gap: p.word("gap", GAPS)?.unwrap_or_default(),
                align: p.word(
                    "align",
                    &[
                        ("start", Align::Start),
                        ("center", Align::Center),
                        ("end", Align::End),
                        ("stretch", Align::Stretch),
                        ("top", Align::Start),
                        ("middle", Align::Center),
                        ("bottom", Align::End),
                    ],
                )?,
                justify: p
                    .word(
                        "justify",
                        &[
                            ("start", Justify::Start),
                            ("center", Justify::Center),
                            ("end", Justify::End),
                            ("between", Justify::Between),
                            ("space_between", Justify::Between),
                            ("space-between", Justify::Between),
                            ("left", Justify::Start),
                            ("right", Justify::End),
                        ],
                    )?
                    .unwrap_or_default(),
                wrap: p.flag("wrap")?,
            },
            "column" => Kind::Column {
                children: self.children(p, depth, true)?,
                gap: p.word("gap", GAPS)?.unwrap_or_default(),
            },
            "grid" => {
                let columns = p.number("columns")?.unwrap_or(2.0);
                if !(1.0..=6.0).contains(&columns) {
                    return Err(format!("{}: must be 1 to 6", p.at("columns")));
                }
                Kind::Grid {
                    columns: columns as usize,
                    children: self.children(p, depth, true)?,
                }
            }
            "card" => Kind::Card {
                title: self.text(p, "title")?,
                children: self.children(p, depth, true)?,
            },
            "tabs" => {
                let path = p.at("tabs");
                let Some(Value::Array(list)) = p.get("tabs") else {
                    return Err(format!("{path} is missing: [{{ label, children }}]"));
                };
                if list.is_empty() {
                    return Err(format!("{path}: give it at least one tab"));
                }
                let mut tabs = Vec::new();
                for (ix, tab) in list.iter().enumerate() {
                    let path = format!("{path}[{ix}]");
                    let Some(map) = tab.as_object() else {
                        return Err(format!("{path}: must be {{ label, children }}"));
                    };
                    let mut tp = Props {
                        map,
                        path: &path,
                        read: Vec::new(),
                    };
                    tabs.push(Tab {
                        label: self.required(&mut tp, &["label", "title"])?,
                        children: self.children(&mut tp, depth, true)?,
                    });
                }
                Kind::Tabs { tabs }
            }
            "details" => Kind::Details {
                summary: self.required(p, &["summary", "title", "label"])?,
                open: p.flag("open")?,
                children: self.children(p, depth, true)?,
            },
            "divider" => Kind::Divider,
            "text" => Kind::Text {
                text: self.required(p, &["text", "value"])?,
                size: p
                    .word(
                        "size",
                        &[
                            ("xs", Size::Xs),
                            ("sm", Size::Sm),
                            ("md", Size::Md),
                            ("lg", Size::Lg),
                            ("xl", Size::Xl),
                            ("small", Size::Sm),
                            ("medium", Size::Md),
                            ("large", Size::Lg),
                        ],
                    )?
                    .unwrap_or_default(),
                weight: p
                    .word(
                        "weight",
                        &[
                            ("normal", Weight::Normal),
                            ("medium", Weight::Medium),
                            ("bold", Weight::Bold),
                            ("semibold", Weight::Bold),
                            ("regular", Weight::Normal),
                        ],
                    )?
                    .unwrap_or_default(),
                tone: tone(p)?,
                mono: p.flag("mono")?,
            },
            "heading" => {
                let level = p.number("level")?.unwrap_or(2.0);
                Kind::Heading {
                    text: self.required(p, &["text", "title"])?,
                    level: level.clamp(1.0, 3.0) as u8,
                }
            }
            "markdown" => Kind::Markdown {
                text: self.required(p, &["text", "markdown", "content"])?,
            },
            "badge" => Kind::Badge {
                text: self.required(p, &["text", "label"])?,
                tone: tone(p)?,
            },
            "stat" => Kind::Stat {
                label: self.required(p, &["label", "title"])?,
                value: self.required(p, &["value"])?,
                delta: self.text(p, "delta")?,
                delta_tone: p.word("delta_tone", TONES)?,
                caption: self.text(p, "caption")?,
                tone: tone(p)?,
            },
            "progress" => Kind::Progress {
                value: self.required(p, &["value"])?,
                max: self
                    .text(p, "max")?
                    .unwrap_or_else(|| Template::value(Value::from(100))),
                label: self.text(p, "label")?,
                tone: tone(p)?,
            },
            "list" => {
                let path = p.at("items");
                let Some(Value::Array(list)) = p.get("items") else {
                    return Err(format!("{path} is missing: [{{ title, text?, badge? }}]"));
                };
                let mut items = Vec::new();
                for (ix, item) in list.iter().enumerate() {
                    let path = format!("{path}[{ix}]");
                    items.push(match item {
                        Value::Object(map) => {
                            let mut ip = Props {
                                map,
                                path: &path,
                                read: Vec::new(),
                            };
                            ListItem {
                                title: self.required(&mut ip, &["title", "label", "text"])?,
                                text: match map.contains_key("title") || map.contains_key("label") {
                                    true => self.text(&mut ip, "text")?,
                                    false => None,
                                },
                                badge: self.text(&mut ip, "badge")?,
                                tone: ip.word("tone", TONES)?.unwrap_or_default(),
                                action: self.action(&mut ip)?,
                            }
                        }
                        other => ListItem {
                            title: self
                                .template(other, &path)?
                                .ok_or_else(|| format!("{path} is empty"))?,
                            text: None,
                            badge: None,
                            tone: Tone::Default,
                            action: None,
                        },
                    });
                }
                Kind::List { items }
            }
            "key_value" => {
                let path = p.at("items");
                let mut items = Vec::new();
                match p.any(&["items", "rows", "data"]) {
                    Some(Value::Array(list)) => {
                        for (ix, item) in list.iter().enumerate() {
                            let path = format!("{path}[{ix}]");
                            let Some(map) = item.as_object() else {
                                return Err(format!("{path}: must be {{ label, value }}"));
                            };
                            let mut ip = Props {
                                map,
                                path: &path,
                                read: Vec::new(),
                            };
                            items.push((
                                self.required(&mut ip, &["label", "key", "name"])?,
                                self.required(&mut ip, &["value"])?,
                            ));
                        }
                    }
                    // { "Branch": "main", "Commit": "6f0b9c5" }
                    Some(Value::Object(map)) => {
                        for (key, value) in map {
                            let value = self
                                .template(value, &format!("{path}.{key}"))?
                                .unwrap_or_default();
                            items.push((Template::value(Value::String(key.clone())), value));
                        }
                    }
                    _ => return Err(format!("{path} is missing: [{{ label, value }}]")),
                }
                Kind::KeyValue { items }
            }
            "steps" => {
                let path = p.at("items");
                let Some(Value::Array(list)) = p.any(&["items", "steps"]) else {
                    return Err(format!("{path} is missing: [{{ title, text?, status? }}]"));
                };
                let mut items = Vec::new();
                for (ix, item) in list.iter().enumerate() {
                    let path = format!("{path}[{ix}]");
                    items.push(match item {
                        Value::Object(map) => {
                            let mut ip = Props {
                                map,
                                path: &path,
                                read: Vec::new(),
                            };
                            Step {
                                title: self.required(&mut ip, &["title", "label"])?,
                                text: self.text(&mut ip, "text")?,
                                status: ip
                                    .word(
                                        "status",
                                        &[
                                            ("done", StepStatus::Done),
                                            ("active", StepStatus::Active),
                                            ("pending", StepStatus::Pending),
                                            ("failed", StepStatus::Failed),
                                            ("completed", StepStatus::Done),
                                            ("complete", StepStatus::Done),
                                            ("in_progress", StepStatus::Active),
                                            ("running", StepStatus::Active),
                                            ("current", StepStatus::Active),
                                            ("todo", StepStatus::Pending),
                                            ("error", StepStatus::Failed),
                                        ],
                                    )?
                                    .unwrap_or(StepStatus::Pending),
                            }
                        }
                        other => Step {
                            title: self
                                .template(other, &path)?
                                .ok_or_else(|| format!("{path} is empty"))?,
                            text: None,
                            status: StepStatus::Pending,
                        },
                    });
                }
                Kind::Steps { items }
            }
            "callout" => Kind::Callout {
                title: self.text(p, "title")?,
                text: self.required(p, &["text", "message"])?,
                tone: p.word("tone", TONES)?.unwrap_or(Tone::Info),
            },
            "code" => Kind::Code {
                text: self.required(p, &["text", "code"])?,
                language: p.any(&["language", "lang"]).and_then(|v| {
                    v.as_str()
                        .filter(|s| {
                            s.chars()
                                .all(|c| c.is_alphanumeric() || "+-#_.".contains(c))
                        })
                        .map(str::to_owned)
                }),
            },
            "table" => self.table(p)?,
            "chart" => self.chart(p)?,
            "slider" => {
                let bind = self.bind(p)?;
                let min = p.number("min")?.unwrap_or(0.0);
                let max = p.number("max")?.unwrap_or(100.0);
                if max <= min {
                    return Err(format!("{}: must be above `min`", p.at("max")));
                }
                let step = p.number("step")?.unwrap_or(1.0);
                if step <= 0.0 {
                    return Err(format!("{}: must be above 0", p.at("step")));
                }
                let start = p.number("value")?.unwrap_or(min).clamp(min, max);
                self.binds.push((bind.clone(), expr::num_value(start)));
                Kind::Slider {
                    bind,
                    min,
                    max,
                    step,
                    label: self.text(p, "label")?,
                    format: self.format(p)?,
                }
            }
            "input" => {
                let bind = self.bind(p)?;
                let number = matches!(
                    p.any(&["kind", "input_type"]).and_then(Value::as_str),
                    Some("number" | "numeric")
                );
                let start = match p.get("value") {
                    Some(v) if number => expr::number(v).map_or(Value::Null, expr::num_value),
                    Some(v) => Value::String(expr::text(v)),
                    None if number => Value::Null,
                    None => Value::String(String::new()),
                };
                self.binds.push((bind.clone(), start));
                Kind::Input {
                    bind,
                    label: self.text(p, "label")?,
                    placeholder: p.string("placeholder")?,
                    number,
                }
            }
            "select" | "choice" => {
                let bind = self.bind(p)?;
                let options = self.options(p)?;
                let multiple = name == "choice" && p.flag("multiple")?;
                let start = match p.get("value") {
                    Some(v) => v.clone(),
                    None if multiple => Value::Array(Vec::new()),
                    None => options[0].value.clone(),
                };
                self.binds.push((bind.clone(), start));
                let label = self.text(p, "label")?;
                if name == "select" {
                    Kind::Select {
                        bind,
                        label,
                        options,
                    }
                } else {
                    Kind::Choice {
                        bind,
                        label,
                        options,
                        multiple,
                    }
                }
            }
            "checkbox" | "toggle" => {
                let bind = self.bind(p)?;
                let start = matches!(p.any(&["value", "checked"]), Some(Value::Bool(true)));
                self.binds.push((bind.clone(), Value::Bool(start)));
                let label = self.required(p, &["label", "text"])?;
                if name == "checkbox" {
                    Kind::Checkbox { bind, label }
                } else {
                    Kind::Toggle { bind, label }
                }
            }
            "button" => {
                let label = self.required(p, &["label", "text"])?;
                let Some(action) = self.action(p)? else {
                    return Err(format!(
                        "{}: a button needs an action: {{ send: \"…\" }}, {{ set: {{…}} }}, {{ copy: \"…\" }} or {{ open: \"…\" }}",
                        p.at("action")
                    ));
                };
                Kind::Button {
                    label,
                    action,
                    style: p
                        .any(&["style", "variant"])
                        .map(|v| match v.as_str().unwrap_or_default() {
                            "primary" | "accent" => ButtonStyle::Primary,
                            "ghost" | "link" | "text" => ButtonStyle::Ghost,
                            "danger" | "destructive" => ButtonStyle::Danger,
                            _ => ButtonStyle::Secondary,
                        })
                        .unwrap_or_default(),
                }
            }
            other => {
                return Err(format!(
                    "{}: unknown block type `{other}`; types: {}",
                    p.path.rsplit_once(" (").map_or(p.path, |(head, _)| head),
                    TYPES.join(", ")
                ));
            }
        })
    }

    /// The state name an input writes.
    fn bind(&mut self, p: &mut Props) -> Result<String, String> {
        let path = p.at("bind");
        match p.any(&["bind", "name"]) {
            Some(Value::String(name)) if is_name(name) => Ok(name.clone()),
            Some(_) => Err(format!(
                "{path}: must be a name (letters, digits, _), like \"seats\""
            )),
            None => Err(format!(
                "{path} is missing: the state name this input writes"
            )),
        }
    }

    fn format(&mut self, p: &mut Props) -> Result<Option<Format>, String> {
        let path = p.at("format");
        match p.get("format") {
            None => Ok(None),
            Some(v) => v
                .as_str()
                .and_then(|s| Format::named(&s.to_lowercase()))
                .map(Some)
                .ok_or_else(|| format!("{path}: must be one of {}", Format::NAMES.join(", "))),
        }
    }

    fn options(&mut self, p: &mut Props) -> Result<Vec<Choice>, String> {
        let path = p.at("options");
        let Some(Value::Array(list)) = p.any(&["options", "choices"]) else {
            return Err(format!(
                "{path} is missing: [\"a\", \"b\"] or [{{ label, value }}]"
            ));
        };
        if list.is_empty() {
            return Err(format!("{path}: give it at least one option"));
        }
        let mut options = Vec::new();
        for (ix, option) in list.iter().enumerate() {
            let path = format!("{path}[{ix}]");
            options.push(match option {
                Value::Object(map) => {
                    let label = map.get("label").or_else(|| map.get("value"));
                    let Some(label) = label else {
                        return Err(format!("{path}: must be a string or {{ label, value }}"));
                    };
                    Choice {
                        label: self.template(label, &path)?.unwrap_or_default(),
                        value: map.get("value").unwrap_or(label).clone(),
                    }
                }
                Value::String(_) | Value::Number(_) | Value::Bool(_) => Choice {
                    // An option's text is shown as written, braces and all.
                    label: Template::value(Value::String(expr::text(option))),
                    value: option.clone(),
                },
                _ => return Err(format!("{path}: must be a string or {{ label, value }}")),
            });
        }
        Ok(options)
    }

    /// What a button (or a list row) does: `action: { send: … }`, or the
    /// same keys on the block itself.
    fn action(&mut self, p: &mut Props) -> Result<Option<Action>, String> {
        let path = p.at("action");
        let nested = match p.any(&["action", "on_click", "onClick"]) {
            Some(Value::Object(map)) => Some(map),
            Some(_) => {
                return Err(format!(
                    "{path}: must be {{ send }}, {{ set }}, {{ copy }} or {{ open }}"
                ));
            }
            None => None,
        };
        let mut found = None;
        for key in ["send", "set", "copy", "open"] {
            let inline = p.get(key);
            let value = nested.and_then(|m| m.get(key)).or(inline);
            if found.is_none()
                && let Some(value) = value
            {
                found = Some((key, value));
            }
        }
        // { type: "send", text: "…" } and friends.
        if found.is_none()
            && let Some(map) = nested
            && let Some(kind) = map.get("type").and_then(Value::as_str)
        {
            let payload = ["text", "message", "value", "url", "path", "state"]
                .iter()
                .find_map(|k| map.get(*k));
            let key = match kind {
                "send" | "message" | "reply" => Some("send"),
                "set" | "set_state" => Some("set"),
                "copy" => Some("copy"),
                "open" | "open_url" | "open_file" | "link" => Some("open"),
                _ => None,
            };
            if let (Some(key), Some(payload)) = (key, payload) {
                found = Some((key, payload));
            }
        }
        let Some((key, value)) = found else {
            return match nested {
                Some(_) => Err(format!(
                    "{path}: must be {{ send }}, {{ set }}, {{ copy }} or {{ open }}"
                )),
                None => Ok(None),
            };
        };
        let path = format!("{path}.{key}");
        Ok(Some(match key {
            "set" => {
                let Some(map) = value.as_object() else {
                    return Err(format!(
                        "{path}: must be an object of state names and their new values"
                    ));
                };
                let mut changes = Vec::new();
                for (name, v) in map {
                    if !is_name(name) {
                        return Err(format!("{path}: `{name}` is not a state name"));
                    }
                    let template = match v {
                        Value::String(_) | Value::Number(_) | Value::Bool(_) => self
                            .template(v, &format!("{path}.{name}"))?
                            .unwrap_or_default(),
                        // Lists and objects are set as they are.
                        other => Template::value(other.clone()),
                    };
                    self.sets.push(name.clone());
                    changes.push((name.clone(), template));
                }
                Action::Set(changes)
            }
            _ => {
                let Some(template) = self.template(value, &path)? else {
                    return Err(format!("{path} is empty"));
                };
                match key {
                    "send" => Action::Send(template),
                    "copy" => Action::Copy(template),
                    _ => Action::Open(template),
                }
            }
        }))
    }

    /// Rows written out, or `{ for, from, to, step?, row }`.
    fn data(&mut self, value: &Value, path: &str, columns: &[String]) -> Result<Data, String> {
        match value {
            Value::Array(list) => {
                if list.len() > MAX_ROWS {
                    self.notes.push(format!(
                        "{path}: {} rows given; the first {MAX_ROWS} are shown",
                        list.len()
                    ));
                }
                let mut rows = Vec::new();
                for (ix, row) in list.iter().take(MAX_ROWS).enumerate() {
                    rows.push(self.row(row, &format!("{path}[{ix}]"), columns)?);
                }
                Ok(Data::Rows(rows))
            }
            Value::Object(map) if map.contains_key("for") || map.contains_key("row") => {
                let var = match map.get("for") {
                    Some(Value::String(name)) if is_name(name) => name.clone(),
                    None => "i".to_owned(),
                    Some(_) => return Err(format!("{path}.for: must be a name, like \"i\"")),
                };
                let bound = |ctx: &mut Self, key: &str, default: f64| {
                    let at = format!("{path}.{key}");
                    match map.get(key) {
                        Some(v) => ctx
                            .template(v, &at)?
                            .ok_or_else(|| format!("{at}: must be a number")),
                        None => Ok(Template::value(Value::from(default))),
                    }
                };
                let from = bound(self, "from", 0.0)?;
                let step = bound(self, "step", 1.0)?;
                if !map.contains_key("to") {
                    return Err(format!("{path}.to is missing: where the range ends"));
                }
                let to = bound(self, "to", 0.0)?;
                let Some(row) = map.get("row") else {
                    return Err(format!(
                        "{path}.row is missing: the cells of each generated row"
                    ));
                };
                self.locals.push(var.clone());
                let row = self.row(row, &format!("{path}.row"), columns);
                self.locals.pop();
                Ok(Data::Generated {
                    var,
                    from,
                    to,
                    step,
                    row: row?,
                })
            }
            _ => Err(format!(
                "{path}: must be a list of rows, or {{ for, from, to, step?, row }}"
            )),
        }
    }

    /// One row: an object by column key, or a list in column order.
    fn row(
        &mut self,
        value: &Value,
        path: &str,
        columns: &[String],
    ) -> Result<Vec<(String, Template)>, String> {
        let mut cells = Vec::new();
        let mut cell = |ctx: &mut Self, key: &str, v: &Value| -> Result<(), String> {
            let template = match v {
                Value::Null => Template::default(),
                Value::String(_) | Value::Number(_) | Value::Bool(_) => ctx
                    .template(v, &format!("{path}.{key}"))?
                    .unwrap_or_default(),
                other => Template::value(Value::String(expr::text(other))),
            };
            cells.push((key.to_owned(), template));
            Ok(())
        };
        match value {
            Value::Object(map) => {
                for (key, v) in map {
                    cell(self, key, v)?;
                }
            }
            Value::Array(list) if !columns.is_empty() => {
                for (key, v) in columns.iter().zip(list) {
                    cell(self, key, v)?;
                }
            }
            _ => {
                return Err(format!(
                    "{path}: a row must be an object keyed by column, like {{ \"name\": \"a\", \"ms\": 12 }}"
                ));
            }
        }
        Ok(cells)
    }

    fn table(&mut self, p: &mut Props) -> Result<Kind, String> {
        let path = p.at("columns");
        let Some(Value::Array(list)) = p.get("columns") else {
            return Err(format!(
                "{path} is missing: [\"name\", …] or [{{ key, label?, align?, format? }}]"
            ));
        };
        if list.is_empty() || list.len() > 12 {
            return Err(format!("{path}: give it 1 to 12 columns"));
        }
        let mut columns = Vec::new();
        for (ix, column) in list.iter().enumerate() {
            let path = format!("{path}[{ix}]");
            columns.push(match column {
                Value::String(key) => Column {
                    key: key.clone(),
                    label: Template::value(Value::String(key.clone())),
                    align: None,
                    format: None,
                },
                Value::Object(map) => {
                    let mut cp = Props {
                        map,
                        path: &path,
                        read: Vec::new(),
                    };
                    let Some(key) = cp.string("key")? else {
                        return Err(format!("{path}.key is missing"));
                    };
                    Column {
                        label: self
                            .text(&mut cp, "label")?
                            .unwrap_or_else(|| Template::value(Value::String(key.clone()))),
                        key,
                        align: cp.word(
                            "align",
                            &[
                                ("left", ColumnAlign::Left),
                                ("center", ColumnAlign::Center),
                                ("right", ColumnAlign::Right),
                                ("start", ColumnAlign::Left),
                                ("end", ColumnAlign::Right),
                            ],
                        )?,
                        format: self.format(&mut cp)?,
                    }
                }
                _ => return Err(format!("{path}: must be a key or {{ key, label? }}")),
            });
        }
        let keys: Vec<String> = columns.iter().map(|c| c.key.clone()).collect();
        let path = p.at("rows");
        let Some(rows) = p.any(&["rows", "data"]) else {
            return Err(format!("{path} is missing: a list of rows"));
        };
        Ok(Kind::Table {
            rows: self.data(rows, &path, &keys)?,
            columns,
        })
    }

    fn chart(&mut self, p: &mut Props) -> Result<Kind, String> {
        let kind = p
            .word(
                "kind",
                &[
                    ("bar", ChartKind::Bar),
                    ("line", ChartKind::Line),
                    ("area", ChartKind::Area),
                    ("pie", ChartKind::Pie),
                    ("donut", ChartKind::Donut),
                    ("column", ChartKind::Bar),
                    ("doughnut", ChartKind::Donut),
                ],
            )?
            .unwrap_or(ChartKind::Bar);
        let Some(x) = p.string("x")? else {
            return Err(format!(
                "{} is missing: the row key that names each bar, point or slice",
                p.at("x")
            ));
        };
        let path = p.at("y");
        let one = |v: &Value, path: &str| -> Result<Series, String> {
            match v {
                Value::String(key) => Ok(Series {
                    key: key.clone(),
                    label: key.clone(),
                }),
                Value::Object(map) => {
                    let Some(key) = map.get("key").and_then(Value::as_str) else {
                        return Err(format!("{path}: must be a row key or {{ key, label }}"));
                    };
                    Ok(Series {
                        key: key.to_owned(),
                        label: map
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or(key)
                            .to_owned(),
                    })
                }
                _ => Err(format!("{path}: must be a row key or {{ key, label }}")),
            }
        };
        let series = match p.get("y") {
            Some(Value::Array(list)) if !list.is_empty() => list
                .iter()
                .enumerate()
                .map(|(ix, v)| one(v, &format!("{path}[{ix}]")))
                .collect::<Result<Vec<_>, _>>()?,
            Some(v @ (Value::String(_) | Value::Object(_))) => vec![one(v, &path)?],
            _ => {
                return Err(format!(
                    "{path} is missing: the row key (or keys) holding the numbers"
                ));
            }
        };
        if series.len() > 6 {
            return Err(format!("{path}: at most 6 series"));
        }
        if series.len() > 1 && !matches!(kind, ChartKind::Line | ChartKind::Area) {
            return Err(format!(
                "{path}: only line and area charts take several series; give this one a single key"
            ));
        }
        let path = p.at("data");
        let Some(data) = p.any(&["data", "rows"]) else {
            return Err(format!("{path} is missing: a list of rows"));
        };
        let data = self.data(data, &path, &[])?;
        // A key no row has would draw an empty chart.
        let cells = match &data {
            Data::Rows(rows) => rows.first(),
            Data::Generated { row, .. } => Some(row),
        };
        if let Some(cells) = cells {
            for key in std::iter::once(&x).chain(series.iter().map(|s| &s.key)) {
                if !cells.iter().any(|(k, _)| k == key) {
                    let has: Vec<&str> = cells.iter().map(|(k, _)| k.as_str()).collect();
                    return Err(format!(
                        "{path}: rows have no `{key}`; they have: {}",
                        has.join(", ")
                    ));
                }
            }
        } else {
            return Err(format!("{path}: give it at least one row"));
        }
        Ok(Kind::Chart(Chart {
            kind,
            data,
            x,
            series,
            height: p.number("height")?.unwrap_or(220.0).clamp(120.0, 480.0) as f32,
            format: self.format(p)?,
        }))
    }
}

fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
        && !matches!(s, "true" | "false" | "null")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(spec: Value) -> (View, Vec<String>) {
        parse(&spec).unwrap_or_else(|e| panic!("should parse: {e}"))
    }

    fn err(spec: Value) -> String {
        parse(&spec).expect_err("should not parse")
    }

    #[test]
    fn a_view_reads_into_blocks() {
        let (view, notes) = ok(json!({
            "title": "Seats",
            "state": { "price": 12 },
            "body": [
                { "type": "slider", "bind": "seats", "min": 1, "max": 50, "value": 8 },
                { "type": "stat", "label": "Monthly", "value": "{{ usd(seats * price) }}" },
                { "type": "button", "label": "Buy", "send": "Buy {{seats}} seats" },
            ]
        }));
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(view.body.len(), 3);
        // The slider's name joins the state at its starting value.
        assert_eq!(view.state["seats"], json!(8));
        assert_eq!(view.state["price"], json!(12));
        let scope = Scope::of(&view.state);
        let Kind::Stat { value, .. } = &view.body[1].kind else {
            panic!("a stat");
        };
        assert_eq!(value.text(&scope), "$96");
        let Kind::Button {
            action: Action::Send(text),
            ..
        } = &view.body[2].kind
        else {
            panic!("a button that sends");
        };
        assert_eq!(text.text(&scope), "Buy 8 seats");
        // Ids are stable and distinct.
        let ids: Vec<u32> = view.body.iter().map(|b| b.id).collect();
        assert_eq!(ids, [1, 2, 3]);
    }

    #[test]
    fn what_agents_get_wrong_is_named() {
        assert!(err(json!({})).contains("`body` is missing"));
        assert!(err(json!({ "body": [] })).contains("`body` is empty"));
        let e = err(json!({ "body": [{ "type": "sparkle" }] }));
        assert!(
            e.contains("body[0]: unknown block type `sparkle`") && e.contains("types: row,"),
            "{e}"
        );
        assert!(err(json!({ "body": [{ "text": "hi" }] })).contains("needs a `type`"));
        let e = err(json!({ "body": [{ "type": "stat", "label": "A" }] }));
        assert!(e.contains("body[0] (stat).value is missing"), "{e}");
        let e = err(json!({ "body": [{ "type": "text", "text": "{{ seats * 2 }}" }] }));
        assert!(e.contains("`seats` is not in `state`"), "{e}");
        let e = err(json!({
            "state": { "seats": 1 },
            "body": [{ "type": "text", "text": "{{ seatz }}" }]
        }));
        assert!(
            e.contains("`seatz`") && e.contains("state has: seats"),
            "{e}"
        );
        let e = err(json!({ "body": [{ "type": "text", "text": "{{ 1 + }}" }] }));
        assert!(e.contains("body[0] (text).text"), "{e}");
        let e = err(json!({ "body": [{ "type": "badge", "text": "x", "tone": "sparkly" }] }));
        assert!(e.contains("`sparkly` is not one of default, muted"), "{e}");
        let e = err(json!({ "body": [{ "type": "button", "label": "Go" }] }));
        assert!(e.contains("a button needs an action"), "{e}");
        let e = err(json!({ "body": [{ "type": "slider", "bind": "n", "min": 5, "max": 5 }] }));
        assert!(e.contains("must be above `min`"), "{e}");
    }

    #[test]
    fn unknown_properties_are_notes_not_errors() {
        let (_, notes) = ok(json!({
            "colour": "red",
            "body": [{ "type": "badge", "text": "ok", "colour": "green" }]
        }));
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("body[0] (badge): unknown property `colour`"));
        assert!(notes[1].contains("unknown property `colour` on the view"));
    }

    #[test]
    fn plain_meanings_are_taken() {
        // A string where a block goes, other names for blocks, a lone
        // block as the whole view, the view as JSON in a string.
        let (view, _) = ok(
            json!({ "body": ["Hello **there**", { "type": "metric", "label": "A", "value": 1 }] }),
        );
        assert_eq!(view.body[0].kind.name(), "markdown");
        assert_eq!(view.body[1].kind.name(), "stat");
        let (view, _) = ok(json!({ "type": "callout", "text": "Careful" }));
        assert_eq!(view.body[0].kind.name(), "callout");
        let (view, _) = ok(json!(r#"{ "body": [{ "type": "divider" }] }"#));
        assert_eq!(view.body[0].kind.name(), "divider");
        let (view, _) = ok(json!({ "body": { "type": "text", "text": "one block" } }));
        assert_eq!(view.body.len(), 1);
    }

    #[test]
    fn inputs_start_somewhere() {
        let (view, _) = ok(json!({ "body": [
            { "type": "input", "bind": "name" },
            { "type": "input", "bind": "count", "kind": "number", "value": 3 },
            { "type": "select", "bind": "env", "options": ["staging", "production"] },
            { "type": "choice", "bind": "tags", "options": ["a", "b"], "multiple": true },
            { "type": "choice", "bind": "size", "options": [{ "label": "Small", "value": "s" }], "value": "s" },
            { "type": "toggle", "bind": "dry", "label": "Dry run", "value": true },
            { "type": "checkbox", "bind": "ok", "label": "Confirm" },
        ]}));
        assert_eq!(view.state["name"], json!(""));
        assert_eq!(view.state["count"], json!(3));
        assert_eq!(view.state["env"], json!("staging"));
        assert_eq!(view.state["tags"], json!([]));
        assert_eq!(view.state["size"], json!("s"));
        assert_eq!(view.state["dry"], json!(true));
        assert_eq!(view.state["ok"], json!(false));
    }

    #[test]
    fn rows_can_be_generated() {
        let (view, _) = ok(json!({
            "state": { "years": 3, "rate": 10 },
            "body": [{
                "type": "chart", "kind": "line", "x": "year", "y": "balance",
                "data": { "for": "t", "from": 0, "to": "{{years}}",
                          "row": { "year": "Y{{t}}", "balance": "{{ round(1000 * (1 + rate / 100) ^ t) }}" } }
            }]
        }));
        let Kind::Chart(chart) = &view.body[0].kind else {
            panic!("a chart");
        };
        let rows = chart.data.rows(&Scope::of(&view.state));
        let balances: Vec<&Value> = rows.iter().map(|r| &r["balance"]).collect();
        assert_eq!(
            balances,
            [&json!(1000), &json!(1100), &json!(1210), &json!(1331)]
        );
        assert_eq!(rows[2]["year"], json!("Y2"));
        // The range follows the state.
        let mut state = view.state.clone();
        state.insert("years".into(), json!(1));
        assert_eq!(chart.data.rows(&Scope::of(&state)).len(), 2);
        // And is capped.
        state.insert("years".into(), json!(1e9));
        assert_eq!(chart.data.rows(&Scope::of(&state)).len(), MAX_ROWS);
    }

    #[test]
    fn charts_and_tables_check_their_keys() {
        let e = err(json!({ "body": [{
            "type": "chart", "x": "name", "y": "ms", "data": [{ "name": "a", "millis": 3 }]
        }]}));
        // (The order keys are listed in follows how serde_json is built.)
        assert!(
            e.contains("rows have no `ms`; they have: ") && e.contains("millis"),
            "{e}"
        );
        let e = err(json!({ "body": [{
            "type": "chart", "kind": "bar", "x": "n", "y": ["a", "b"], "data": [{ "n": 1, "a": 1, "b": 2 }]
        }]}));
        assert!(
            e.contains("only line and area charts take several series"),
            "{e}"
        );
        let e = err(json!({ "body": [{ "type": "table", "rows": [{ "a": 1 }] }] }));
        assert!(e.contains("columns is missing"), "{e}");
        // Rows as lists follow the columns.
        let (view, _) = ok(json!({ "body": [{
            "type": "table", "columns": ["file", { "key": "size", "format": "bytes", "align": "right" }],
            "rows": [["a.rs", 2048], { "file": "b.rs", "size": 10 }]
        }]}));
        let Kind::Table { columns, rows } = &view.body[0].kind else {
            panic!("a table");
        };
        assert_eq!(columns[1].format, Some(Format::Bytes));
        let rows = rows.rows(&Scope::of(&view.state));
        assert_eq!(rows[0]["size"], json!(2048));
        assert_eq!(rows[1]["file"], json!("b.rs"));
    }

    #[test]
    fn actions_come_in_several_spellings() {
        let (view, _) = ok(json!({
            "state": { "n": 1 },
            "body": [
                { "type": "button", "label": "A", "action": { "send": "Do A" } },
                { "type": "button", "label": "B", "set": { "n": "{{ n + 1 }}", "seen": true } },
                { "type": "button", "label": "C", "action": { "type": "copy", "text": "npm test" } },
                { "type": "button", "label": "D", "open": "src/main.rs" },
                { "type": "text", "text": "{{ seen ? 'yes' : 'no' }}" },
            ]
        }));
        let actions: Vec<&Action> = view
            .body
            .iter()
            .filter_map(|b| match &b.kind {
                Kind::Button { action, .. } => Some(action),
                _ => None,
            })
            .collect();
        assert!(matches!(actions[0], Action::Send(_)));
        assert!(matches!(actions[1], Action::Set(changes) if changes.len() == 2));
        assert!(matches!(actions[2], Action::Copy(_)));
        assert!(matches!(actions[3], Action::Open(_)));
    }

    #[test]
    fn limits_hold() {
        let many: Vec<Value> = (0..MAX_BLOCKS + 1)
            .map(|_| json!({ "type": "divider" }))
            .collect();
        assert!(err(json!({ "body": many })).contains("more than 400 blocks"));
        let mut deep = json!({ "type": "divider" });
        for _ in 0..MAX_DEPTH + 2 {
            deep = json!({ "type": "column", "children": [deep] });
        }
        assert!(err(json!({ "body": [deep] })).contains("nest more than"));
        let big = "x".repeat(MAX_BYTES);
        assert!(err(json!({ "body": [{ "type": "text", "text": big }] })).contains("bytes"));
    }

    #[test]
    fn conditions_take_bare_expressions() {
        let (view, _) = ok(json!({
            "state": { "plan": "pro" },
            "body": [
                { "type": "text", "text": "Pro only", "when": "plan == 'pro'" },
                { "type": "text", "text": "Free only", "when": "{{ plan == 'free' }}" },
            ]
        }));
        let scope = Scope::of(&view.state);
        assert!(view.body[0].when.as_ref().unwrap().truthy(&scope));
        assert!(!view.body[1].when.as_ref().unwrap().truthy(&scope));
    }
}
