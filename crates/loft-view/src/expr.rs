//! The expressions of a view: `{{ seats * price }}`.
//!
//! Small on purpose: numbers, strings, booleans, lists, arithmetic,
//! comparisons, `a ? b : c` and a handful of functions. No loops, no
//! assignment, and nothing to read but the view's own state.

use serde_json::{Map, Value};

/// The longest expression taken (characters).
const MAX_LEN: usize = 2_000;
/// How deep an expression may nest (parentheses, calls, operators).
const MAX_DEPTH: usize = 48;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit(Value),
    Var(String),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    Binary(Op, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(Func, Vec<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Member(Box<Expr>, String),
    List(Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

macro_rules! funcs {
    ($($variant:ident = $name:literal ($min:literal..=$max:literal)),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Func { $($variant),* }

        impl Func {
            /// Every function, by the name it is called by.
            pub const NAMES: &'static [&'static str] = &[$($name),*];

            fn named(name: &str) -> Option<Self> {
                match name { $($name => Some(Func::$variant),)* _ => None }
            }

            fn name(self) -> &'static str {
                match self { $(Func::$variant => $name),* }
            }

            /// How many arguments it takes (fewest, most).
            fn arity(self) -> (usize, usize) {
                match self { $(Func::$variant => ($min, $max)),* }
            }
        }
    };
}

funcs! {
    Min = "min" (1..=16),
    Max = "max" (1..=16),
    Abs = "abs" (1..=1),
    Round = "round" (1..=2),
    Floor = "floor" (1..=1),
    Ceil = "ceil" (1..=1),
    Sqrt = "sqrt" (1..=1),
    Pow = "pow" (2..=2),
    Exp = "exp" (1..=1),
    Ln = "ln" (1..=1),
    Log10 = "log10" (1..=1),
    Clamp = "clamp" (3..=3),
    Len = "len" (1..=1),
    Sum = "sum" (1..=16),
    Avg = "avg" (1..=16),
    If = "if" (3..=3),
    Str = "str" (1..=1),
    Num = "num" (1..=1),
    Upper = "upper" (1..=1),
    Lower = "lower" (1..=1),
    Contains = "contains" (2..=2),
    Join = "join" (1..=2),
    Number = "number" (1..=2),
    Compact = "compact" (1..=1),
    Percent = "percent" (1..=2),
    Usd = "usd" (1..=1),
    Money = "money" (1..=2),
    Bytes = "bytes" (1..=1),
    Duration = "duration" (1..=1),
}

/// What an expression reads from: the view's state, and the loop variable
/// of the rows being generated (innermost last).
#[derive(Clone, Copy)]
pub struct Scope<'a> {
    pub state: &'a Map<String, Value>,
    pub locals: &'a [(String, Value)],
}

impl<'a> Scope<'a> {
    pub fn of(state: &'a Map<String, Value>) -> Self {
        Self { state, locals: &[] }
    }

    fn get(&self, name: &str) -> Value {
        self.locals
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .or_else(|| self.state.get(name).cloned())
            .unwrap_or(Value::Null)
    }
}

impl Expr {
    /// Parse one expression. Errors say what is wrong and where.
    pub fn parse(source: &str) -> Result<Expr, String> {
        if source.chars().count() > MAX_LEN {
            return Err(format!("expression is over {MAX_LEN} characters"));
        }
        let tokens = lex(source)?;
        let mut parser = Parser {
            tokens,
            at: 0,
            depth: 0,
        };
        let expr = parser.ternary()?;
        match parser.peek() {
            None => Ok(expr),
            Some(t) => Err(format!("unexpected `{}` in `{}`", t.show(), source.trim())),
        }
    }

    /// The names this expression reads.
    pub fn variables(&self, out: &mut Vec<String>) {
        match self {
            Expr::Lit(_) => {}
            Expr::Var(name) => out.push(name.clone()),
            Expr::Not(a) | Expr::Neg(a) | Expr::Member(a, _) => a.variables(out),
            Expr::Binary(_, a, b) | Expr::Index(a, b) => {
                a.variables(out);
                b.variables(out);
            }
            Expr::Cond(a, b, c) => {
                a.variables(out);
                b.variables(out);
                c.variables(out);
            }
            Expr::Call(_, args) | Expr::List(args) => args.iter().for_each(|a| a.variables(out)),
        }
    }

