//! `agit rc start --here`: make this computer reachable through Cloud on the Hub the person
//! signed in to, and say plainly when it is not. Once it is online, and after one question, it
//! binds the folder as a project of this computer's workspace there, so the sessions started in it
//! from then on upload to a new private repository of the person's. It uploads nothing that is
//! already there.

use crate::rc::control::{CloudLine, CloudPresence};
use anyhow::Result;
use std::path::Path;

/// How long `--here` waits for this computer's presence on the Hub.
pub(super) const PRESENCE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum HubChoice {
    Use(String),
    /// `AGIT_HUB_URL` names another Hub than the one the person signed in to.
    Mismatch {
        signed_in: String,
        environment: String,
    },
}

/// The Hub `--here` acts on: the one `agit login --hub` recorded in the config. An
/// `AGIT_HUB_URL` that names another Hub is refused while the recorded one holds a sign-in, so
/// Cloud access is never switched on for a Hub the person did not sign in to (#306). Without a
/// sign-in on the recorded Hub, `AGIT_HUB_URL` is the only choice there is, and the sign-in that
/// follows is made there.
pub(super) fn choose_hub(
    configured: &str,
    environment: Option<&str>,
    signed_in: impl Fn(&str) -> bool,
) -> HubChoice {
    let normal = |hub: &str| origin(hub);
    let configured = normal(configured);
    match environment.map(normal) {
        Some(environment) if environment != configured && signed_in(&configured) => {
            HubChoice::Mismatch {
                signed_in: configured,
                environment,
            }
        }
        Some(environment) => HubChoice::Use(environment),
        None => HubChoice::Use(configured),
    }
}

/// `hub` as the Cloud client names it (scheme and host lowercased, no trailing slash): the key
/// the daemon reports its Cloud state under. A value the client cannot parse is only trimmed.
pub(super) fn origin(hub: &str) -> String {
    agit_peer::client::Client::new(hub.trim())
        .map(|client| client.origin().to_owned())
        .unwrap_or_else(|_| hub.trim().trim_end_matches('/').to_owned())
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Online,
    /// Not online on this Hub; `elsewhere` lists the Hubs this computer is online on instead.
    NotOnline {
        class: String,
        elsewhere: Vec<String>,
    },
    /// The running daemon says nothing about Cloud: it predates the per-Hub state.
    Unknown,
}

/// Failures that no amount of waiting repairs.
fn final_failure(class: Option<&str>) -> bool {
    matches!(class, Some("not_known_to_this_hub" | "hub_without_cloud"))
}

/// Waits for this computer's presence on `hub`. `ask` reads the daemon's Cloud state (`None`
/// when it did not answer); `requested` says whether this run's inbound request still waits for
/// the daemon; `pause` waits a moment and returns false once the time is up. Online counts only
/// once the daemon has taken the request and serves the Hub with an enrollment that is no longer
/// pending, so a connection left from before the request (another account's) is never success.
/// Each look checks the request before it asks, and only an answer fetched after the request
/// was seen consumed decides Online: a snapshot from before, or a look whose query failed, never
/// does.
pub(super) fn wait(
    hub: &str,
    mut ask: impl FnMut() -> Option<Vec<CloudLine>>,
    mut requested: impl FnMut() -> bool,
    mut pause: impl FnMut() -> bool,
) -> Outcome {
    let mut seen = false;
    let mut last = Vec::new();
    let mut consumed = false;
    loop {
        // Checked before the question, so a fresh answer postdates the daemon taking the request.
        consumed = consumed || !requested();
        let fresh = match ask() {
            Some(lines) => {
                seen |= !lines.is_empty();
                last = lines;
                consumed
            }
            None => false,
        };
        let here = last.iter().find(|line| line.hub == hub);
        if fresh
            && here.is_some_and(|line| {
                line.presence == CloudPresence::Connected && line.enrolled && !line.pending
            })
        {
            return Outcome::Online;
        }
        let failed = here.is_some_and(|line| {
            line.presence == CloudPresence::Failed && final_failure(line.error.as_deref())
        });
        if failed || !pause() {
            break;
        }
    }
    if !seen {
        return Outcome::Unknown;
    }
    let here = last.iter().find(|line| line.hub == hub);
    let class = match here {
        None => "not_enrolled",
        Some(line) if !line.enrolled && !line.pending => "not_enrolled",
        Some(line) => line.error.as_deref().unwrap_or("still_connecting"),
    };
    Outcome::NotOnline {
        class: class.to_owned(),
        elsewhere: last
            .iter()
            .filter(|line| line.hub != hub && line.presence == CloudPresence::Connected)
            .map(|line| line.hub.clone())
            .collect(),
    }
}

