use regex::Regex;
use std::sync::OnceLock;

// -- Regex patterns (compiled once) --

fn bold_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").unwrap())
}

fn italic_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // No look-around needed: bold (**) is matched first and its spans are
    // excluded via overlaps_any, so this only catches single-star italic.
    RE.get_or_init(|| Regex::new(r"\*([^*]+?)\*").unwrap())
}

fn strikethrough_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"~~(.+?)~~").unwrap())
}

fn inline_code_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`([^`]+?)`").unwrap())
}

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap())
}

fn header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^#{1,6}\s+(.+)$").unwrap())
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == "---" || trimmed == "***" || trimmed == "___"
}

fn is_table_line(line: &str) -> bool {
    line.starts_with('|')
}

// -- Telegram MarkdownV2 conversion --

const TELEGRAM_SPECIAL: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
];

/// Convert GitHub-flavored markdown to Telegram MarkdownV2 format.
pub fn to_telegram(text: &str) -> String {
    let mut output = Vec::new();
    let mut in_code_block = false;
    let mut table_buffer: Vec<&str> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();

        // Code block toggle
        if trimmed.starts_with("```") {
            if !table_buffer.is_empty() {
                flush_table_telegram(&table_buffer, &mut output);
                table_buffer.clear();
            }
            in_code_block = !in_code_block;
            output.push(line.to_string());
            continue;
        }

        // Inside code block — emit verbatim
        if in_code_block {
            output.push(line.to_string());
            continue;
        }

        // Table lines
        if is_table_line(trimmed) {
            table_buffer.push(line);
            continue;
        }

        // Flush any pending table
        if !table_buffer.is_empty() {
            flush_table_telegram(&table_buffer, &mut output);
            table_buffer.clear();
        }

        // Horizontal rules — pass through escaped
        if is_horizontal_rule(trimmed) {
            output.push("\\-\\-\\-".to_string());
            continue;
        }

        // Headers → bold
        if let Some(caps) = header_re().captures(trimmed) {
            let heading_text = caps.get(1).unwrap().as_str();
            let escaped = telegram_escape_plain(heading_text);
            output.push(format!("*{escaped}*"));
            continue;
        }

        // Regular line — convert inline formatting then escape
        output.push(convert_telegram_line(line));
    }

    // Flush any trailing table
    if !table_buffer.is_empty() {
        flush_table_telegram(&table_buffer, &mut output);
    }

    output.join("\n")
}

/// Escape special characters for Telegram MarkdownV2 (plain text regions only).
fn telegram_escape_plain(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for ch in text.chars() {
        if TELEGRAM_SPECIAL.contains(&ch) {
            result.push('\\');
        }
        result.push(ch);
    }
    result
}

/// Escape special characters inside URLs for Telegram MarkdownV2.
/// Only `)` and `\` need escaping inside URL parentheses.
fn telegram_escape_url(url: &str) -> String {
    let mut result = String::with_capacity(url.len());
    for ch in url.chars() {
        if ch == ')' || ch == '\\' {
            result.push('\\');
        }
        result.push(ch);
    }
    result
}