    /// Its value. Never fails: what can't be worked out is `null`.
    pub fn eval(&self, scope: &Scope) -> Value {
        match self {
            Expr::Lit(v) => v.clone(),
            Expr::Var(name) => scope.get(name),
            Expr::Not(a) => Value::Bool(!truthy(&a.eval(scope))),
            Expr::Neg(a) => number(&a.eval(scope)).map_or(Value::Null, |n| num_value(-n)),
            Expr::Binary(op, a, b) => binary(*op, a, b, scope),
            Expr::Cond(c, a, b) => {
                if truthy(&c.eval(scope)) {
                    a.eval(scope)
                } else {
                    b.eval(scope)
                }
            }
            Expr::Call(func, args) => call(*func, args, scope),
            Expr::Index(a, i) => match (a.eval(scope), i.eval(scope)) {
                (Value::Array(list), i) => number(&i)
                    .filter(|n| *n >= 0.0)
                    .and_then(|n| list.get(n as usize).cloned())
                    .unwrap_or(Value::Null),
                (Value::Object(map), Value::String(key)) => {
                    map.get(&key).cloned().unwrap_or(Value::Null)
                }
                _ => Value::Null,
            },
            Expr::Member(a, key) => match a.eval(scope) {
                Value::Object(map) => map.get(key).cloned().unwrap_or(Value::Null),
                Value::Array(list) if key == "length" => Value::from(list.len()),
                Value::String(s) if key == "length" => Value::from(s.chars().count()),
                _ => Value::Null,
            },
            Expr::List(items) => Value::Array(items.iter().map(|i| i.eval(scope)).collect()),
        }
    }
}

fn binary(op: Op, a: &Expr, b: &Expr, scope: &Scope) -> Value {
    // Short-circuit: `a && b` is `b` when `a` holds, as in JavaScript.
    match op {
        Op::And => {
            let left = a.eval(scope);
            return if truthy(&left) { b.eval(scope) } else { left };
        }
        Op::Or => {
            let left = a.eval(scope);
            return if truthy(&left) { left } else { b.eval(scope) };
        }
        _ => {}
    }
    let (left, right) = (a.eval(scope), b.eval(scope));
    match op {
        Op::Eq => return Value::Bool(loosely_equal(&left, &right)),
        Op::Ne => return Value::Bool(!loosely_equal(&left, &right)),
        // `+` joins text when either side is text.
        Op::Add if left.is_string() || right.is_string() => {
            return Value::String(format!("{}{}", text(&left), text(&right)));
        }
        Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            if let (Value::String(l), Value::String(r)) = (&left, &right) {
                return Value::Bool(match op {
                    Op::Lt => l < r,
                    Op::Le => l <= r,
                    Op::Gt => l > r,
                    _ => l >= r,
                });
            }
        }
        _ => {}
    }
    let (Some(l), Some(r)) = (number(&left), number(&right)) else {
        return Value::Null;
    };
    match op {
        Op::Add => num_value(l + r),
        Op::Sub => num_value(l - r),
        Op::Mul => num_value(l * r),
        Op::Div => num_value(l / r),
        Op::Rem => num_value(l % r),
        Op::Pow => num_value(l.powf(r)),
        Op::Lt => Value::Bool(l < r),
        Op::Le => Value::Bool(l <= r),
        Op::Gt => Value::Bool(l > r),
        Op::Ge => Value::Bool(l >= r),
        Op::Eq | Op::Ne | Op::And | Op::Or => Value::Null,
    }
}

fn loosely_equal(a: &Value, b: &Value) -> bool {
    match (number(a), number(b)) {
        // 3 and "3" and 3.0 are the same thing to someone writing a view.
        (Some(x), Some(y)) if !a.is_boolean() && !b.is_boolean() => x == y,
        _ => a == b,
    }
}