pub(super) fn outcome_line(hub: &str, outcome: &Outcome) -> String {
    let retry = format!(
        "Check `agit rc cloud status --hub {hub}`, then `agit rc stop` and `agit rc start --here`."
    );
    match outcome {
        Outcome::Online => format!("Online on {hub}."),
        // The wrong Hub leads, on a line of its own, so it cannot be read past.
        Outcome::NotOnline { class, elsewhere } if !elsewhere.is_empty() => format!(
            "This computer is online on {}, not on {hub}.\n({class}) {retry}",
            elsewhere.join(", ")
        ),
        Outcome::NotOnline { class, .. } => format!("Not online on {hub} ({class}). {retry}"),
        Outcome::Unknown => format!(
            "Can't tell whether this computer is online on {hub}: the agit background service \
             running now is older. Run `agit rc stop`, then `agit rc start --here`."
        ),
    }
}

/// The step on the website for a folder `--here` did not bind.
pub(super) fn website_step(hub: &str) -> String {
    format!(
        "Next: on {hub}/workspaces add this folder as a project (private), then DM your link code \
         to the bot."
    )
}

/// What `--here` knows about the folder before it asks anything.
pub(super) struct FolderState {
    /// A workspace of this computer binds it as a project already.
    pub bound: bool,
    /// This computer holds its sessions in a repository of an earlier project.
    pub earlier_project: bool,
    /// Someone at a terminal can answer, and `--yes` did not try to.
    pub can_ask: bool,
}

/// Why `--here` binds nothing for `folder`, before it asks the Hub anything: the line, and whether
/// that is a refusal. `None` when it may go on.
pub(super) fn before_binding(
    hub: &str,
    folder: &Path,
    state: &FolderState,
) -> Option<(String, bool)> {
    let folder = crate::ui::tilde(folder);
    if state.bound {
        return Some((
            format!("{folder} is already a project on {hub}. Next: DM your link code to the bot."),
            false,
        ));
    }
    if state.earlier_project {
        return Some((
            format!(
                "{folder} holds sessions of an earlier project on this computer, so --here binds \
                 nothing. To upload them, run `agit save` in it; to add the folder as a project, \
                 use {hub}/workspaces."
            ),
            true,
        ));
    }
    if !state.can_ask {
        return Some((
            format!(
                "Nothing was bound: --here asks one question first, and there was no terminal to \
                 ask it in. Add the folder as a project on {hub}/workspaces (private), or run \
                 `agit rc start --here` in a terminal."
            ),
            false,
        ));
    }
    None
}

/// This computer as the Hub knows it.
pub(super) struct Device {
    pub id: String,
    /// The name the Hub shows for it, which its workspace is named after.
    pub display_name: String,
}

/// The one question `--here` asks before it binds `folder` to the new private repository
/// `repository`.
pub(super) fn question(hub: &str, folder: &Path, repository: &str) -> String {
    let folder = crate::ui::tilde(folder);
    format!(
        "Sessions started from now on in {folder} from the web or the relay upload to {repository} \
         (new, private: only its members can read it); a terminal session does once you open it \
         on the web.\n\
         Sessions already in {folder} stay on this computer until you save them (`agit save`).\n\
         To stop the uploads: `agit off {folder}`.\n\
         Turning off Cloud access (`agit rc cloud inbound --hub {hub} --enabled false`) stops the \
         relay reaching this computer, not the uploads.\n\
         Add {folder} as a project?"
    )
}

/// How binding the folder ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Bind {
    /// The folder is a project, and its new sessions upload to `repository`.
    Bound { repository: String },
    /// The Hub offered no new private repository of the person's; the website step remains.
    NoNewRepository,
    /// The person said no.
    Declined,
}

