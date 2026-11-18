//! `browse.<name>(args)` → one [`BrowserCommand`], with no table of
//! commands: the name becomes the serde `action` tag and the arguments its
//! fields, so a command added to `BrowserCommand` later works at once.

use loft_page_script::BrowserCommand;
use serde_json::{Map, Value};

/// The command a `browse.<name>(args)` call stands for, and the tab it
/// names (`None`: the script's own tab). `args` is the call's argument as
/// JSON (`null` when none was given). Errors are written for the agent.
pub(crate) fn command(
    name: &str,
    args: &Value,
) -> Result<(Option<String>, BrowserCommand), String> {
    let action = action(name);
    if action.is_empty() {
        return Err(
            "browse needs a command name after the dot, e.g. browse.snapshot() or browse.click({ref: 3})"
                .into(),
        );
    }
    if action == "execute" {
        return Err(format!(
            "browse.{name}: a script can't start another script; call the browser commands directly"
        ));
    }
    let mut fields = match args {
        Value::Null => Map::new(),
        Value::Object(map) => map.clone(),
        _ => {
            return Err(format!(
                "browse.{name} takes one object of arguments, e.g. browse.{name}({{ref: 3}})"
            ));
        }
    };
    fields.retain(|_, value| !value.is_null());
    let tab = match fields.remove("tab") {
        None => None,
        Some(Value::String(tab)) => Some(tab),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(_) => {
            return Err(format!(
                "browse.{name}: `tab` must be a tab id (a string, as browser_execute's `tab:` line gives it)"
            ));
        }
    };
    let mut wire = Map::new();
    for (key, value) in fields {
        let key = match snake_case(&key).as_str() {
            "ref" => "ref_id".to_owned(),
            other => other.to_owned(),
        };
        let value = if key == "ref_id" || key.ends_with("_ref_id") {
            ref_number(value)
        } else {
            value
        };
        wire.insert(key, value);
    }
    wire.insert("action".into(), Value::String(action.clone()));
    match serde_json::from_value::<BrowserCommand>(Value::Object(wire)) {
        Ok(command) => Ok((tab, command)),
        Err(error) => Err(explain(name, &action, &error.to_string())),
    }
}

/// `browser_waitFor` → `wait_for`.
pub(crate) fn action(name: &str) -> String {
    snake_case(name.strip_prefix("browser_").unwrap_or(name))
}

/// `waitFor` → `wait_for`, `WaitFor` → `wait_for`; snake_case stays.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            if !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// A ref written as the snapshot shows it (`"[ref=12]"`, `"ref=12"`,
/// `"12"`) becomes the number; anything else is left for the command to
/// judge.
fn ref_number(value: Value) -> Value {
    let Value::String(text) = &value else {
        return value;
    };
    let inner = text.trim();
    let inner = inner
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(inner)
        .trim();
    let inner = inner.strip_prefix("ref=").unwrap_or(inner).trim();
    match inner.parse::<u64>() {
        Ok(n) if !inner.is_empty() && inner.bytes().all(|b| b.is_ascii_digit()) => Value::from(n),
        _ => value,
    }
}

/// A deserialisation error, for the agent: an unknown command lists the
/// ones there are.
fn explain(name: &str, action: &str, error: &str) -> String {
    let unknown = format!("unknown variant `{action}`");
    if let Some(rest) = error.strip_prefix(&unknown) {
        let known = rest
            .split_once("expected one of ")
            .map(|(_, list)| list)
            .or_else(|| rest.split_once("expected ").map(|(_, list)| list))
            .unwrap_or_default()
            .replace('`', "");
        return format!(
            "browse.{name} is not supported: this Agentloft has no browser command \"{action}\". \
             The commands are: {known}"
        );
    }
    format!("browse.{name}: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_page_script::Target;
    use serde_json::json;

    #[test]
    fn names_become_actions() {
        assert_eq!(action("waitFor"), "wait_for");
        assert_eq!(action("browser_wait_for"), "wait_for");
        assert_eq!(action("WaitFor"), "wait_for");
        assert_eq!(action("browser_click"), "click");
        assert_eq!(action("screenshot_diff"), "screenshot_diff");
        assert_eq!(action("networkStart"), "network_start");
    }

    #[test]
    fn arguments_become_fields() {
        let (tab, cmd) = command("click", &json!({"ref": "[ref=3]", "tab": "p1"})).unwrap();
        assert_eq!(tab.as_deref(), Some("p1"));
        assert_eq!(
            serde_json::to_value(&cmd).unwrap(),
            json!({"action": "click", "ref_id": 3})
        );

        let (tab, cmd) = command(
            "browser_type",
            &json!({"refId": 2, "text": "hi", "submit": null}),
        )
        .unwrap();
        assert_eq!(tab, None);
        assert_eq!(
            cmd,
            BrowserCommand::Type {
                target: Target::by_ref(2),
                text: "hi".into(),
                submit: false,
                insert: false,
            }
        );

        // Commands added later need no table: `paste` maps as it is.
        let (_, cmd) = command(
            "paste",
            &json!({"selector": "#body", "text": "One", "html": "<p>One</p>"}),
        )
        .unwrap();
        assert_eq!(
            cmd,
            BrowserCommand::Paste {
                target: Target::by_selector("#body"),
                text: "One".into(),
                html: Some("<p>One</p>".into()),
            }
        );

        let (tab, cmd) = command("snapshot", &Value::Null).unwrap();
        assert_eq!(tab, None);
        assert_eq!(cmd, BrowserCommand::Snapshot { within: None });

        // A numeric tab is its decimal string; `ref=` forms are numbers.
        let (tab, cmd) = command("hover", &json!({"ref_id": "ref=12", "tab": 4})).unwrap();
        assert_eq!(tab.as_deref(), Some("4"));
        assert_eq!(
            serde_json::to_value(&cmd).unwrap(),
            json!({"action": "hover", "ref_id": 12})
        );

        // Nested objects are left as they are.
        let (_, cmd) = command(
            "drag",
            &json!({"from": {"ref_id": 1}, "to": {"x": 10, "y": 20}, "steps": 3}),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&cmd).unwrap(),
            json!({"action": "drag", "from": {"ref_id": 1}, "to": {"x": 10.0, "y": 20.0}, "steps": 3})
        );

        let (_, cmd) = command("emulate", &json!({"colorScheme": "dark"})).unwrap();
        assert_eq!(
            serde_json::to_value(&cmd).unwrap(),
            json!({"action": "emulate", "color_scheme": "dark"})
        );
    }

    #[test]
    fn mistakes_are_explained() {
        let unknown = command("teleport", &json!({"ref": 1})).unwrap_err();
        assert!(
            unknown.starts_with("browse.teleport is not supported"),
            "{unknown}"
        );
        assert!(
            unknown.contains("navigate") && !unknown.contains('`'),
            "{unknown}"
        );

        let missing = command("type", &json!({})).unwrap_err();
        assert!(missing.contains("missing field `text`"), "{missing}");
        assert!(missing.starts_with("browse.type: "), "{missing}");

        assert!(command("execute", &json!({"script": "1"})).is_err());
        assert!(command("click", &json!(3)).is_err());
        assert!(command("", &Value::Null).is_err());
        assert!(command("browser_", &Value::Null).is_err());
        let tab = command("click", &json!({"ref": 1, "tab": true})).unwrap_err();
        assert!(tab.contains("`tab` must be a tab id"), "{tab}");
    }
}
