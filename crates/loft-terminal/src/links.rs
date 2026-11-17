//! URL detection in terminal rows (⌘-click to open).

/// The URL covering column `col` of `line`, if any. Columns are taken as
/// character positions, which holds for the ASCII text URLs are made of.
pub fn url_at(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let text: String = chars.iter().collect();
    for scheme in ["https://", "http://", "file://"] {
        let mut from = 0;
        while let Some(found) = text[from..].find(scheme) {
            let start_byte = from + found;
            let start = text[..start_byte].chars().count();
            let mut end = start;
            while end < chars.len() && !ends_url(chars[end]) {
                end += 1;
            }
            // Trailing punctuation belongs to the sentence, not the URL.
            while end > start && ".,;:!?'\")]}".contains(chars[end - 1]) {
                // Keep a closing paren that has its opener inside the URL.
                if chars[end - 1] == ')' && chars[start..end].contains(&'(') {
                    break;
                }
                end -= 1;
            }
            if (start..end).contains(&col) && end - start > scheme.len() {
                return Some(chars[start..end].iter().collect());
            }
            from = start_byte + scheme.len();
        }
    }
    None
}

fn ends_url(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | '|')
}

#[cfg(test)]
mod tests {
    use super::url_at;

    #[test]
    fn finds_urls_under_the_column() {
        let line = "  Local:   http://localhost:5173/ (press h for help)";
        assert_eq!(url_at(line, 15).as_deref(), Some("http://localhost:5173/"));
        assert_eq!(url_at(line, 2), None);
        let line = "see https://example.com/a_(b), then go.";
        assert_eq!(
            url_at(line, 8).as_deref(),
            Some("https://example.com/a_(b)")
        );
        let line = "docs: https://ex.com/path.";
        assert_eq!(url_at(line, 10).as_deref(), Some("https://ex.com/path"));
        assert_eq!(url_at("https://", 3), None);
    }
}
