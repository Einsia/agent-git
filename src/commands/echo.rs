//! Human target notices belong to one top-level invocation and its verified selections.

use super::Commands;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Explicit,
    Environment,
    Mixed,
    Interactive,
    Transaction,
}

impl Source {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Explicit => "explicit arguments",
            Self::Environment => "AGIT_SESSION",
            Self::Mixed => "explicit arguments + AGIT_SESSION",
            Self::Interactive => "interactive selection",
            Self::Transaction => "merge transaction",
        }
    }

    /// A parsed argument records the environment dependency before `@` is substituted.
    pub(crate) fn for_spec(spec: &crate::domain::refs::RefSpec) -> Self {
        use crate::domain::refs::{Base, RepoSel};
        match (&spec.repo, &spec.base) {
            (RepoSel::Context, Base::At | Base::SessionBranch(_)) => Self::Environment,
            (RepoSel::Context, _) => Self::Mixed,
            (_, Base::At | Base::SessionBranch(_)) => Self::Mixed,
            _ => Self::Explicit,
        }
    }
}

pub(crate) struct Selection {
    target: String,
    source: Source,
    role: Option<&'static str>,
}

impl Selection {
    /// Callers supply the identity they validated, never an unresolved candidate or cwd guess.
    pub(crate) fn new(target: impl Into<String>, source: Source) -> Self {
        Self {
            target: target.into(),
            source,
            role: None,
        }
    }

    pub(crate) fn role(mut self, role: &'static str) -> Self {
        self.role = Some(role);
        self
    }

    fn render(&self) -> String {
        let target = one_line(&self.target);
        let role = self.role.map(|role| format!("{role}=")).unwrap_or_default();
        format!("{role}{target} (via {})", self.source.label())
    }
}

struct Active {
    command: &'static str,
    emitted: bool,
    enabled: bool,
    legacy: bool,
}

thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}

/// Library helpers, hooks and nested command calls cannot create their own target notice.
pub struct Invocation {
    previous: Option<Active>,
    _thread: PhantomData<Rc<()>>,
}

impl Invocation {
    pub fn enter(command: &Commands, json: bool) -> Self {
        let protocol_child = std::env::var_os("AGIT_PROTOCOL_CHILD").is_some();
        let enabled = !json
            && !protocol_child
            && std::env::var_os("AGIT_QUIET").is_none()
            && human_output(command);
        let active = Some(Active {
            command: super::command_name(command),
            emitted: false,
            enabled,
            legacy: json || protocol_child || std::env::var_os("AGIT_QUIET").is_some(),
        });
        Self {
            previous: ACTIVE.with(|current| current.replace(active)),
            _thread: PhantomData,
        }
    }
}

impl Drop for Invocation {
    fn drop(&mut self) {
        ACTIVE.with(|current| current.replace(self.previous.take()));
    }
}

fn human_output(command: &Commands) -> bool {
    match command {
        Commands::Log(_)
        | Commands::View(_)
        | Commands::Tag(_)
        | Commands::Branch(_)
        | Commands::Fork(_)
        | Commands::Run(_)
        | Commands::Resume(_)
        | Commands::Merge(_)
        | Commands::CherryPick(_)
        | Commands::Revert(_)
        | Commands::Push(_)
        | Commands::Pull(_)
        | Commands::Fetch(_)
        | Commands::Share(_)
        | Commands::Scan(_)
        | Commands::Memory(_)
        | Commands::Distill(_) => true,
        Commands::Commit(args) => !args.from_hook,
        Commands::Show(args) => !args.raw,
        Commands::Diff(args) => !args.files && args.range.is_some(),
        Commands::Export(args) => args.out.as_deref().is_some_and(|path| path != "-"),
        Commands::Pr(args) => matches!(args.cmd, super::pr::Cmd::Create { .. }),
        _ => false,
    }
}

pub(crate) fn emit(command: &str, selections: &[Selection]) -> bool {
    let enabled = ACTIVE.with(|current| {
        let mut current = current.borrow_mut();
        let Some(active) = current.as_mut() else {
            return false;
        };
        if !active.enabled || active.command != command || active.emitted || selections.is_empty() {
            return false;
        }
        active.emitted = true;
        true
    });
    if enabled {
        let targets = selections.iter().map(Selection::render).collect::<Vec<_>>();
        println!("target: {}", targets.join("; "));
    }
    enabled
}

/// Existing serialized or quiet output keeps its text without enabling nested human notices.
pub(crate) fn legacy_output(command: &str) -> bool {
    ACTIVE.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|active| active.command == command && active.legacy)
    })
}

fn one_line(text: &str) -> String {
    let mut output = String::new();
    for character in text.chars() {
        if character.is_control() {
            output.extend(character.escape_debug());
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_controls_cannot_add_lines_or_terminal_sequences() {
        let selection = Selection::new("me/repo@branch\n\u{1b}[31m", Source::Explicit);
        assert_eq!(
            selection.render(),
            "me/repo@branch\\n\\u{1b}[31m (via explicit arguments)"
        );
    }
}
