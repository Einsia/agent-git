//! Metadata previews retain bounded text while transcript storage remains authoritative.

pub(crate) const SESSION_PREVIEW_CHARS: usize = 80;

/// Whitespace is collapsed without allocating the complete prompt. Truncation preserves
/// Unicode scalar boundaries and reports omitted content with an ellipsis.
pub(crate) fn shorten(text: &str, max: usize) -> String {
    let mut output = String::new();
    let mut count = 0;
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = count != 0;
            continue;
        }
        if space {
            if count == max {
                output.push('…');
                return output;
            }
            output.push(' ');
            count += 1;
            space = false;
        }
        if count == max {
            output.push('…');
            return output;
        }
        output.push(ch);
        count += 1;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_preserves_normalized_prefix_and_marks_only_omitted_content() {
        for text in ["", " \n\t", "\n alpha\t beta \n", "alpha beta gamma"] {
            let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
            for max in 0..=normalized.chars().count() + 1 {
                let mut expected: String = normalized.chars().take(max).collect();
                if normalized.chars().count() > max {
                    expected.push('…');
                }
                assert_eq!(shorten(text, max), expected);
            }
        }
    }

    #[test]
    fn long_unicode_prompts_produce_bounded_metadata() {
        // Unicode truncation fixture exercises multibyte characters and separators.
        let prompt = "\u{4f60}\u{597d}\u{2003}".repeat(1 << 18);
        let preview = shorten(&prompt, SESSION_PREVIEW_CHARS);
        assert_eq!(preview.chars().count(), SESSION_PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
        assert!(preview.len() <= (SESSION_PREVIEW_CHARS + 1) * 4);
    }
}