fn call(func: Func, args: &[Expr], scope: &Scope) -> Value {
    let values: Vec<Value> = args.iter().map(|a| a.eval(scope)).collect();
    // A single list argument stands for its members (`sum(prices)`).
    let spread: Vec<Value> = match values.as_slice() {
        [Value::Array(list)] => list.clone(),
        other => other.to_vec(),
    };
    let numbers = || spread.iter().filter_map(number).collect::<Vec<f64>>();
    let n = |ix: usize| values.get(ix).and_then(number);
    let unary = |f: fn(f64) -> f64| n(0).map_or(Value::Null, |x| num_value(f(x)));
    match func {
        Func::Min => numbers()
            .into_iter()
            .reduce(f64::min)
            .map_or(Value::Null, num_value),
        Func::Max => numbers()
            .into_iter()
            .reduce(f64::max)
            .map_or(Value::Null, num_value),
        Func::Sum => num_value(numbers().iter().sum()),
        Func::Avg => {
            let all = numbers();
            if all.is_empty() {
                Value::Null
            } else {
                num_value(all.iter().sum::<f64>() / all.len() as f64)
            }
        }
        Func::Abs => unary(f64::abs),
        Func::Floor => unary(f64::floor),
        Func::Ceil => unary(f64::ceil),
        Func::Sqrt => unary(f64::sqrt),
        Func::Exp => unary(f64::exp),
        Func::Ln => unary(f64::ln),
        Func::Log10 => unary(f64::log10),
        Func::Round => match (n(0), n(1)) {
            (Some(x), digits) => {
                let scale = 10f64.powi(digits.unwrap_or(0.0).clamp(0.0, 9.0) as i32);
                num_value((x * scale).round() / scale)
            }
            _ => Value::Null,
        },
        Func::Pow => match (n(0), n(1)) {
            (Some(x), Some(y)) => num_value(x.powf(y)),
            _ => Value::Null,
        },
        Func::Clamp => match (n(0), n(1), n(2)) {
            (Some(x), Some(lo), Some(hi)) if lo <= hi => num_value(x.clamp(lo, hi)),
            _ => Value::Null,
        },
        Func::Len => match &values[0] {
            Value::Array(list) => Value::from(list.len()),
            Value::String(s) => Value::from(s.chars().count()),
            Value::Object(map) => Value::from(map.len()),
            _ => Value::from(0),
        },
        Func::If => {
            if truthy(&values[0]) {
                values[1].clone()
            } else {
                values[2].clone()
            }
        }
        Func::Str => Value::String(text(&values[0])),
        Func::Num => n(0).map_or(Value::Null, num_value),
        Func::Upper => Value::String(text(&values[0]).to_uppercase()),
        Func::Lower => Value::String(text(&values[0]).to_lowercase()),
        Func::Contains => Value::Bool(match &values[0] {
            Value::Array(list) => list.iter().any(|v| loosely_equal(v, &values[1])),
            Value::String(s) => s.contains(&text(&values[1])),
            _ => false,
        }),
        Func::Join => match &values[0] {
            Value::Array(list) => {
                let sep = values.get(1).map(text).unwrap_or_else(|| ", ".into());
                Value::String(list.iter().map(text).collect::<Vec<_>>().join(&sep))
            }
            other => Value::String(text(other)),
        },
        Func::Number => n(0).map_or(Value::Null, |x| {
            Value::String(match n(1) {
                Some(digits) => grouped(x, digits.clamp(0.0, 9.0) as usize),
                None => Format::Number.apply(x),
            })
        }),
        Func::Compact => formatted(n(0), Format::Compact),
        Func::Percent => n(0).map_or(Value::Null, |x| {
            Value::String(percent(x, n(1).unwrap_or(0.0).clamp(0.0, 6.0) as usize))
        }),
        Func::Usd => formatted(n(0), Format::Usd),
        Func::Money => n(0).map_or(Value::Null, |x| {
            let symbol = values.get(1).map(text).unwrap_or_else(|| "$".into());
            Value::String(money(x, &symbol))
        }),
        Func::Bytes => formatted(n(0), Format::Bytes),
        Func::Duration => formatted(n(0), Format::Duration),
    }
}

fn formatted(n: Option<f64>, format: Format) -> Value {
    n.map_or(Value::Null, |x| Value::String(format.apply(x)))
}

// -- values ------------------------------------------------------------------

/// A number as JSON: whole numbers stay whole (`8`, not `8.0`), and what
/// isn't a number (a division by zero) is `null`.
pub fn num_value(n: f64) -> Value {
    if !n.is_finite() {
        Value::Null
    } else if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

/// A value read as a number: numbers, numeric text, booleans.
pub fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().replace(',', "").parse().ok(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(list) => !list.is_empty(),
        Value::Object(_) => true,
    }
}

/// A value as the user reads it.
pub fn text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() != 0.0 => trim_zeros(format!("{f:.4}")),
            _ => n.to_string(),
        },
        Value::String(s) => s.clone(),
        Value::Array(list) => list.iter().map(text).collect::<Vec<_>>().join(", "),
        Value::Object(_) => v.to_string(),
    }
}