/// Binds `folder` as a project of this computer's workspace on the Hub `client` serves, to the new
/// private repository the Hub suggests in the person's own namespace (`username`), once `ask`
/// says yes to [`question`]. A repository that already exists, one of another owner, or none
/// binds nothing. Right after the yes and before the bind, the conversations `held` lists, the
/// ones the folder holds now, are recorded to stay out of its uploads (`rc::folder_cutoff`).
pub(super) fn bind_folder(
    client: &crate::hub::Client,
    hub: &str,
    folder: &Path,
    device: &Device,
    username: &str,
    held: impl FnOnce() -> Result<Vec<String>>,
    ask: impl FnOnce(&str) -> Result<bool>,
) -> Result<Bind> {
    use crate::hub::workspaces::FolderBind;
    let local_path = folder.to_string_lossy();
    let workspace = client.device_workspace(&device.id, &device.display_name)?;
    let suggested = match client.bind_folder(&workspace.id, &local_path, None)? {
        FolderBind::Choice { suggested } => suggested,
        FolderBind::Bound(_) => anyhow::bail!("the Hub bound the folder without a choice"),
    };
    let Some((owner, name)) = suggested
        .as_deref()
        .and_then(|suggested| suggested.split_once('/'))
        .filter(|(owner, _)| *owner == username)
    else {
        return Ok(Bind::NoNewRepository);
    };
    let repository = format!("{owner}/{name}");
    if !absent(client.get_agent_unredirected(owner, name))? {
        return Ok(Bind::NoNewRepository);
    }
    if !ask(&question(hub, folder, &repository))? {
        return Ok(Bind::Declined);
    }
    crate::rc::folder_cutoff::record(folder, held()?)?;
    match client.bind_folder(&workspace.id, &local_path, Some(&repository))? {
        FolderBind::Bound(_) => Ok(Bind::Bound { repository }),
        FolderBind::Choice { .. } => anyhow::bail!("the Hub asked again which repository to use"),
    }
}

/// Whether a repository lookup found nothing there: a 404. Any other failure leaves it unknown.
fn absent<T>(lookup: Result<T>) -> Result<bool> {
    match lookup {
        Ok(_) => Ok(false),
        Err(error) => match error.downcast_ref::<crate::hub::client::ApiError>() {
            Some(api) if api.status == 404 => Ok(true),
            _ => Err(error),
        },
    }
}

pub(super) fn mismatch_line(signed_in: &str, environment: &str) -> String {
    format!(
        "Signed in to {signed_in}, but AGIT_HUB_URL points to {environment}: nothing was changed. \
         Unset AGIT_HUB_URL, or sign in there with `agit login --hub {environment}`."
    )
}

/// What plain `agit rc start` does about the Hub (#306). It keeps following `AGIT_HUB_URL`, which
/// self-hosted setups and scripts rely on, but says so when that names another Hub than the one
/// the person signed in to. It turns Cloud access on only for a Hub with a stored sign-in.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PlainStart {
    pub(super) warning: Option<String>,
    pub(super) enable_inbound: bool,
}

pub(super) fn plain_start(
    configured: &str,
    environment: Option<&str>,
    hub: &str,
    signed_in: impl Fn(&str) -> bool,
) -> PlainStart {
    let warning = match choose_hub(configured, environment, &signed_in) {
        HubChoice::Mismatch {
            signed_in,
            environment,
        } => Some(format!(
            "AGIT_HUB_URL points to {environment}, so Cloud access goes there, not to {signed_in} \
             where you signed in. To use {signed_in}, unset AGIT_HUB_URL and run `agit rc start` \
             again."
        )),
        HubChoice::Use(_) => None,
    };
    PlainStart {
        warning,
        enable_inbound: signed_in(hub),
    }
}

pub(super) fn inbound_off_line(hub: &str) -> String {
    format!(
        "Cloud access stays off for {hub} until you sign in there with `agit login --hub {hub}`; \
         local and SSH access work now."
    )
}

