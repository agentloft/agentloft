//! Views to try things with: the test agent shows them (`ui:tests`,
//! `ui:all`, `ui:stream:pricing`), and the tests check each one is a view
//! the tool would take.

use serde_json::{Value, json};

/// Each sample's name, its title, and its page.
const ALL: &[(&str, &str, &str)] = &[
    (
        "tests",
        "Test run · cargo test --workspace",
        include_str!("samples/tests.html"),
    ),
    (
        "pricing",
        "Team plan estimate",
        include_str!("samples/pricing.html"),
    ),
    (
        "approach",
        "Three ways to make search faster",
        include_str!("samples/approach.html"),
    ),
    ("orbit", "Moons", include_str!("samples/orbit.html")),
];

/// The samples' names.
pub fn names() -> Vec<&'static str> {
    ALL.iter().map(|(name, _, _)| *name).collect()
}

/// A sample as the arguments of a `show_ui` call.
pub fn get(name: &str) -> Option<Value> {
    ALL.iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, title, html)| json!({ "title": title, "html": html }))
}

pub fn all() -> Vec<Value> {
    ALL.iter().filter_map(|(name, _, _)| get(name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sample_is_a_view() {
        for name in names() {
            let view = crate::parse(&get(name).unwrap()).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(view.title.is_some(), "{name}");
            // Samples load nothing: they must work with no network at all.
            assert!(!view.html.contains("https://"), "{name} loads something");
        }
        assert_eq!(all().len(), names().len());
        assert!(get("nope").is_none());
    }
}