fn trim_zeros(s: String) -> String {
    if !s.contains('.') {
        return s;
    }
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

// -- number formats ----------------------------------------------------------

/// How a number is written out (a chart's axis, a table column, a slider's
/// value). The same names work as functions in expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// 12,345.5
    #[default]
    Number,
    /// 12.3k
    Compact,
    /// 0.42 → 42%
    Percent,
    /// $1,234.50
    Usd,
    /// 1.2 MB
    Bytes,
    /// Milliseconds → 1m 5s
    Duration,
}

impl Format {
    pub const NAMES: &'static [&'static str] =
        &["number", "compact", "percent", "usd", "bytes", "duration"];

    pub fn named(name: &str) -> Option<Self> {
        Some(match name {
            "number" => Format::Number,
            "compact" => Format::Compact,
            "percent" => Format::Percent,
            "usd" | "currency" | "money" => Format::Usd,
            "bytes" => Format::Bytes,
            "duration" | "ms" => Format::Duration,
            _ => return None,
        })
    }

    pub fn apply(self, n: f64) -> String {
        if !n.is_finite() {
            return "—".into();
        }
        match self {
            Format::Number => {
                let digits = if is_whole(n) {
                    0
                } else if n.abs() < 1.0 {
                    3
                } else {
                    2
                };
                trim_zeros(grouped(n, digits))
            }
            Format::Compact => compact(n),
            Format::Percent => percent(
                n,
                if (n * 100.0).fract().abs() < 1e-9 {
                    0
                } else {
                    1
                },
            ),
            Format::Usd => money(n, "$"),
            Format::Bytes => bytes(n),
            Format::Duration => duration(n),
        }
    }
}