pub(super) fn not_signed_in_line(hub: &str) -> String {
    format!("Not signed in to {hub}: run `agit login --hub {hub}`, then `agit rc start --here`.")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "https://dev.example";
    const PROD: &str = "https://prod.example";

    fn line(hub: &str, presence: CloudPresence, error: Option<&str>) -> CloudLine {
        CloudLine {
            hub: hub.into(),
            enrolled: true,
            pending: false,
            presence,
            error: error.map(Into::into),
        }
    }

    /// Runs `wait` over a script of daemon answers, one per look; the time is up when the script
    /// ends. Returns the outcome and how many looks it took.
    fn waited(script: Vec<Option<Vec<CloudLine>>>) -> (Outcome, usize) {
        let requested = vec![false; script.len()];
        waited_requesting(script, requested)
    }

    /// As [`waited`], with whether this run's inbound request still waits for the daemon, one
    /// answer per check in order (still waiting once they run out).
    fn waited_requesting(
        script: Vec<Option<Vec<CloudLine>>>,
        requested: Vec<bool>,
    ) -> (Outcome, usize) {
        let total = script.len();
        let mut answers = script.into_iter();
        let mut requests = requested.into_iter();
        let looked = std::cell::Cell::new(0);
        let outcome = wait(
            DEV,
            || {
                looked.set(looked.get() + 1);
                answers.next().flatten()
            },
            || requests.next().unwrap_or(true),
            || looked.get() < total,
        );
        (outcome, looked.get())
    }

    /// `--here` never switches Cloud access on for a Hub the person did not sign in to: signed in
    /// to dev while `AGIT_HUB_URL` names prod, it refuses (#306).
    #[test]
    fn here_never_enables_inbound_on_a_hub_not_signed_in_to() {
        let signed_in = |hub: &str| hub == DEV;
        assert_eq!(
            choose_hub(DEV, Some(PROD), signed_in),
            HubChoice::Mismatch {
                signed_in: DEV.into(),
                environment: PROD.into(),
            }
        );
        assert_eq!(choose_hub(DEV, None, signed_in), HubChoice::Use(DEV.into()));
        assert_eq!(
            choose_hub(DEV, Some("https://dev.example/"), signed_in),
            HubChoice::Use(DEV.into()),
            "the same Hub spelled with a slash is no mismatch"
        );
        assert_eq!(
            choose_hub(PROD, Some(DEV), signed_in),
            HubChoice::Use(DEV.into()),
            "without a sign-in on the recorded Hub, AGIT_HUB_URL is used and signed in to"
        );
    }

    /// `--here` waits until this computer's presence connects on the signed-in Hub, and not
    /// longer: an answer that arrives on the third look is seen on the third look.
    #[test]
    fn here_waits_for_presence_on_the_signed_in_hub() {
        let connecting = Some(vec![line(DEV, CloudPresence::Connecting, None)]);
        let connected = Some(vec![line(DEV, CloudPresence::Connected, None)]);
        assert_eq!(
            waited(vec![
                None,
                connecting.clone(),
                connected.clone(),
                connecting
            ]),
            (Outcome::Online, 3)
        );
        assert_eq!(
            waited(vec![Some(vec![line(
                DEV,
                CloudPresence::Failed,
                Some("not_known_to_this_hub")
            )])])
            .0,
            Outcome::NotOnline {
                class: "not_known_to_this_hub".into(),
                elsewhere: vec![]
            }
        );
        assert_eq!(waited(vec![Some(vec![]), Some(vec![])]).0, Outcome::Unknown);
    }

    /// Signed in to dev while this computer is online on prod only: the answer's first line is
    /// "This computer is online on prod, not on dev."
    #[test]
    fn here_names_the_hub_this_computer_is_online_on_instead() {
        let (outcome, _) = waited(vec![Some(vec![
            line(DEV, CloudPresence::Failed, Some("not_known_to_this_hub")),
            line(PROD, CloudPresence::Connected, None),
        ])]);
        assert_eq!(
            outcome,
            Outcome::NotOnline {
                class: "not_known_to_this_hub".into(),
                elsewhere: vec![PROD.into()]
            }
        );
        let text = outcome_line(DEV, &outcome);
        // The wrong Hub is the whole first line, not a clause inside a longer one.
        assert_eq!(
            text.lines().next(),
            Some(format!("This computer is online on {PROD}, not on {DEV}.").as_str()),
            "{text}"
        );
    }

    /// Online is this run's enrollment connected, never a connection left from before: not while
    /// the daemon has yet to take the request, and not while the enrollment it serves is pending
    /// (an earlier account's socket still open after a switch).
    #[test]
    fn here_counts_only_the_enrollment_this_run_requested() {
        let connected = line(DEV, CloudPresence::Connected, None);
        let (outcome, looks) = waited_requesting(
            vec![Some(vec![connected.clone()]); 3],
            vec![true, true, false],
        );
        assert_eq!((outcome, looks), (Outcome::Online, 3));
        let switching = CloudLine {
            enrolled: false,
            pending: true,
            ..connected.clone()
        };
        let (outcome, _) = waited(vec![Some(vec![switching]); 3]);
        assert!(matches!(outcome, Outcome::NotOnline { .. }), "{outcome:?}");
        let (outcome, _) = waited_requesting(vec![Some(vec![connected])], vec![true]);
        assert!(matches!(outcome, Outcome::NotOnline { .. }), "{outcome:?}");
    }

    /// The Hub is compared as the Cloud client names it, so a sign-in recorded with another case
    /// or a trailing slash is the same Hub, not a mismatch and not "online elsewhere".
    #[test]
    fn here_compares_hubs_by_their_cloud_origin() {
        assert_eq!(origin("https://DEV.example/"), DEV);
        assert_eq!(
            choose_hub("https://DEV.example", Some("https://dev.example/"), |_| {
                true
            }),
            HubChoice::Use(DEV.into())
        );
        let (outcome, _) = waited(vec![Some(vec![line(
            &origin("https://DEV.example"),
            CloudPresence::Connected,
            None,
        )])]);
        assert_eq!(outcome, Outcome::Online);
    }

    /// A snapshot taken while the request still waited never turns into Online: not when the
    /// next query fails after the request is gone, only on an answer fetched after that.
    #[test]
    fn here_decides_online_only_on_an_answer_fetched_after_the_request_was_taken() {
        let old = line(DEV, CloudPresence::Connected, None);
        // Look 1: the earlier account's line, request still waiting. Look 2: the request is
        // gone but the query fails. Look 3: a fresh answer.
        let (outcome, looks) = waited_requesting(
            vec![Some(vec![old.clone()]), None, Some(vec![old.clone()])],
            vec![true, false],
        );
        assert_eq!((outcome, looks), (Outcome::Online, 3));
        let (outcome, _) = waited_requesting(vec![Some(vec![old]), None], vec![true, false]);
        assert!(matches!(outcome, Outcome::NotOnline { .. }), "{outcome:?}");
    }

    /// Plain `rc start` (#306): an `AGIT_HUB_URL` that names another Hub than the signed-in one
    /// is warned about, not refused; Cloud access is turned on only for a Hub with a sign-in;
    /// a person signed in to the Hub they use sees nothing new.
    #[test]
    fn plain_start_warns_about_another_hub_and_needs_a_sign_in_for_cloud_access() {
        let only_dev = |hub: &str| hub == DEV;
        let mismatch = plain_start(DEV, Some(PROD), PROD, only_dev);
        let warning = mismatch
            .warning
            .as_deref()
            .expect("a warning, not a refusal");
        assert!(warning.contains(PROD) && warning.contains(DEV), "{warning}");
        assert!(
            !mismatch.enable_inbound,
            "no sign-in on prod: Cloud access stays off"
        );
        let both = |_: &str| true;
        let signed_both = plain_start(DEV, Some(PROD), PROD, both);
        assert!(signed_both.warning.is_some() && signed_both.enable_inbound);
        assert_eq!(
            plain_start(DEV, None, DEV, only_dev),
            PlainStart {
                warning: None,
                enable_inbound: true
            },
            "the usual case is unchanged"
        );
        assert_eq!(
            plain_start(DEV, Some("https://dev.example/"), DEV, only_dev).warning,
            None,
            "the same Hub spelled differently is not another Hub"
        );
        let nobody = |_: &str| false;
        assert_eq!(
            plain_start(DEV, Some(PROD), PROD, nobody),
            PlainStart {
                warning: None,
                enable_inbound: false
            },
            "AGIT_HUB_URL alone, no sign-in anywhere: nothing to warn about, no Cloud access"
        );
    }

    mod binding {
        use super::super::*;
        use crate::hub::workspaces::test_hub::{TestHome, client, serve};

        const WORKSPACE: (u16, &str) = (200, r#"{"id":"ws-1","name":"Ada's laptop"}"#);
        const NOT_FOUND: (u16, &str) = (404, r#"{"error":"not found","kind":"not_found"}"#);

        fn choice(suggested: &str) -> String {
            format!(
                r#"{{"error":"choose","kind":"repository_choice","fix":[],"candidates":["acme/work"],"suggested":"{suggested}","unavailable":[],"audiences":{{}}}}"#
            )
        }

        /// What a run of [`bind_folder`] came to: its outcome, the question asked, the requests
        /// the Hub saw, and whether the folder's earlier conversation stays out afterwards.
        type Run = (Result<Bind>, Option<String>, Vec<(String, String)>, bool);

        fn device() -> Device {
            Device {
                id: "dev-9".into(),
                display_name: "Ada's laptop".into(),
            }
        }

        /// Runs [`bind_folder`] for a new folder against `script`, answering the question with
        /// `yes`. Returns the outcome (or error), the question asked, the requests the Hub saw,
        /// and whether the folder's earlier conversation stays out afterwards.
        fn run(script: Vec<(u16, &str)>, yes: bool) -> Run {
            let _home = TestHome::new();
            let folder = tempfile::tempdir().unwrap();
            let folder = folder.path().canonicalize().unwrap();
            let (base, hub) = serve(script);
            let mut asked = None;
            let outcome = bind_folder(
                &client(&base),
                "https://hub.example",
                &folder,
                &device(),
                "ada",
                || Ok(vec!["claude-code:before".to_owned()]),
                |question| {
                    asked = Some(question.to_owned());
                    Ok(yes)
                },
            );
            drop(client(&base));
            // The stub waits for every scripted request; one the flow did not make is not waited
            // for.
            let seen = hub.join().unwrap();
            let kept_out = crate::rc::folder_cutoff::leaves_out(&folder, "claude-code", "before");
            (outcome, asked, seen, kept_out)
        }

        /// On a yes, `--here` names the workspace like the device, asks the Hub's choice, checks
        /// that the suggested name is free, records the conversations the folder holds, and only
        /// then binds with that repository. The record is in place even when the bind fails.
        #[test]
        fn here_records_what_the_folder_holds_before_it_binds() {
            let chosen = choice("ada/work");
            let (outcome, asked, seen, kept_out) = run(
                vec![
                    WORKSPACE,
                    (409, &chosen),
                    NOT_FOUND,
                    (500, r#"{"error":"boom","kind":"internal"}"#),
                ],
                true,
            );
            assert!(outcome.is_err(), "{outcome:?}");
            assert!(asked.unwrap().contains("ada/work"));
            assert_eq!(seen.len(), 4);
            assert!(
                seen[0]
                    .0
                    .starts_with("PUT /api/peer/devices/dev-9/workspace ")
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&seen[0].1).unwrap(),
                serde_json::json!({"name": "Ada's laptop"}),
                "the workspace is named like its device"
            );
            assert!(
                seen[2].0.starts_with("GET /api/agents/ada/work "),
                "{}",
                seen[2].0
            );
            assert!(seen[3].0.starts_with("POST /api/workspaces/ws-1/projects "));
            let sent: serde_json::Value = serde_json::from_str(&seen[3].1).unwrap();
            assert_eq!(sent["repository"], "ada/work");
            assert!(kept_out, "recorded before the bind");

            let chosen = choice("ada/work");
            let (outcome, _, seen, kept_out) = run(
                vec![
                    WORKSPACE,
                    (409, &chosen),
                    NOT_FOUND,
                    (201, r#"{"id":"p-1"}"#),
                ],
                true,
            );
            assert_eq!(
                outcome.unwrap(),
                Bind::Bound {
                    repository: "ada/work".into()
                }
            );
            assert_eq!(seen.len(), 4);
            assert!(kept_out);
        }

        /// A no binds nothing and records nothing.
        #[test]
        fn here_binds_nothing_when_the_person_says_no() {
            let chosen = choice("ada/work");
            let (outcome, asked, seen, kept_out) =
                run(vec![WORKSPACE, (409, &chosen), NOT_FOUND], false);
            assert_eq!(outcome.unwrap(), Bind::Declined);
            assert!(asked.is_some());
            assert_eq!(seen.len(), 3, "no bind with a repository");
            assert!(!kept_out);
        }

        /// `--here` takes only the new repository the Hub suggests in the person's own namespace:
        /// never one of another owner, never a candidate (an existing repository, possibly an
        /// organization's), and never a suggested name that exists by now. Nothing is asked,
        /// recorded or bound then.
        #[test]
        fn here_takes_only_a_new_repository_of_its_own() {
            let other = choice("acme/work-sessions");
            let (outcome, asked, seen, _) = run(vec![WORKSPACE, (409, &other)], true);
            assert_eq!(outcome.unwrap(), Bind::NoNewRepository);
            assert!(asked.is_none());
            assert_eq!(seen.len(), 2);

            let none = r#"{"error":"choose","kind":"repository_choice","fix":[],"candidates":["acme/work"],"unavailable":[],"audiences":{}}"#;
            let (outcome, asked, _, _) = run(vec![WORKSPACE, (409, none)], true);
            assert_eq!(outcome.unwrap(), Bind::NoNewRepository);
            assert!(asked.is_none());

            let chosen = choice("ada/work");
            let existing = r#"{"agent_id":"r-1","owner":"ada","name":"work","clone_url":"https://hub.example/ada/work.git","visibility":"private"}"#;
            let (outcome, asked, seen, kept_out) =
                run(vec![WORKSPACE, (409, &chosen), (200, existing)], true);
            assert_eq!(outcome.unwrap(), Bind::NoNewRepository);
            assert!(asked.is_none(), "an existing name is never offered");
            assert_eq!(seen.len(), 3);
            assert!(!kept_out);
        }

        /// The question names the new repository and who can read it, what uploads from now on
        /// and what doesn't, how to stop the uploads, and that turning Cloud access off doesn't.
        #[test]
        fn here_asks_what_it_starts_and_how_to_stop() {
            let folder = Path::new("/work/app");
            let asked = question("https://hub.example", folder, "ada/app");
            for expected in [
                "ada/app (new, private: only its members can read it)",
                "Sessions started from now on in /work/app from the web or the relay upload to",
                "a terminal session does once you open it on the web",
                "Sessions already in /work/app stay on this computer until you save them",
                "To stop the uploads: `agit off /work/app`",
                "`agit rc cloud inbound --hub https://hub.example --enabled false`",
                "not the uploads",
            ] {
                assert!(asked.contains(expected), "{expected:?} in {asked}");
            }
        }

        /// Without someone at a terminal to answer (no terminal, or `--yes`), `--here` binds
        /// nothing; nor for a folder that is a project already, or one whose sessions this
        /// computer holds for an earlier project, which it refuses.
        #[test]
        fn here_binds_nothing_it_was_not_asked_to() {
            let hub = "https://hub.example";
            let folder = Path::new("/work/app");
            let state = |bound, earlier_project, can_ask| FolderState {
                bound,
                earlier_project,
                can_ask,
            };
            assert!(before_binding(hub, folder, &state(false, false, true)).is_none());
            let (line, refused) = before_binding(hub, folder, &state(false, false, false)).unwrap();
            assert!(!refused && line.starts_with("Nothing was bound"), "{line}");
            let (line, refused) = before_binding(hub, folder, &state(false, true, true)).unwrap();
            assert!(refused && line.contains("`agit save`"), "{line}");
            let (line, refused) = before_binding(hub, folder, &state(true, false, true)).unwrap();
            assert!(!refused && line.contains("already a project"), "{line}");
        }
    }
}