/// Convert a regular line: identify protected spans (formatting, inline code, links)
/// and escape everything else.
fn convert_telegram_line(line: &str) -> String {
    // Identify protected regions using a span-based approach.
    // We mark byte ranges as "protected" (formatting markers, code, links)
    // and escape everything else.

    let mut spans: Vec<(usize, usize, String)> = Vec::new();

    // Inline code — highest priority, preserved as-is
    for m in inline_code_re().find_iter(line) {
        spans.push((m.start(), m.end(), m.as_str().to_string()));
    }

    // Links — text escaped, URL has `)` and `\` escaped
    for caps in link_re().captures_iter(line) {
        let full = caps.get(0).unwrap();
        if !overlaps_any(&spans, full.start(), full.end()) {
            let link_text = caps.get(1).unwrap().as_str();
            let url = caps.get(2).unwrap().as_str();
            let escaped_text = telegram_escape_plain(link_text);
            let escaped_url = telegram_escape_url(url);
            spans.push((
                full.start(),
                full.end(),
                format!("[{escaped_text}]({escaped_url})"),
            ));
        }
    }

    // Bold **text** → *text*
    for caps in bold_re().captures_iter(line) {
        let full = caps.get(0).unwrap();
        if !overlaps_any(&spans, full.start(), full.end()) {
            let inner = caps.get(1).unwrap().as_str();
            let escaped = telegram_escape_plain(inner);
            spans.push((full.start(), full.end(), format!("*{escaped}*")));
        }
    }

    // Italic *text* → _text_
    for caps in italic_re().captures_iter(line) {
        let full = caps.get(0).unwrap();
        if !overlaps_any(&spans, full.start(), full.end()) {
            let inner = caps.get(1).unwrap().as_str();
            let escaped = telegram_escape_plain(inner);
            spans.push((full.start(), full.end(), format!("_{escaped}_")));
        }
    }

    // Strikethrough ~~text~~ → ~text~
    for caps in strikethrough_re().captures_iter(line) {
        let full = caps.get(0).unwrap();
        if !overlaps_any(&spans, full.start(), full.end()) {
            let inner = caps.get(1).unwrap().as_str();
            let escaped = telegram_escape_plain(inner);
            spans.push((full.start(), full.end(), format!("~{escaped}~")));
        }
    }

    // Sort spans by start position
    spans.sort_by_key(|s| s.0);

    // Build output: protected spans as-is, gaps escaped
    let mut result = String::new();
    let mut pos = 0;

    for (start, end, replacement) in &spans {
        if *start > pos {
            result.push_str(&telegram_escape_plain(&line[pos..*start]));
        }
        result.push_str(replacement);
        pos = *end;
    }

    if pos < line.len() {
        result.push_str(&telegram_escape_plain(&line[pos..]));
    }

    result
}

fn overlaps_any(spans: &[(usize, usize, String)], start: usize, end: usize) -> bool {
    spans
        .iter()
        .any(|(s, e, _)| start < *e && end > *s)
}

fn flush_table_telegram(table: &[&str], output: &mut Vec<String>) {
    output.push("```".to_string());
    for line in table {
        output.push(line.to_string());
    }
    output.push("```".to_string());
}

// -- Discord conversion --

/// Convert GitHub-flavored markdown to Discord-compatible format.
///
/// Discord already supports most GH markdown, so this mainly handles
/// headers (→ bold), tables (→ code blocks), and drops horizontal rules.
pub fn to_discord(text: &str) -> String {
    let mut output = Vec::new();
    let mut in_code_block = false;
    let mut table_buffer: Vec<&str> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();

        // Code block toggle
        if trimmed.starts_with("```") {
            if !table_buffer.is_empty() {
                flush_table_discord(&table_buffer, &mut output);
                table_buffer.clear();
            }
            in_code_block = !in_code_block;
            output.push(line.to_string());
            continue;
        }

        // Inside code block — emit verbatim
        if in_code_block {
            output.push(line.to_string());
            continue;
        }

        // Table lines
        if is_table_line(trimmed) {
            table_buffer.push(line);
            continue;
        }

        // Flush any pending table
        if !table_buffer.is_empty() {
            flush_table_discord(&table_buffer, &mut output);
            table_buffer.clear();
        }

        // Drop horizontal rules
        if is_horizontal_rule(trimmed) {
            continue;
        }

        // Headers → bold
        if let Some(caps) = header_re().captures(trimmed) {
            let heading_text = caps.get(1).unwrap().as_str();
            output.push(format!("**{heading_text}**"));
            continue;
        }

        // Everything else — pass through
        output.push(line.to_string());
    }

    // Flush any trailing table
    if !table_buffer.is_empty() {
        flush_table_discord(&table_buffer, &mut output);
    }

    output.join("\n")
}

fn flush_table_discord(table: &[&str], output: &mut Vec<String>) {
    output.push("```".to_string());
    for line in table {
        output.push(line.to_string());
    }
    output.push("```".to_string());
}