/// `1234567.891` with 2 digits → `1,234,567.89`.
fn grouped(n: f64, digits: usize) -> String {
    // Halves round up (formatting alone rounds them to even: 1234.5 → 1234).
    let scale = 10f64.powi(digits as i32);
    let rounded = (n.abs() * scale).round() / scale;
    let fixed = format!("{rounded:.digits$}");
    let (whole, frac) = fixed.split_once('.').unwrap_or((&fixed, ""));
    let mut out = String::new();
    for (ix, ch) in whole.chars().enumerate() {
        if ix > 0 && (whole.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    // No "-0".
    let zero = out.chars().all(|c| matches!(c, '0' | '.' | ','));
    if n < 0.0 && !zero {
        out.insert(0, '-');
    }
    out
}

fn compact(n: f64) -> String {
    let abs = n.abs();
    let (scaled, unit) = match abs {
        a if a >= 1e12 => (n / 1e12, "T"),
        a if a >= 1e9 => (n / 1e9, "B"),
        a if a >= 1e6 => (n / 1e6, "M"),
        a if a >= 1e3 => (n / 1e3, "k"),
        _ => return Format::Number.apply(n),
    };
    let digits = if scaled.abs() >= 100.0 { 0 } else { 1 };
    format!("{}{unit}", trim_zeros(format!("{scaled:.digits$}")))
}

fn percent(ratio: f64, digits: usize) -> String {
    format!("{}%", grouped(ratio * 100.0, digits))
}

/// Whole, give or take what arithmetic leaves behind (4000.0000000001).
fn is_whole(n: f64) -> bool {
    (n - n.round()).abs() < 1e-6
}

fn money(n: f64, symbol: &str) -> String {
    // No cents when there are none to show.
    let digits = if (n - n.round()).abs() < 0.005 { 0 } else { 2 };
    let body = grouped(n, digits);
    match body.strip_prefix('-') {
        Some(rest) => format!("-{symbol}{rest}"),
        None => format!("{symbol}{body}"),
    }
}

fn bytes(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n;
    let mut unit = 0;
    while value.abs() >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let digits = if unit == 0 || value.abs() >= 100.0 {
        0
    } else {
        1
    };
    format!(
        "{} {}",
        trim_zeros(format!("{value:.digits$}")),
        UNITS[unit]
    )
}

fn duration(ms: f64) -> String {
    let abs = ms.abs();
    if abs < 1_000.0 {
        return format!("{}ms", trim_zeros(format!("{ms:.1}")));
    }
    if abs < 60_000.0 {
        return format!("{}s", trim_zeros(format!("{:.1}", ms / 1_000.0)));
    }
    let secs = (abs / 1_000.0).round() as u64;
    let sign = if ms < 0.0 { "-" } else { "" };
    if secs < 3_600 {
        format!("{sign}{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{sign}{}h {}m", secs / 3_600, secs % 3_600 / 60)
    }
}

// -- lexer -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Num(f64),
    Str(String),
    Ident(String),
    /// An operator or a piece of punctuation.
    Sym(&'static str),
}

impl Token {
    fn show(&self) -> String {
        match self {
            Token::Num(n) => n.to_string(),
            Token::Str(s) => format!("'{s}'"),
            Token::Ident(s) => s.clone(),
            Token::Sym(s) => (*s).to_owned(),
        }
    }
}

const SYMBOLS: [&str; 26] = [
    "===", "!==", "**", "==", "!=", "<=", ">=", "&&", "||", "+", "-", "*", "/", "%", "^", "<", ">",
    "!", "?", ":", "(", ")", "[", "]", ",", ".",
];

fn lex(source: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit()
            || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit))
        {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            // 1e6, 2.5e-3
            if i < chars.len() && matches!(chars[i], 'e' | 'E') {
                let mut j = i + 1;
                if j < chars.len() && matches!(chars[j], '+' | '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].is_ascii_digit() {
                    while j < chars.len() && chars[j].is_ascii_digit() {
                        j += 1;
                    }
                    i = j;
                }
            }
            let raw: String = chars[start..i].iter().filter(|c| **c != '_').collect();
            let n = raw
                .parse::<f64>()
                .map_err(|_| format!("`{raw}` is not a number"))?;
            tokens.push(Token::Num(n));
        } else if c == '"' || c == '\'' {
            let mut out = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err("a string is missing its closing quote".into()),
                    Some(&ch) if ch == c => break,
                    Some('\\') => {
                        i += 1;
                        match chars.get(i) {
                            Some('n') => out.push('\n'),
                            Some('t') => out.push('\t'),
                            Some(&other) => out.push(other),
                            None => return Err("a string is missing its closing quote".into()),
                        }
                    }
                    Some(&ch) => out.push(ch),
                }
                i += 1;
            }
            i += 1;
            tokens.push(Token::Str(out));
        } else if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '$')) {
                i += 1;
            }
            tokens.push(Token::Ident(chars[start..i].iter().collect()));
        } else {
            let rest: String = chars[i..chars.len().min(i + 3)].iter().collect();
            let Some(sym) = SYMBOLS.iter().find(|s| rest.starts_with(**s)) else {
                return Err(format!("unexpected `{c}`"));
            };
            i += sym.chars().count();
            tokens.push(Token::Sym(sym));
        }
    }
    Ok(tokens)
}

