//! What Codex says about AgentGit's hooks. Codex runs a user hook only after the person has
//! approved that exact hook, so one that is installed but not yet approved, changed since its
//! approval, or disabled captures nothing and nothing else shows it. Codex keeps the approval as a
//! hash of the hook in its own normalized form; recomputing that hash here would break silently
//! whenever Codex changes the form, so the installed Codex is asked instead, through its app
//! server's `hooks/list`.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long Codex may take to answer before agit stops asking and says nothing about the hooks.
const ANSWER_LIMIT: Duration = Duration::from_secs(10);

/// AgentGit's hooks as Codex lists them for one working directory.
#[derive(Debug, Default)]
pub(crate) struct CodexHooks {
    /// How many AgentGit hooks Codex lists.
    pub(crate) listed: usize,
    /// The AgentGit hooks Codex will not run, with the reason.
    pub(crate) inactive: Vec<(String, &'static str)>,
    /// The events AgentGit hooks for that Codex lists none of AgentGit's hooks for.
    pub(crate) missing: Vec<&'static str>,
}

impl CodexHooks {
    /// One line naming the hooks Codex will not run or does not have, and how to fix that;
    /// `None` when Codex runs an AgentGit hook for every event. A Codex that lists none of them
    /// is the caller's to report: the hooks may never have been set up there.
    pub(crate) fn problem(&self) -> Option<String> {
        let mut said = Vec::new();
        if !self.inactive.is_empty() {
            let hooks = self
                .inactive
                .iter()
                .map(|(event, reason)| format!("{event} ({reason})"))
                .collect::<Vec<_>>()
                .join(", ");
            said.push(format!(
                "Codex will not run these AgentGit hooks: {hooks}. Nothing from Codex is saved until they run: start Codex and approve them in its hooks review, or enable and trust them in Codex's /hooks"
            ));
        }
        // A hook the person deleted leaves the rest approved, and `agit on` in a folder already on
        // does not add it back, so every hook Codex lists can run while one is missing.
        if self.listed > 0 && !self.missing.is_empty() {
            let lost = if self.missing.contains(&"Stop") {
                "no Codex turn here is saved when it finishes"
            } else {
                "a Codex conversation here starts without AgentGit recording it"
            };
            said.push(format!(
                "Codex lists no AgentGit {} hook, so {lost}: add it with `agit setup --runtime codex --hooks`, then approve it in Codex's hooks review",
                self.missing.join(" or ")
            ));
        }
        (!said.is_empty()).then(|| said.join(". "))
    }
}

/// Ask the installed Codex about AgentGit's hooks in `cwd`. `None` when there is no Codex, or it
/// does not answer `hooks/list` in time or in a form agit reads; nothing is claimed then.
pub(crate) fn codex_hooks(cwd: &Path) -> Option<CodexHooks> {
    let codex = crate::adapter::which("codex")?;
    let child = Command::new(codex)
        .arg("app-server")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut server = Server(child);
    let mut input = server.0.stdin.take()?;
    let output = server.0.stdout.take()?;
    let (lines, answers) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + ANSWER_LIMIT;
    let mut send = |message: serde_json::Value| writeln!(input, "{message}").ok();
    send(serde_json::json!({
        "id": 1,
        "method": "initialize",
        "params": {"clientInfo": {"name": "agit", "version": env!("CARGO_PKG_VERSION")}},
    }))?;
    answer(&answers, 1, deadline)?;
    send(serde_json::json!({"method": "initialized"}))?;
    send(serde_json::json!({
        "id": 2,
        "method": "hooks/list",
        "params": {"cwds": [cwd]},
    }))?;
    let listed = answer(&answers, 2, deadline)?;
    read_hooks(&listed)
}

/// The app server lives only as long as one question; dropping it ends the process on every path.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The result of the response with this `id`, skipping notifications and other responses.
fn answer(
    answers: &mpsc::Receiver<String>,
    id: u64,
    deadline: Instant,
) -> Option<serde_json::Value> {
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        let line = answers.recv_timeout(left).ok()?;
        let Ok(mut message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if message.get("id").and_then(|value| value.as_u64()) == Some(id) {
            return message.get_mut("result").map(serde_json::Value::take);
        }
    }
}

/// AgentGit's hooks in a `hooks/list` result: a command hook whose command is one AgentGit writes
/// for Codex. Every other hook is the person's or another tool's and is not reported here.
fn read_hooks(result: &serde_json::Value) -> Option<CodexHooks> {
    let mut hooks = CodexHooks::default();
    let mut seen = std::collections::HashSet::new();
    let mut events = std::collections::HashSet::new();
    for entry in result.get("data")?.as_array()? {
        for hook in entry.get("hooks")?.as_array()? {
            let Some(command) = hook.get("command").and_then(|value| value.as_str()) else {
                continue;
            };
            let Some((event, _)) = super::HOOKS.iter().find(|(_, argv)| {
                super::is_our_hook(command, &super::hook_arguments(argv, Some("codex")))
            }) else {
                continue;
            };
            let key = hook.get("key").and_then(|value| value.as_str());
            if !seen.insert(key.map_or_else(|| command.to_owned(), str::to_owned)) {
                continue;
            }
            hooks.listed += 1;
            events.insert(*event);
            let reason = match hook.get("trustStatus").and_then(|value| value.as_str()) {
                Some("untrusted") => Some("not yet approved"),
                Some("modified") => Some("changed since it was approved"),
                _ if hook.get("enabled").and_then(|value| value.as_bool()) == Some(false) => {
                    Some("disabled")
                }
                _ => None,
            };
            if let Some(reason) = reason {
                hooks.inactive.push(((*event).to_owned(), reason));
            }
        }
    }
    hooks.missing = super::HOOKS
        .iter()
        .map(|(event, _)| *event)
        .filter(|event| !events.contains(event))
        .collect();
    Some(hooks)
}

#[cfg(test)]
mod tests {
    use super::read_hooks;

