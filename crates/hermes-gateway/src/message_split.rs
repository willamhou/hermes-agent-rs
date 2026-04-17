//! Utilities for splitting long messages into platform-sized chunks.

/// Split `text` into chunks of at most `max_chars` Unicode scalar values.
///
/// - If the text fits in a single chunk, returns `vec![text.to_owned()]` with
///   no numbering.
/// - Otherwise splits preferring newline boundaries and returns numbered chunks
///   in the form `(1/N) …`, `(2/N) …`, etc.
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    if text.chars().count() <= max_chars {
        return vec![text.to_owned()];
    }

    // Reserve space for the "(NN/NN) " prefix that will be prepended to each chunk.
    // A prefix like "(12/12) " is 9 chars; we reserve 12 to be safe.
    const PREFIX_RESERVE: usize = 12;
    let effective_max = max_chars.saturating_sub(PREFIX_RESERVE);

    // First pass: collect raw chunks, splitting at newlines where possible.
    let mut chunks: Vec<String> = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        let char_count = remaining.chars().count();
        if char_count <= effective_max {
            chunks.push(remaining.to_owned());
            break;
        }

        // Find the byte offset of the effective_max-th character.
        let limit_byte = remaining
            .char_indices()
            .nth(effective_max)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());

        let candidate = &remaining[..limit_byte];

        // Prefer the last newline within the candidate window.
        let split_byte = candidate
            .rfind('\n')
            .map(|pos| pos + 1) // include the '\n' in the current chunk
            .unwrap_or(limit_byte); // fall back to character boundary

        // Guard against an empty split (e.g. text starts with '\n').
        let split_byte = if split_byte == 0 {
            limit_byte
        } else {
            split_byte
        };

        chunks.push(remaining[..split_byte].to_owned());
        remaining = &remaining[split_byte..];
    }

    // Second pass: add (k/N) prefix to every chunk.
    let n = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| format!("({}/{}) {}", i + 1, n, chunk))
        .collect()
}

/// Split `text` for Telegram (max 4000 characters per message).
pub fn split_telegram(text: &str) -> Vec<String> {
    split_message(text, 4000)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_message_no_split() {
        let text = "Hello, world!";
        let chunks = split_message(text, 4000);
        assert_eq!(chunks, vec!["Hello, world!"]);
    }

    #[test]
    fn exact_limit_no_split() {
        let text = "a".repeat(4000);
        let chunks = split_message(&text, 4000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn split_at_newline() {
        // Build a text where a natural split point lands on a newline.
        let line_a = "a".repeat(3000);
        let line_b = "b".repeat(3000);
        let text = format!("{line_a}\n{line_b}");

        let chunks = split_message(&text, 4000);
        assert_eq!(chunks.len(), 2);
        // First chunk should end with the newline and carry the (1/2) prefix.
        assert!(chunks[0].starts_with("(1/2) "));
        assert!(chunks[1].starts_with("(2/2) "));
        // The first chunk content must be ≤ 4000 chars (excluding prefix).
        let first_content: String = chunks[0].chars().skip("(1/2) ".len()).collect();
        assert!(first_content.chars().count() <= 4000);
    }

    #[test]
    fn split_no_newline_char_boundary_fallback() {
        // No newlines → must fall back to hard character boundary.
        let text = "x".repeat(9000);
        let chunks = split_message(&text, 4000);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            // Each chunk (including prefix) must not exceed 4000 + prefix length.
            // The raw content portion must be ≤ 4000 chars.
            let prefix_end = chunk.find(") ").map(|i| i + 2).unwrap_or(0);
            let content: &str = &chunk[prefix_end..];
            assert!(content.chars().count() <= 4000);
        }
    }

    #[test]
    fn multi_chunk_numbering() {
        // Force 3 chunks.
        let text = "z".repeat(12001);
        let chunks = split_message(&text, 4000);
        assert!(chunks.len() >= 3);
        let n = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(chunk.starts_with(&format!("({}/{}) ", i + 1, n)));
        }
    }

    #[test]
    fn empty_message() {
        let chunks = split_message("", 4000);
        assert_eq!(chunks, vec![""]);
    }

    #[test]
    fn unicode_safety_chinese_text() {
        // Each Chinese character is one Unicode scalar but 3 UTF-8 bytes.
        let chinese = "你好世界".repeat(500); // 2000 chars
        let chunks = split_message(&chinese, 4000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chars().count(), 2000);

        // Over limit: 5000 Chinese chars.
        let long_chinese = "中".repeat(5000);
        let chunks = split_message(&long_chinese, 4000);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].starts_with("(1/2) "));
    }

    #[test]
    fn very_long_message_multiple_chunks() {
        let text = "A".repeat(20000);
        let chunks = split_message(&text, 4000);
        // effective_max = 4000 - 12 = 3988; 20000 / 3988 → 6 chunks
        assert!(chunks.len() >= 5);
        let n = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(chunk.starts_with(&format!("({}/{}) ", i + 1, n)));
            let prefix_end = chunk.find(") ").map(|p| p + 2).unwrap_or(0);
            let content = &chunk[prefix_end..];
            // Each chunk's total length (prefix + content) must not exceed max_chars
            assert!(chunk.chars().count() <= 4000);
            // Content alone must fit within effective_max
            assert!(content.chars().count() <= 3988);
        }
    }
}
