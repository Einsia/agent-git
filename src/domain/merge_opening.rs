//! The opening message `agit merge` sends its merge agent.
//!
//! The runtime records that message as an ordinary user prompt, and the published LOG keeps it
//! byte for byte. Its first line is therefore a marker naming the merge, so a reader of the LOG
//! can show the turn as "merged <from> into <into>" and leave the protocol text that follows
//! unrendered. Only a message that starts with the marker line counts: a prompt that quotes the
//! marker later on stays the user's own text.
//!
//! Everything in the message is published with the target, so it names the source only as far
//! as [`Source`] allows, and never carries the local transaction marker.

use serde::{Deserialize, Serialize};

/// Starts the first line of every opening message; a JSON [`Opening`] follows on that line.
pub const MARKER: &str = "agit-merge-agent ";

/// What the merge agent was started to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Opening {
    /// The source as the target's readers may see it; absent when the source is withheld.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The target branch, `owner/repo@branch`.
    pub into: String,
}

/// How far the merged conversation may be named in the target's published history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A branch of the target's own repository: its readers can already open it.
    Same {
        branch: String,
        title: Option<String>,
    },
    /// A branch of another repository that is public on the Hub.
    Public {
        repository: String,
        branch: String,
        title: Option<String>,
    },
    /// Another repository that is private, unpublished, or whose visibility could not be read.
    Withheld,
}

impl Source {
    /// The `from` of the marker line.
    pub fn from(&self) -> Option<String> {
        match self {
            Source::Same { branch, .. } => Some(branch.clone()),
            Source::Public {
                repository, branch, ..
            } => Some(format!("{repository}@{branch}")),
            Source::Withheld => None,
        }
    }

    /// The sentence the message opens its body with.
    pub fn sentence(&self) -> String {
        let titled = |name: String, title: &Option<String>| match title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(title) => format!("Merged `{name}` (\"{title}\")."),
            None => format!("Merged `{name}`."),
        };
        match self {
            Source::Same { title, .. } | Source::Public { title, .. } => {
                titled(self.from().unwrap_or_default(), title)
            }
            Source::Withheld => "Merged another conversation.".to_owned(),
        }
    }
}

/// The full opening message: the marker line, then `body`.
pub fn render(opening: &Opening, body: &str) -> String {
    let line = serde_json::to_string(opening).expect("strings always serialize");
    format!("{MARKER}{line}\n{body}")
}

/// The merge a prompt opens, when its text is an opening message.
pub fn recognize(text: &str) -> Option<Opening> {
    let line = text.strip_prefix(MARKER)?.lines().next()?;
    let opening: Opening = serde_json::from_str(line).ok()?;
    let named = |s: &str| !s.trim().is_empty();
    (opening.from.as_deref().is_none_or(named) && named(&opening.into)).then_some(opening)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader recovers both sides from the message alone, and only from its first line: a
    /// recogniser that searched the text would turn a user's prompt quoting the marker into a
    /// merge event and hide what they wrote. A withheld source is still a merge.
    #[test]
    fn only_a_leading_marker_line_names_a_merge() {
        let opening = Opening {
            from: Some("me/repo@try \"quoted\"".into()),
            into: "me/repo@main".into(),
        };
        let text = render(&opening, "Reconcile the branch.\n");
        assert_eq!(recognize(&text), Some(opening.clone()));
        assert_eq!(recognize(&format!("Look at this:\n{text}")), None);
        assert_eq!(recognize(&format!(" {text}")), None);
        let withheld = render(
            &Opening {
                from: None,
                into: "me/repo@main".into(),
            },
            "",
        );
        assert_eq!(withheld, format!("{MARKER}{{\"into\":\"me/repo@main\"}}\n"));
        assert_eq!(recognize(&withheld).unwrap().from, None);
        assert_eq!(
            recognize(&format!("{MARKER}{{\"from\":\"\",\"into\":\"x\"}}\n")),
            None
        );
        assert_eq!(
            recognize(&format!(
                "{MARKER}{{\"from\":\"a\",\"into\":\"b\",\"extra\":1}}\n"
            )),
            None
        );
    }
}