#[cfg(test)]
mod test_to_telegram {
    use super::*;

    #[test]
    fn converts_bold() {
        assert_eq!(to_telegram("**bold**"), "*bold*");
    }

    #[test]
    fn converts_italic() {
        assert_eq!(to_telegram("*italic*"), "_italic_");
    }

    #[test]
    fn converts_strikethrough() {
        assert_eq!(to_telegram("~~strike~~"), "~strike~");
    }

    #[test]
    fn preserves_code_block_content() {
        let input = "```\nfoo.bar! + baz\n```";
        let result = to_telegram(input);
        assert!(result.contains("foo.bar! + baz"));
        // Inside code block — no escaping
        assert!(!result.contains("foo\\.bar"));
    }

    #[test]
    fn preserves_inline_code() {
        let result = to_telegram("Use `foo.bar()` here");
        assert!(result.contains("`foo.bar()`"));
    }

    #[test]
    fn converts_header_to_bold() {
        assert_eq!(to_telegram("# Heading"), "*Heading*");
        assert_eq!(to_telegram("## Sub Heading"), "*Sub Heading*");
    }

    #[test]
    fn converts_table_to_code_block() {
        let input = "| A | B |\n|---|---|\n| 1 | 2 |";
        let result = to_telegram(input);
        assert!(result.starts_with("```"));
        assert!(result.ends_with("```"));
        assert!(result.contains("| A | B |"));
    }

    #[test]
    fn escapes_special_characters() {
        let result = to_telegram("Hello. World! 1+2=3");
        assert!(result.contains(r"Hello\."));
        assert!(result.contains(r"World\!"));
        assert!(result.contains(r"1\+2\=3"));
    }

    #[test]
    fn no_escaping_inside_code() {
        let input = "```\nspecial: . ! + = | # > -\n```";
        let result = to_telegram(input);
        assert!(result.contains("special: . ! + = | # > -"));
    }

    #[test]
    fn preserves_links() {
        let result = to_telegram("[click here](https://example.com)");
        assert!(result.contains("[click here](https://example.com)"));
    }

    #[test]
    fn nested_bold_with_inline_code() {
        let result = to_telegram("**bold with `code` inside**");
        assert!(result.contains("*bold with "));
        assert!(result.contains("`code`"));
    }

    #[test]
    fn escapes_horizontal_rule() {
        let result = to_telegram("before\n---\nafter");
        assert!(result.contains("\\-\\-\\-"));
    }

    #[test]
    fn link_with_special_url() {
        let result = to_telegram("[article](https://en.wikipedia.org/wiki/Foo_(bar))");
        assert!(result.contains("\\)"));
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(to_telegram(""), "");
    }
}

#[cfg(test)]
mod test_to_discord {
    use super::*;

    #[test]
    fn drops_horizontal_rules() {
        let result = to_discord("before\n---\nafter");
        assert!(!result.contains("---"));
        assert!(result.contains("before"));
        assert!(result.contains("after"));
    }

    #[test]
    fn converts_table_to_code_block() {
        let input = "| A | B |\n|---|---|\n| 1 | 2 |";
        let result = to_discord(input);
        assert!(result.starts_with("```"));
        assert!(result.ends_with("```"));
    }

    #[test]
    fn converts_header_to_bold() {
        assert_eq!(to_discord("# Heading"), "**Heading**");
        assert_eq!(to_discord("### Sub"), "**Sub**");
    }

    #[test]
    fn passes_through_bold_italic() {
        assert_eq!(to_discord("**bold** and *italic*"), "**bold** and *italic*");
    }

    #[test]
    fn passes_through_code_blocks() {
        let input = "```rust\nfn main() {}\n```";
        assert_eq!(to_discord(input), input);
    }

    #[test]
    fn preserves_links() {
        let input = "[click](https://example.com)";
        assert_eq!(to_discord(input), input);
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(to_discord(""), "");
    }
}
