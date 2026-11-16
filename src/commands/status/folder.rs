//! What `agit status` answers first in a folder bound to a repository: whether the folder's
//! conversations there are all uploaded, and for each one that isn't, why and what to run (#498,
//! the first step of the status design 246). Its verdict is the session rows' upload cell, so the
//! answer and the rows never disagree.

use super::{UploadsNotThrough, inspect_command, ran_in, said_at, sessions};
use crate::domain::link;
use std::path::Path;

/// How many conversations that aren't uploaded are named; the rest are counted.
const NAMED: usize = 5;

pub(super) struct FolderAnswer {
    pub repository: String,
    /// Whether every session record on this computer was read, and was the same after the rows
    /// had read theirs. When not, one of the folder's conversations may be missing from the list,
    /// so the answer says it can't tell instead of calling the folder uploaded.
    pub complete: bool,
    pub conversations: Vec<FolderConversation>,
}

pub(super) struct FolderConversation {
    /// `owner/repo@branch`.
    pub target: String,
    /// The rows' upload cell: `uploaded`, `waiting`, `not uploaded`, or `—` when it can't be read.
    pub upload: String,
    /// How many saved turns the Hub hasn't received; `None` when local evidence can't tell.
    pub unreceived_turns: Option<u32>,
    /// Why it isn't uploaded, and the command that shows or does the rest.
    pub why: Option<String>,
    pub next: Option<String>,
}

/// The answer for the folder `cwd` is in, when that folder is bound to a repository. Only the
/// bound repository's conversations from this folder count, newest first. `complete` is the
/// rows' verdict on the session records ([`sessions::Page::inventory_complete`]); without it
/// every upload cell reads as unknown, as the rows' do.
pub(super) fn answer(
    cwd: &Path,
    links: &[link::Link],
    complete: bool,
    not_through: &UploadsNotThrough,
    upload_off: &[String],
) -> Option<FolderAnswer> {
    let root = crate::commands::front::folder(cwd).ok()?;
    let repository = crate::commands::front::bound(&root)?;
    let mut claims: Vec<&link::Link> = links
        .iter()
        .filter(|link| link.is_active() && ran_in(link, &root))
        .filter(|link| {
            link.branch.is_some()
                && matches!((&link.owner, &link.agent), (Some(owner), Some(name))
                    if format!("{owner}/{name}") == repository)
        })
        .collect();
    claims.sort_by_key(|link| std::cmp::Reverse(link.settled_at));
    let cells = if complete {
        sessions::upload_cells(&claims)
    } else {
        vec![("—".to_owned(), None); claims.len()]
    };
    let waits = crate::domain::privacy::pending::upload::load();
    let reader = crate::commands::project::Reader::of_this_process();
    let conversations = claims
        .iter()
        .zip(cells)
        .map(|(claim, (upload, unreceived_turns))| {
            let target = format!(
                "{repository}@{}",
                claim.branch.as_deref().unwrap_or_default()
            );
            let push = || Some(format!("`{}`", inspect_command(&target)));
            let failed = not_through
                .items
                .iter()
                .find(|(failed, _)| *failed == target)
                .map(|(_, undone)| *undone);
            let (why, next) = match upload.as_str() {
                "uploaded" => (None, None),
                _ if failed.is_some() => (
                    failed.map(|undone| {
                        undone.describe_for(&target, reader, said_at(&target).as_deref())
                    }),
                    push(),
                ),
                _ if waits.contains_key(&target) => (
                    Some(format!(
                        "its upload waits for the privacy check ({})",
                        waits[&target].reason
                    )),
                    None,
                ),
                _ if upload_off.contains(&repository) => (
                    Some(
                        "automatic upload is off for it here: its turns are saved on this computer"
                            .into(),
                    ),
                    push(),
                ),
                "waiting" => (Some("the Hub has an older copy of it".into()), push()),
                "not uploaded" => (Some("the Hub has no copy of it yet".into()), push()),
                _ => (
                    Some("whether the Hub has it can't be read here".into()),
                    None,
                ),
            };
            FolderConversation {
                target,
                upload,
                unreceived_turns,
                why,
                next,
            }
        })
        .collect();
    Some(FolderAnswer {
        repository,
        complete,
        conversations,
    })
}

impl FolderAnswer {
    fn not_uploaded(&self) -> impl Iterator<Item = &FolderConversation> {
        self.conversations
            .iter()
            .filter(|conversation| conversation.upload != "uploaded")
    }

    /// The answer as text, first in `agit status`.
    pub(super) fn print(&self) {
        let total = self.conversations.len();
        let missing: Vec<_> = self.not_uploaded().collect();
        if !self.complete {
            println!(
                "Can't tell whether this folder's conversations are all uploaded to {}: some session records on this computer can't be read.",
                self.repository
            );
        } else if total == 0 {
            println!(
                "No conversation from this folder is saved to {} yet.",
                self.repository
            );
        } else if missing.is_empty() {
            println!(
                "This folder's {} uploaded to {}.",
                if total == 1 {
                    "conversation is".to_owned()
                } else {
                    format!("{total} conversations are all")
                },
                self.repository
            );
        } else {
            println!(
                "Not uploaded yet ({} of this folder's {total} in {}):",
                missing.len(),
                self.repository
            );
            for conversation in missing.iter().take(NAMED) {
                let counted = match conversation.unreceived_turns {
                    Some(1) => "1 saved turn not yet uploaded; ".to_owned(),
                    Some(turns) if turns > 1 => format!("{turns} saved turns not yet uploaded; "),
                    _ => String::new(),
                };
                println!(
                    "  {}: {counted}{}",
                    conversation.target,
                    conversation.why.as_deref().unwrap_or("not uploaded")
                );
                if let Some(next) = &conversation.next {
                    println!("    → {next}");
                }
            }
            if missing.len() > NAMED {
                println!(
                    "  and {} more: `agit status --repo {}` lists them",
                    missing.len() - NAMED,
                    self.repository
                );
            }
        }
        println!();
    }

    /// The answer for `status --json`, beside the fields that were there before.
    pub(super) fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "repository": self.repository,
            "complete": self.complete,
            "all_uploaded": self.complete.then(|| self.not_uploaded().next().is_none()),
            "conversations": self.conversations.iter().map(|conversation| serde_json::json!({
                "target": conversation.target,
                "upload": sessions::upload_json(&conversation.upload),
                "uploaded": sessions::upload_json(&conversation.upload)
                    .map(|upload| upload == "uploaded"),
                "unreceived_turns": conversation.unreceived_turns,
                "why": conversation.why,
                "next": conversation.next,
            })).collect::<Vec<_>>(),
        })
    }
}
