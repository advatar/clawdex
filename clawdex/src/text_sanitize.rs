#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasoningTagKind {
    Final,
    Thinking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReasoningTag {
    kind: ReasoningTagKind,
    is_close: bool,
    len: usize,
}

fn is_ascii_tag_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C)
}

fn is_ascii_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn eq_ascii_case_insensitive(bytes: &[u8], expected_lower: &[u8]) -> bool {
    if bytes.len() != expected_lower.len() {
        return false;
    }
    bytes
        .iter()
        .zip(expected_lower.iter())
        .all(|(a, b)| a.to_ascii_lowercase() == *b)
}

fn parse_reasoning_tag_at(bytes: &[u8], start: usize) -> Option<ReasoningTag> {
    if bytes.get(start) != Some(&b'<') {
        return None;
    }
    let len = bytes.len();
    let mut i = start + 1;
    while i < len && is_ascii_tag_whitespace(bytes[i]) {
        i += 1;
    }
    if i >= len {
        return None;
    }

    let mut is_close = false;
    if bytes[i] == b'/' {
        is_close = true;
        i += 1;
        while i < len && is_ascii_tag_whitespace(bytes[i]) {
            i += 1;
        }
        if i >= len {
            return None;
        }
    }

    let name_start = i;
    while i < len && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    // Emulate JS `\b` after the tag name: next byte must not be a word char.
    if i < len && is_ascii_word_byte(bytes[i]) {
        return None;
    }

    let name = &bytes[name_start..i];
    let kind = if eq_ascii_case_insensitive(name, b"final") {
        ReasoningTagKind::Final
    } else if eq_ascii_case_insensitive(name, b"think")
        || eq_ascii_case_insensitive(name, b"thinking")
        || eq_ascii_case_insensitive(name, b"thought")
        || eq_ascii_case_insensitive(name, b"antthinking")
    {
        ReasoningTagKind::Thinking
    } else {
        return None;
    };

    // Consume the rest of the tag until '>', rejecting nested '<' like the JS `[^<>]*` part.
    while i < len {
        match bytes[i] {
            b'>' => {
                let tag_len = i + 1 - start;
                return Some(ReasoningTag {
                    kind,
                    is_close,
                    len: tag_len,
                });
            }
            b'<' => return None,
            _ => i += 1,
        }
    }
    None
}

fn looks_like_reasoning_tag(text: &str) -> bool {
    // Equivalent to OpenClaw QUICK_TAG_RE:
    // /<\s*\/?\s*(?:think(?:ing)?|thought|antthinking|final)\b/i
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && is_ascii_tag_whitespace(bytes[j]) {
            j += 1;
        }
        if j >= bytes.len() {
            return false;
        }
        if bytes[j] == b'/' {
            j += 1;
            while j < bytes.len() && is_ascii_tag_whitespace(bytes[j]) {
                j += 1;
            }
        }
        let name_start = j;
        while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
            j += 1;
        }
        if j == name_start {
            i += 1;
            continue;
        }
        if j < bytes.len() && is_ascii_word_byte(bytes[j]) {
            i += 1;
            continue;
        }
        let name = &bytes[name_start..j];
        if eq_ascii_case_insensitive(name, b"final")
            || eq_ascii_case_insensitive(name, b"think")
            || eq_ascii_case_insensitive(name, b"thinking")
            || eq_ascii_case_insensitive(name, b"thought")
            || eq_ascii_case_insensitive(name, b"antthinking")
        {
            return true;
        }
        i += 1;
    }
    false
}