    /// A Codex that runs AgentGit's SessionStart hook but lists no Stop hook saves no turn, though
    /// every hook it lists is approved: the person deleted one, and the folder stays on. The
    /// problem names the missing hook and the setup command that adds it back. One that judged
    /// only the hooks Codex lists would call this Codex saving.
    #[test]
    fn a_missing_stop_hook_is_a_problem_even_when_the_rest_are_approved() {
        let listing = |hooks: serde_json::Value| {
            read_hooks(&serde_json::json!({"data": [{"cwd": "/w", "hooks": hooks}]})).unwrap()
        };
        let hook = |event: &str, argv: &str| {
            serde_json::json!({"key": format!("/h/.codex/hooks.json:{event}:0:0"),
                "eventName": event, "command": format!("/usr/local/bin/agit hooks {argv} --runtime codex"),
                "enabled": true, "trustStatus": "trusted"})
        };
        let only_start = listing(serde_json::json!([hook("sessionStart", "ingest")]));
        assert_eq!(only_start.listed, 1);
        assert_eq!(only_start.missing, ["Stop"]);
        let problem = only_start
            .problem()
            .expect("a missing Stop hook is a problem");
        assert!(problem.contains("no AgentGit Stop hook"), "{problem}");
        assert!(problem.contains("no Codex turn here is saved"), "{problem}");
        assert!(
            problem.contains("`agit setup --runtime codex --hooks`"),
            "{problem}"
        );

        let both = listing(serde_json::json!([
            hook("sessionStart", "ingest"),
            hook("stop", "settle")
        ]));
        assert!(
            both.missing.is_empty() && both.problem().is_none(),
            "{both:?}"
        );
        // Listing none of them is left to the caller, which knows whether they were set up.
        let none = listing(serde_json::json!([]));
        assert_eq!(none.missing.len(), 2);
        assert!(none.problem().is_none());
    }
}
