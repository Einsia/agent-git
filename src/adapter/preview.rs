//! Metadata previews retain bounded text while transcript storage remains authoritative.

pub(crate) const SESSION_PREVIEW_CHARS: usize = 80;

/// How the Hub's line naming another person's words begins. The Hub escapes it inside anyone's
/// words, so words that begin with it carry the Hub's own line.
pub(crate) const HUB_SENDER_LINE: &str = "[AgentGit: this message is from ";

/// How this device's own line naming a workspace member as a message's sender begins, put ahead
/// of their words when the Hub's line doesn't name them.
pub(crate) const DEVICE_SENDER_LINE: &str = "[AgentGit workspace message from @";

/// `text` past the lines naming a colleague as its sender, the Hub's and this device's, when it
/// begins with them, so a title or gist summarizes the person's words themselves.
pub(crate) fn without_sender_line(mut text: &str) -> &str {
    while text.starts_with(HUB_SENDER_LINE) || text.starts_with(DEVICE_SENDER_LINE) {
        text = text.split_once('\n').map_or("", |(_, words)| words);
    }
    text
}

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

    /// A gist starts at a colleague's words, past the Hub's line and this device's naming them;
    /// words that only mention such a line later, or have none, are summarized as they are.
    #[test]
    fn a_summary_skips_the_hubs_sender_line() {
        let named = "[AgentGit: this message is from Carol Wu @carol via Feishu, not from the owner of this computer.]\nFix the retry path";
        assert_eq!(without_sender_line(named), "Fix the retry path");
        assert_eq!(
            shorten(without_sender_line(named), SESSION_PREVIEW_CHARS),
            "Fix the retry path"
        );
        let labelled = "[AgentGit workspace message from @carol]\nFix the retry path";
        assert_eq!(without_sender_line(labelled), "Fix the retry path");
        let both = "[AgentGit workspace message from @carol]\n[AgentGit: this message is from @boss via Feishu.]\nFix the retry path";
        assert_eq!(without_sender_line(both), "Fix the retry path");
        let quoted = "see [AgentGit: this message is from @boss]\nok";
        assert_eq!(without_sender_line(quoted), quoted);
        assert_eq!(without_sender_line("Fix it"), "Fix it");
        assert_eq!(
            without_sender_line("[AgentGit: this message is from @carol]"),
            ""
        );
    }

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