// -- parser ------------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn eat(&mut self, sym: &str) -> bool {
        if matches!(self.peek(), Some(Token::Sym(s)) if *s == sym) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn eat_any(&mut self, syms: &[&'static str]) -> Option<&'static str> {
        syms.iter().copied().find(|s| self.eat(s))
    }

    fn expect(&mut self, sym: &str) -> Result<(), String> {
        if self.eat(sym) {
            return Ok(());
        }
        Err(match self.peek() {
            Some(t) => format!("expected `{sym}`, found `{}`", t.show()),
            None => format!("expected `{sym}`, but the expression ends"),
        })
    }

    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, String>) -> Result<T, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("expression is nested too deeply".into());
        }
        let out = f(self);
        self.depth -= 1;
        out
    }

    fn ternary(&mut self) -> Result<Expr, String> {
        self.nested(|p| {
            let cond = p.binary(0)?;
            if !p.eat("?") {
                return Ok(cond);
            }
            let then = p.ternary()?;
            p.expect(":")?;
            let other = p.ternary()?;
            Ok(Expr::Cond(Box::new(cond), Box::new(then), Box::new(other)))
        })
    }

    /// Left-associative binary operators, loosest first.
    fn binary(&mut self, level: usize) -> Result<Expr, String> {
        const LEVELS: [&[(&str, Op)]; 6] = [
            &[("||", Op::Or)],
            &[("&&", Op::And)],
            &[
                ("===", Op::Eq),
                ("!==", Op::Ne),
                ("==", Op::Eq),
                ("!=", Op::Ne),
            ],
            &[("<=", Op::Le), (">=", Op::Ge), ("<", Op::Lt), (">", Op::Gt)],
            &[("+", Op::Add), ("-", Op::Sub)],
            &[("*", Op::Mul), ("/", Op::Div), ("%", Op::Rem)],
        ];
        let Some(ops) = LEVELS.get(level) else {
            return self.unary();
        };
        let mut left = self.binary(level + 1)?;
        loop {
            let syms: Vec<&'static str> = ops.iter().map(|(s, _)| *s).collect();
            let Some(sym) = self.eat_any(&syms) else {
                return Ok(left);
            };
            let op = ops.iter().find(|(s, _)| *s == sym).map(|(_, op)| *op);
            let right = self.binary(level + 1)?;
            left = Expr::Binary(op.unwrap_or(Op::Add), Box::new(left), Box::new(right));
        }
    }

    fn unary(&mut self) -> Result<Expr, String> {
        self.nested(|p| {
            if p.eat("!") {
                return Ok(Expr::Not(Box::new(p.unary()?)));
            }
            if p.eat("-") {
                return Ok(Expr::Neg(Box::new(p.unary()?)));
            }
            if p.eat("+") {
                return p.unary();
            }
            p.power()
        })
    }

    /// `a ^ b` and `a ** b`, right-associative, binding tighter than a
    /// leading minus (`-2 ^ 2` is -4).
    fn power(&mut self) -> Result<Expr, String> {
        let base = self.postfix()?;
        if self.eat_any(&["**", "^"]).is_none() {
            return Ok(base);
        }
        let exponent = self.unary()?;
        Ok(Expr::Binary(Op::Pow, Box::new(base), Box::new(exponent)))
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.primary()?;
        loop {
            if self.eat("[") {
                let index = self.ternary()?;
                self.expect("]")?;
                expr = Expr::Index(Box::new(expr), Box::new(index));
            } else if self.eat(".") {
                match self.tokens.get(self.at).cloned() {
                    Some(Token::Ident(name)) => {
                        self.at += 1;
                        expr = Expr::Member(Box::new(expr), name);
                    }
                    _ => return Err("expected a name after `.`".into()),
                }
            } else {
                return Ok(expr);
            }
        }
    }

    fn list(&mut self, close: &str) -> Result<Vec<Expr>, String> {
        let mut items = Vec::new();
        if self.eat(close) {
            return Ok(items);
        }
        loop {
            items.push(self.ternary()?);
            if self.eat(close) {
                return Ok(items);
            }
            self.expect(",")?;
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let Some(token) = self.tokens.get(self.at).cloned() else {
            return Err("the expression ends too soon".into());
        };
        self.at += 1;
        match token {
            Token::Num(n) => Ok(Expr::Lit(num_value(n))),
            Token::Str(s) => Ok(Expr::Lit(Value::String(s))),
            Token::Sym("(") => {
                let inner = self.ternary()?;
                self.expect(")")?;
                Ok(inner)
            }
            Token::Sym("[") => Ok(Expr::List(self.list("]")?)),
            Token::Ident(name) => match name.as_str() {
                "true" => Ok(Expr::Lit(Value::Bool(true))),
                "false" => Ok(Expr::Lit(Value::Bool(false))),
                "null" => Ok(Expr::Lit(Value::Null)),
                _ if self.eat("(") => {
                    let Some(func) = Func::named(&name) else {
                        return Err(format!(
                            "unknown function `{name}`; functions: {}",
                            Func::NAMES.join(", ")
                        ));
                    };
                    let args = self.list(")")?;
                    let (min, max) = func.arity();
                    if args.len() < min || args.len() > max {
                        return Err(format!(
                            "`{}` takes {} argument(s), got {}",
                            func.name(),
                            if min == max {
                                min.to_string()
                            } else {
                                format!("{min} to {max}")
                            },
                            args.len()
                        ));
                    }
                    Ok(Expr::Call(func, args))
                }
                _ => Ok(Expr::Var(name)),
            },
            other => Err(format!("unexpected `{}`", other.show())),
        }
    }
}

// -- templates ---------------------------------------------------------------

/// Text with `{{ expressions }}` in it. A template that is one expression
/// and nothing else keeps that expression's value (a number stays a number).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Template {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq)]
enum Part {
    Text(String),
    Expr(Expr),
}

