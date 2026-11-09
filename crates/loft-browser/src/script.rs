//! JavaScript sources sent to the page. Every script runs through WebKit's
//! `callAsyncJavaScript`, so each is an async function *body*: it can
//! `await`, and it `return`s a JSON string the Rust side parses.

use serde_json::Value;

/// Helpers for snapshot/click/type/press/scroll (see `js/agent.js`).
const AGENT_JS: &str = include_str!("js/agent.js");

/// Serializes any JS value to JSON for `Eval`: DOM nodes, functions,
/// BigInts, Maps, Sets, Errors and cycles become readable placeholders.
const SERIALIZE_JS: &str = r#"
function __loftSerialize(value) {
  const seen = new WeakSet();
  const describeNode = (n) => {
    if (n.nodeType !== 1) return '[' + n.nodeName + ']';
    const html = n.outerHTML || '';
    return html.length > 200 ? html.slice(0, 199) + '…' : html;
  };
  const json = JSON.stringify(value === undefined ? null : value, function (key, v) {
    if (typeof v === 'bigint') return v.toString();
    if (typeof v === 'function') return '[function ' + (v.name || 'anonymous') + ']';
    if (typeof v === 'symbol') return v.toString();
    if (v && typeof v === 'object') {
      if (typeof v.nodeType === 'number' && typeof v.nodeName === 'string') return describeNode(v);
      if (v === v.window) return '[Window]';
      if (seen.has(v)) return '[Circular]';
      seen.add(v);
      if (v instanceof Map) return Object.fromEntries(v);
      if (v instanceof Set) return [...v];
      if (v instanceof Error) return { name: v.name, message: v.message, stack: v.stack };
    }
    return v;
  });
  return json === undefined ? 'null' : json;
}
"#;

/// The helper functions callable through [`helper_call`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Helper {
    Snapshot,
    Click,
    Type,
    Press,
    Scroll,
}

impl Helper {
    fn function(self) -> &'static str {
        match self {
            Helper::Snapshot => "loftSnapshot",
            Helper::Click => "loftClick",
            Helper::Type => "loftType",
            Helper::Press => "loftPress",
            Helper::Scroll => "loftScroll",
        }
    }
}

/// A body that runs one helper with `args` and returns its JSON result.
pub(crate) fn helper_call(helper: Helper, args: &Value) -> String {
    // serde_json output is a valid JS expression (ES2019+ accepts U+2028).
    format!(
        "const ARGS = {args};\n{AGENT_JS}\nreturn JSON.stringify({}(ARGS));\n",
        helper.function()
    )
}

/// Compiles, but never runs, `script` as an expression. It succeeds iff the
/// script parses as one, which picks between [`eval_expression`] and
/// [`eval_function_body`] without running anything twice.
pub(crate) fn eval_probe(script: &str) -> String {
    format!("return 'expression';\nasync function __loftProbe() {{ return (\n{script}\n); }}\n")
}

/// Evaluates `script` as an expression; promises are awaited.
pub(crate) fn eval_expression(script: &str) -> String {
    format!(
        "const __loftValue = await (\n{script}\n);\nreturn __loftSerialize(__loftValue);\n{SERIALIZE_JS}"
    )
}

/// Runs `script` as an async function body; its `return` value is the result.
pub(crate) fn eval_function_body(script: &str) -> String {
    format!(
        "const __loftValue = await (async () => {{\n{script}\n}})();\nreturn __loftSerialize(__loftValue);\n{SERIALIZE_JS}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn helper_call_embeds_args_as_json() {
        let body = helper_call(
            Helper::Type,
            &json!({"ref": 3, "text": "a\"b</script>\u{2028}"}),
        );
        assert!(body.starts_with(r#"const ARGS = {"ref":3,"text":"a\"b</script>"#));
        assert!(body.ends_with("return JSON.stringify(loftType(ARGS));\n"));
        assert!(body.contains("function loftSnapshot(args)"));
    }

    #[test]
    fn every_helper_is_defined() {
        for helper in [
            Helper::Snapshot,
            Helper::Click,
            Helper::Type,
            Helper::Press,
            Helper::Scroll,
        ] {
            let decl = format!("function {}(args)", helper.function());
            assert!(AGENT_JS.contains(&decl), "missing {decl}");
        }
    }

    #[test]
    fn eval_bodies_keep_comments_from_swallowing_code() {
        // A trailing line comment must not comment out the closing paren.
        let body = eval_expression("document.title // the title");
        assert!(body.contains("document.title // the title\n);"));
        let body = eval_function_body("return 1 // one");
        assert!(body.contains("return 1 // one\n})();"));
        let probe = eval_probe("1 // x");
        assert!(probe.starts_with("return 'expression';"));
    }

    /// Compiles every script with JavaScriptCore (via `osascript`) so syntax
    /// errors in `agent.js` fail `cargo test` instead of a live page.
    #[cfg(target_os = "macos")]
    #[test]
    fn scripts_compile_in_javascriptcore() {
        let bodies = [
            ("agent", helper_call(Helper::Snapshot, &json!({}))),
            ("eval-expr", eval_expression("1 + 1")),
            ("eval-body", eval_function_body("return 2;")),
            ("probe", eval_probe("document.title")),
        ];
        for (name, body) in bodies {
            // Wrap like callAsyncJavaScript does and compile without running.
            let source = format!(
                "(function() {{ new Function('return async function() {{' + {} + '}}'); return 'ok'; }})()",
                serde_json::to_string(&body).unwrap()
            );
            let out = match std::process::Command::new("osascript")
                .args(["-l", "JavaScript", "-e", &source])
                .output()
            {
                Ok(out) => out,
                Err(_) => return, // no osascript: nothing to check with
            };
            assert!(
                out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "ok",
                "{name} failed to compile: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        // And the probe rejects statements without running them.
        let source = format!(
            "(function() {{ try {{ new Function('return async function() {{' + {} + '}}'); return 'compiled'; }} catch (e) {{ return e.name; }} }})()",
            serde_json::to_string(&eval_probe("const x = 1; x")).unwrap()
        );
        if let Ok(out) = std::process::Command::new("osascript")
            .args(["-l", "JavaScript", "-e", &source])
            .output()
        {
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "SyntaxError");
        }
    }
}
