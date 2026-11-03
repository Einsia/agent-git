//! `agit rc start --here`: make this computer reachable through Cloud on the Hub the person
//! signed in to, and say plainly when it is not. It publishes nothing and touches no repository.

use crate::rc::control::{CloudLine, CloudPresence};

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
        Outcome::Online => format!(
            "Online on {hub}. Next: on {hub}/workspaces add this folder as a project (private), \
             then DM your link code to the bot."
        ),
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

pub(super) fn mismatch_line(signed_in: &str, environment: &str) -> String {
    format!(
        "Signed in to {signed_in}, but AGIT_HUB_URL points to {environment}: nothing was changed. \
         Unset AGIT_HUB_URL, or sign in there with `agit login --hub {environment}`."
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
}