impl Template {
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut rest = source;
        while let Some(open) = rest.find("{{") {
            let Some(len) = rest[open + 2..].find("}}") else {
                return Err(format!("`{{{{` without `}}}}` in `{source}`"));
            };
            if open > 0 {
                parts.push(Part::Text(rest[..open].to_owned()));
            }
            let inner = &rest[open + 2..open + 2 + len];
            parts.push(Part::Expr(Expr::parse(inner)?));
            rest = &rest[open + 2 + len + 2..];
        }
        if !rest.is_empty() {
            parts.push(Part::Text(rest.to_owned()));
        }
        Ok(Self { parts })
    }

    /// A value as it is (a number or boolean given where text may go).
    pub fn value(v: Value) -> Self {
        match v {
            Value::String(s) => Self {
                parts: vec![Part::Text(s)],
            },
            other => Self {
                parts: vec![Part::Expr(Expr::Lit(other))],
            },
        }
    }

    /// One bare expression (`when`: `plan == 'pro'`).
    pub fn expr(expr: Expr) -> Self {
        Self {
            parts: vec![Part::Expr(expr)],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Whether anything in it is worked out (it can change with the state).
    pub fn is_dynamic(&self) -> bool {
        self.parts
            .iter()
            .any(|p| matches!(p, Part::Expr(e) if !matches!(e, Expr::Lit(_))))
    }

    pub fn variables(&self, out: &mut Vec<String>) {
        for part in &self.parts {
            if let Part::Expr(e) = part {
                e.variables(out);
            }
        }
    }

    pub fn eval(&self, scope: &Scope) -> Value {
        match self.parts.as_slice() {
            [] => Value::String(String::new()),
            [Part::Expr(e)] => e.eval(scope),
            _ => Value::String(self.text(scope)),
        }
    }

    pub fn text(&self, scope: &Scope) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Text(t) => out.push_str(t),
                Part::Expr(e) => out.push_str(&text(&e.eval(scope))),
            }
        }
        out
    }

    pub fn number(&self, scope: &Scope) -> Option<f64> {
        number(&self.eval(scope))
    }

    pub fn truthy(&self, scope: &Scope) -> bool {
        truthy(&self.eval(scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn eval(source: &str, state: Value) -> Value {
        let state = state.as_object().cloned().unwrap_or_default();
        Expr::parse(source).unwrap().eval(&Scope::of(&state))
    }

    #[test]
    fn arithmetic_follows_the_usual_order() {
        assert_eq!(eval("1 + 2 * 3", json!({})), json!(7));
        assert_eq!(eval("(1 + 2) * 3", json!({})), json!(9));
        assert_eq!(eval("2 ^ 3 ^ 2", json!({})), json!(512));
        assert_eq!(eval("2 ** 10", json!({})), json!(1024));
        assert_eq!(eval("-2 ^ 2", json!({})), json!(-4));
        assert_eq!(eval("7 % 4", json!({})), json!(3));
        assert_eq!(eval("10 / 4", json!({})), json!(2.5));
        assert_eq!(eval("1e3 + .5", json!({})), json!(1000.5));
    }

    #[test]
    fn state_drives_the_result() {
        let state = json!({ "seats": 8, "price": 12.5, "plan": "pro" });
        assert_eq!(eval("seats * price", state.clone()), json!(100));
        assert_eq!(
            eval("plan == 'pro' ? seats * 2 : seats", state.clone()),
            json!(16)
        );
        assert_eq!(
            eval("'Plan: ' + upper(plan)", state.clone()),
            json!("Plan: PRO")
        );
        assert_eq!(eval("seats > 5 && plan != 'free'", state), json!(true));
    }

    #[test]
    fn what_cannot_be_worked_out_is_null() {
        assert_eq!(eval("missing * 2", json!({})), Value::Null);
        assert_eq!(eval("1 / 0", json!({})), Value::Null);
        assert_eq!(eval("sqrt(-1)", json!({})), Value::Null);
        assert_eq!(eval("items[9]", json!({ "items": [1] })), Value::Null);
    }

    #[test]
    fn lists_and_members() {
        let state = json!({ "rows": [{ "n": 2 }, { "n": 5 }], "tags": ["a", "b"] });
        assert_eq!(eval("rows[1].n", state.clone()), json!(5));
        assert_eq!(eval("len(rows)", state.clone()), json!(2));
        assert_eq!(eval("tags.length", state.clone()), json!(2));
        assert_eq!(eval("contains(tags, 'b')", state.clone()), json!(true));
        assert_eq!(eval("join(tags, ' + ')", state), json!("a + b"));
        assert_eq!(eval("sum([1, 2, 3.5])", json!({})), json!(6.5));
        assert_eq!(eval("max(3, 9, 4)", json!({})), json!(9));
        assert_eq!(eval("avg([2, 4])", json!({})), json!(3));
    }

    #[test]
    fn numbers_are_written_for_people() {
        assert_eq!(
            eval("number(1234567.891)", json!({})),
            json!("1,234,567.89")
        );
        assert_eq!(eval("number(1234.5, 0)", json!({})), json!("1,235"));
        assert_eq!(eval("usd(1234.5)", json!({})), json!("$1,234.50"));
        assert_eq!(eval("usd(-99)", json!({})), json!("-$99"));
        assert_eq!(eval("money(1500, '€')", json!({})), json!("€1,500"));
        assert_eq!(eval("percent(0.256, 1)", json!({})), json!("25.6%"));
        assert_eq!(eval("percent(0.5)", json!({})), json!("50%"));
        assert_eq!(eval("compact(1234)", json!({})), json!("1.2k"));
        assert_eq!(eval("compact(2500000)", json!({})), json!("2.5M"));
        assert_eq!(eval("bytes(1536)", json!({})), json!("1.5 KB"));
        assert_eq!(eval("bytes(900)", json!({})), json!("900 B"));
        assert_eq!(eval("duration(850)", json!({})), json!("850ms"));
        assert_eq!(eval("duration(12300)", json!({})), json!("12.3s"));
        assert_eq!(eval("duration(125000)", json!({})), json!("2m 5s"));
        assert_eq!(eval("round(2.346, 2)", json!({})), json!(2.35));
        assert_eq!(eval("round(2.5)", json!({})), json!(3));
        assert_eq!(text(&json!(0.1 + 0.2)), "0.3");
        assert_eq!(Format::Number.apply(-0.0001), "0");
        // What arithmetic leaves behind doesn't show as cents or decimals.
        assert_eq!(Format::Usd.apply(4_000.000_000_000_5), "$4,000");
        assert_eq!(Format::Number.apply(0.1 * 3.0 * 10.0), "3");
        assert_eq!(Format::Usd.apply(19.999), "$20");
        assert_eq!(Format::Usd.apply(19.5), "$19.50");
    }

    #[test]
    fn mistakes_are_named() {
        let err = |s: &str| Expr::parse(s).unwrap_err();
        assert!(err("fmt(3)").contains("unknown function `fmt`"));
        assert!(err("round()").contains("takes 1 to 2"));
        assert!(err("1 +").contains("ends too soon"));
        assert!(err("(1 + 2").contains("expected `)`"));
        assert!(err("a b").contains("unexpected `b`"));
        assert!(err("'open").contains("closing quote"));
        assert!(err(&"(".repeat(200)).contains("too deeply"));
        assert!(err("1 # 2").contains("unexpected `#`"));
    }

    #[test]
    fn templates_mix_text_and_values() {
        let state = json!({ "seats": 8, "price": 12 });
        let state = state.as_object().unwrap();
        let scope = Scope::of(state);
        let t = Template::parse("{{seats}} seats cost {{ usd(seats * price) }}").unwrap();
        assert_eq!(t.text(&scope), "8 seats cost $96");
        assert!(t.is_dynamic());
        // One expression alone keeps its type.
        let one = Template::parse("{{ seats * price }}").unwrap();
        assert_eq!(one.eval(&scope), json!(96));
        assert_eq!(one.number(&scope), Some(96.0));
        let plain = Template::parse("No braces here").unwrap();
        assert!(!plain.is_dynamic());
        assert_eq!(plain.text(&scope), "No braces here");
        assert!(Template::parse("{{ seats").is_err());
        let mut vars = Vec::new();
        t.variables(&mut vars);
        assert_eq!(vars, ["seats", "seats", "price"]);
    }

    #[test]
    fn the_loop_variable_shadows_state() {
        let state = json!({ "t": 1, "rate": 10 });
        let state = state.as_object().unwrap();
        let locals = [("t".to_owned(), json!(3))];
        let scope = Scope {
            state,
            locals: &locals,
        };
        assert_eq!(Expr::parse("t * rate").unwrap().eval(&scope), json!(30));
    }
}