/// Strip OpenClaw-style reasoning tags (`<think>...</think>`, `<final>...</final>`) from user-facing
/// text. This matches OpenClaw defaults: strict mode + trim both ends.
///
/// - `<think>` (and variants) are removed along with their content.
/// - `<final>` markup is removed but its content is preserved (by design).
/// - Tags inside fenced/inline code regions are preserved.
pub fn strip_reasoning_tags_from_text(text: &str) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    if !looks_like_reasoning_tag(text) {
        return text.to_string();
    }

    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i: usize = 0;
    let mut line_start = true;
    let mut fenced: Option<u8> = None; // b'`' or b'~'
    let mut inline_code = false;
    let mut thinking = false;

    while i < bytes.len() {
        let b = bytes[i];

        if thinking {
            if b == b'<' {
                if let Some(tag) = parse_reasoning_tag_at(bytes, i) {
                    if tag.kind == ReasoningTagKind::Thinking {
                        thinking = !tag.is_close;
                        // Skip tag bytes without emitting.
                        let last = bytes[i + tag.len - 1];
                        line_start = last == b'\n' || last == b'\r';
                        i += tag.len;
                        continue;
                    }
                }
            }
            // Discard all bytes until we hit a closing thinking tag.
            line_start = b == b'\n' || b == b'\r';
            i += 1;
            continue;
        }

        if let Some(fence_char) = fenced {
            // In fenced code: only exit on a bare closing fence line.
            if line_start
                && i + 3 <= bytes.len()
                && bytes[i] == fence_char
                && bytes[i + 1] == fence_char
                && bytes[i + 2] == fence_char
            {
                let after = i + 3;
                if after == bytes.len() || bytes[after] == b'\n' || bytes[after] == b'\r' {
                    out.extend_from_slice(&bytes[i..after]);
                    i = after;
                    if i < bytes.len() && bytes[i] == b'\r' {
                        out.push(bytes[i]);
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'\n' {
                            out.push(bytes[i]);
                            i += 1;
                        }
                        line_start = true;
                    } else if i < bytes.len() && bytes[i] == b'\n' {
                        out.push(bytes[i]);
                        i += 1;
                        line_start = true;
                    } else {
                        line_start = false;
                    }
                    fenced = None;
                    continue;
                }
            }
            out.push(b);
            line_start = b == b'\n' || b == b'\r';
            i += 1;
            continue;
        }

        if inline_code {
            // In inline code: exit on the next backtick run.
            if b == b'`' {
                while i < bytes.len() && bytes[i] == b'`' {
                    out.push(bytes[i]);
                    i += 1;
                }
                line_start = false;
                inline_code = false;
                continue;
            }
            out.push(b);
            line_start = b == b'\n' || b == b'\r';
            i += 1;
            continue;
        }

        // Start fenced code region only at the start of a line and only when the fence line ends
        // with a newline (OpenClaw's `findCodeRegions` requires `[^\n]*\n`).
        if line_start && i + 3 <= bytes.len() && (&bytes[i..i + 3] == b"```" || &bytes[i..i + 3] == b"~~~") {
            let fence_char = bytes[i];
            let mut j = i + 3;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'\n' {
                out.extend_from_slice(&bytes[i..=j]);
                i = j + 1;
                line_start = true;
                fenced = Some(fence_char);
                continue;
            }
        }

        // Start inline code only if we can find a later closing backtick run. Otherwise treat the
        // backticks as normal text (OpenClaw's inline regex requires a closing delimiter).
        if b == b'`' {
            let start = i;
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            let run_end = i;
            let mut has_closing = false;
            let mut k = run_end;
            while k < bytes.len() {
                if bytes[k] == b'`' {
                    has_closing = true;
                    break;
                }
                k += 1;
            }
            out.extend_from_slice(&bytes[start..run_end]);
            line_start = false;
            if has_closing {
                inline_code = true;
            }
            continue;
        }

        if b == b'<' {
            if let Some(tag) = parse_reasoning_tag_at(bytes, i) {
                match tag.kind {
                    ReasoningTagKind::Final => {
                        let last = bytes[i + tag.len - 1];
                        line_start = last == b'\n' || last == b'\r';
                        i += tag.len;
                        continue;
                    }
                    ReasoningTagKind::Thinking => {
                        thinking = !tag.is_close;
                        let last = bytes[i + tag.len - 1];
                        line_start = last == b'\n' || last == b'\r';
                        i += tag.len;
                        continue;
                    }
                }
            }
        }

        out.push(b);
        line_start = b == b'\n' || b == b'\r';
        i += 1;
    }

    let cleaned = String::from_utf8(out).unwrap_or_default();
    cleaned.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_when_no_tags() {
        let input = "Hello, this is a normal message.";
        assert_eq!(strip_reasoning_tags_from_text(input), input);
    }

    #[test]
    fn strips_think_tags_and_content() {
        let input = "Hello <think>internal reasoning</think> world!";
        assert_eq!(strip_reasoning_tags_from_text(input), "Hello  world!");
    }

    #[test]
    fn strips_multiple_blocks() {
        let input = "<think>first</think>A<think>second</think>B";
        assert_eq!(strip_reasoning_tags_from_text(input), "AB");
    }

    #[test]
    fn preserves_tags_inside_fenced_code_blocks() {
        let input = "Use the tag like this:\n```\n<think>reasoning</think>\n```\nThat's it!";
        assert_eq!(strip_reasoning_tags_from_text(input), input);
    }

    #[test]
    fn preserves_tags_inside_inline_code() {
        let input = "The `<think>` tag is used for reasoning. Don't forget the closing `</think>` tag.";
        assert_eq!(strip_reasoning_tags_from_text(input), input);
    }

    #[test]
    fn strips_real_tags_but_preserves_code_examples() {
        let input = "<think>hidden</think>Visible text with `<think>` example.";
        assert_eq!(
            strip_reasoning_tags_from_text(input),
            "Visible text with `<think>` example."
        );
    }

    #[test]
    fn preserves_fence_at_eof_without_trailing_newline() {
        let input = "Example:\n```\n<think>reasoning</think>\n```";
        assert_eq!(strip_reasoning_tags_from_text(input), input);
    }

    #[test]
    fn strips_final_markup_but_preserves_content() {
        let input = "A<final>1</final>B<final>2</final>C";
        assert_eq!(strip_reasoning_tags_from_text(input), "A1B2C");
    }

    #[test]
    fn preserves_final_tags_in_inline_code() {
        let input = "`<final>` in code, <final>visible</final> outside";
        assert_eq!(
            strip_reasoning_tags_from_text(input),
            "`<final>` in code, visible outside"
        );
    }

    #[test]
    fn mismatched_fence_type_treats_rest_as_code() {
        let input = "```\n<think>not protected\n~~~\n</think>text";
        assert_eq!(strip_reasoning_tags_from_text(input), input);
    }

    #[test]
    fn unclosed_inline_code_does_not_protect_tags() {
        let input = "Start `unclosed <think>hidden</think> end";
        assert_eq!(strip_reasoning_tags_from_text(input), "Start `unclosed  end");
    }
}

