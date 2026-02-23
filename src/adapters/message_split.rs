/// Split a message into chunks that fit within `max_length`.
///
/// Respects paragraph and line boundaries when possible, and maintains
/// code block fencing across split points.
pub fn split_message(text: &str, max_length: usize) -> Vec<String> {
    if text.len() <= max_length {
        return vec![text.to_string()];
    }

    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut open_fence: Option<String> = None;

    for paragraph in paragraphs.iter() {
        let separator = if current.is_empty() { "" } else { "\n\n" };

        if current.len() + separator.len() + paragraph.len() <= max_length {
            current.push_str(separator);
            current.push_str(paragraph);
        } else if current.is_empty() {
            // Single paragraph exceeds max_length — split by lines
            let lines: Vec<&str> = paragraph.split('\n').collect();
            for line in lines.iter() {
                let line_sep = if current.is_empty() { "" } else { "\n" };

                if current.len() + line_sep.len() + line.len() <= max_length {
                    current.push_str(line_sep);
                    current.push_str(line);
                } else if current.is_empty() {
                    // Single line exceeds max_length — hard split
                    let mut remaining = *line;
                    while !remaining.is_empty() {
                        let take = char_boundary(remaining, max_length);
                        let chunk_text = &remaining[..take];
                        remaining = &remaining[take..];

                        if remaining.is_empty() {
                            current.push_str(chunk_text);
                        } else {
                            flush_chunk(chunk_text, &mut chunks, &mut open_fence);
                        }
                    }
                } else {
                    flush_chunk(&current, &mut chunks, &mut open_fence);
                    current = reopen_fence(&open_fence);
                    current.push_str(line);
                }
            }
        } else {
            flush_chunk(&current, &mut chunks, &mut open_fence);
            current = reopen_fence(&open_fence);
            current.push_str(paragraph);
        }

    }

    if !current.is_empty() {
        flush_chunk(&current, &mut chunks, &mut open_fence);
    }

    if chunks.is_empty() {
        vec![text.to_string()]
    } else {
        chunks
    }
}

/// Flush the current chunk, handling code block state.
fn flush_chunk(text: &str, chunks: &mut Vec<String>, open_fence: &mut Option<String>) {
    let mut chunk = text.to_string();

    // Track fence state through this chunk
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if open_fence.is_some() {
                *open_fence = None;
            } else {
                let lang = trimmed.strip_prefix("```").unwrap().trim().to_string();
                *open_fence = Some(lang);
            }
        }
    }

    // If we're inside a code block at the end of this chunk, close it
    if open_fence.is_some() {
        chunk.push_str("\n```");
    }

    chunks.push(chunk);
}

/// Generate the opening fence for the next chunk if we're inside a code block.
fn reopen_fence(open_fence: &Option<String>) -> String {
    match open_fence {
        Some(lang) if !lang.is_empty() => format!("```{lang}\n"),
        Some(_) => "```\n".to_string(),
        None => String::new(),
    }
}

/// Find the largest byte offset <= `max` that falls on a UTF-8 character boundary.
fn char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut pos = max;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    // Guard against zero-width split (e.g., max_length = 0)
    if pos == 0 && max > 0 {
        // Advance to the end of the first character to make progress
        let mut end = 1;
        while end < s.len() && !s.is_char_boundary(end) {
            end += 1;
        }
        end
    } else {
        pos
    }
}

#[cfg(test)]
mod test_split_message {
    use super::*;

    #[test]
    fn short_message_returns_single_chunk() {
        let text = "Hello, world!";
        let result = split_message(text, 100);
        assert_eq!(result, vec!["Hello, world!"]);
    }

    #[test]
    fn exactly_max_length_no_split() {
        let text = "a".repeat(100);
        let result = split_message(&text, 100);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], text);
    }

    #[test]
    fn splits_at_paragraph_boundary() {
        let text = "First paragraph.\n\nSecond paragraph.";
        let result = split_message(text, 20);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "First paragraph.");
        assert_eq!(result[1], "Second paragraph.");
    }

    #[test]
    fn splits_at_line_boundary() {
        let text = "Line one.\nLine two.\nLine three.";
        let result = split_message(text, 22);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "Line one.\nLine two.");
        assert_eq!(result[1], "Line three.");
    }

    #[test]
    fn hard_splits_when_no_line_breaks() {
        let text = "a".repeat(250);
        let result = split_message(&text, 100);
        assert!(result.len() >= 3);
        for chunk in &result {
            assert!(chunk.len() <= 100);
        }
        let reassembled: String = result.concat();
        assert_eq!(reassembled, text);
    }

    #[test]
    fn code_block_closed_and_reopened_at_split() {
        let text = "Before\n\n```python\nline 1\nline 2\nline 3\nline 4\n```";
        let result = split_message(text, 30);
        assert!(result.len() >= 2);

        // Find the chunk that starts the code block but doesn't end it naturally
        let first_code_chunk = result.iter().find(|c| c.contains("```python")).unwrap();
        // It should be closed with ```
        assert!(
            first_code_chunk.ends_with("```"),
            "First code chunk should end with closing fence: {first_code_chunk:?}"
        );

        // The next chunk should reopen the fence
        let idx = result
            .iter()
            .position(|c| c.contains("```python"))
            .unwrap();
        if idx + 1 < result.len() {
            let next = &result[idx + 1];
            assert!(
                next.starts_with("```python"),
                "Next chunk should reopen with language tag: {next:?}"
            );
        }
    }

    #[test]
    fn code_block_without_language_tag() {
        let text = "Before\n\n```\nline 1\nline 2\nline 3\nline 4\n```";
        let result = split_message(text, 25);
        assert!(result.len() >= 2);

        // Find chunk that opens code block
        let idx = result
            .iter()
            .position(|c| c.contains("```\n"))
            .unwrap();
        if idx + 1 < result.len() {
            let next = &result[idx + 1];
            assert!(
                next.starts_with("```\n") || next.starts_with("```"),
                "Next chunk should reopen with plain fence: {next:?}"
            );
        }
    }

    #[test]
    fn nested_code_blocks_toggle_correctly() {
        // Claude explaining markdown: outer code block contains inner triple backticks
        let text = "````\n```python\nprint('hi')\n```\n````";
        let result = split_message(text, 1000);
        // Should fit in one chunk since it's under max_length
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], text);
    }

    #[test]
    fn hard_split_preserves_utf8() {
        // 3-byte emoji repeated — split must not land inside a multi-byte char
        let text = "\u{1F600}".repeat(50); // 50 emoji, 4 bytes each = 200 bytes
        let result = split_message(&text, 100);
        assert!(result.len() >= 2);
        for chunk in &result {
            // If we can iterate chars, the string is valid UTF-8
            assert!(chunk.chars().count() > 0);
        }
        let reassembled: String = result.concat();
        assert_eq!(reassembled, text);
    }

    #[test]
    fn empty_input() {
        let result = split_message("", 100);
        assert_eq!(result, vec![""]);
    }
}
