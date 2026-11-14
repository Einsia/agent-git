//! The team memory a conversation the daemon launches starts with: the `memory/` on `main` of the
//! repository its turns upload to, placed in the runtime's memory directory by the step `agit new`
//! and `agit resume` use ([`crate::commands::memory`]).
//!
//! A conversation started from the Hub settles in a device-local repository with no `main` of its
//! own, so the destination's `main` is read into a private ref of that repository. A start whose
//! read does not finish in time goes on with the copy read last; with no copy it places nothing.

use crate::{commands::memory, domain::repo::Repo, rc::lineage::AgitSession};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// How long a start waits for the Hub's `main` before it goes on with the copy read last.
pub(crate) const FETCH_WITHIN: Duration = Duration::from_secs(5);

/// How long a launch waits for another placing in the same memory directory before it starts
/// without placing. What that wait covers is local file writes only: the Hub is read before it.
const INDEX_WAIT: Duration = Duration::from_secs(5);

/// One placing at a time per runtime memory directory: two at once each write back the index the
/// other read. Directories don't wait on each other.
static INDEXES: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

fn index_lock(dir: &Path) -> Arc<tokio::sync::Mutex<()>> {
    INDEXES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(dir.to_path_buf())
        .or_default()
        .clone()
}

/// Branches whose launch is under way, by repository, with how many launches hold each.
static LAUNCHING: LazyLock<Mutex<BTreeMap<(String, String), usize>>> =
    LazyLock::new(Default::default);

/// Keeps a launching conversation's index section from being taken out by another start in its
/// folder, from before its memory is placed until the daemon lists it as running.
pub(crate) struct Launching {
    key: (String, String),
}

impl Launching {
    pub(crate) fn enter(lineage: &AgitSession) -> Self {
        let key = (lineage.slug(), lineage.branch().to_owned());
        *LAUNCHING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(key.clone())
            .or_default() += 1;
        Self { key }
    }
}

impl Drop for Launching {
    fn drop(&mut self) {
        let mut launching = LAUNCHING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = launching.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                launching.remove(&self.key);
            }
        }
    }
}

/// The branches of `slug` whose launch is under way.
pub(crate) fn launching(slug: &str) -> BTreeSet<String> {
    LAUNCHING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .keys()
        .filter(|(repository, _)| repository == slug)
        .map(|(_, branch)| branch.clone())
        .collect()
}

/// The private ref holding the `main` last read from the Hub repository `agent_id`.
fn copy_ref(agent_id: &str) -> String {
    format!("refs/agit/memory-source/{agent_id}")
}

/// What placing a conversation's team memory comes to, worked out before any index is written:
/// the runtime's memory directory, and how the conversation gets its memory there.
pub(crate) struct Plan {
    lineage: AgitSession,
    runtime: String,
    cwd: PathBuf,
    dir: PathBuf,
    step: Step,
}

enum Step {
    /// A conversation that comes back lists its own mirror again.
    Reindex,
    /// A start on a branch the repository has: its own tree.
    Materialize,
    /// A start on a branch not born yet: the destination's `main`, as read into this ref.
    Unborn(String),
    /// A start with nothing to place, which still takes idle sections out.
    Nothing,
}

/// Places the team memory of `lineage`'s conversation before `runtime` starts in `cwd`, as a
/// launch does: the Hub is read first, within `within`, and only then is the runtime's index
/// locked and written, with `running` asked for the branches whose sections stay. A slow Hub
/// therefore holds up no other placing, and a placing that waits longer than [`INDEX_WAIT`] for
/// its index goes on without one. Nothing here stops a launch: what cannot be done is logged.
pub(crate) async fn place_for_launch<F, Fut>(
    lineage: AgitSession,
    runtime: String,
    cwd: PathBuf,
    fresh: bool,
    within: Duration,
    running: F,
) where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = BTreeSet<String>>,
{
    let planned =
        tokio::task::spawn_blocking(move || plan(&lineage, &runtime, &cwd, fresh, within)).await;
    let plan = match planned {
        Ok(Ok(Some(plan))) => plan,
        Ok(Ok(None)) => return,
        Ok(Err(error)) => {
            eprintln!("agitd: team memory was not placed: {error:#}");
            return;
        }
        Err(error) => {
            eprintln!("agitd: team memory was not placed: {error}");
            return;
        }
    };
    let lock = index_lock(&plan.dir);
    let Ok(_index) = tokio::time::timeout(INDEX_WAIT, lock.lock_owned()).await else {
        eprintln!("agitd: team memory was not placed: another start kept the memory index busy");
        return;
    };
    let running = running(plan.lineage.slug()).await;
    if let Ok(Err(error)) = tokio::task::spawn_blocking(move || apply(plan, &running)).await {
        eprintln!("agitd: team memory was not placed: {error:#}");
    }
}

/// [`place_for_launch`] in one go, without the index lock, for a caller that is the only one
/// placing.
#[cfg(test)]
pub(crate) fn prepare(
    lineage: &AgitSession,
    runtime: &str,
    cwd: &Path,
    fresh: bool,
    running: &BTreeSet<String>,
    within: Duration,
) {
    let placed = plan(lineage, runtime, cwd, fresh, within)
        .and_then(|plan| plan.map_or(Ok(()), |plan| apply(plan, running)));
    if let Err(error) = placed {
        eprintln!("agitd: team memory was not placed: {error:#}");
    }
}

/// Works out how `lineage`'s conversation gets its memory, reading the destination's `main` from
/// the Hub for a start on an unborn branch. Writes no index. `None` when the runtime keeps no
/// memory directory here or the repository is gone.
fn plan(
    lineage: &AgitSession,
    runtime: &str,
    cwd: &Path,
    fresh: bool,
    within: Duration,
) -> crate::Result<Option<Plan>> {
    let Some(runtime_memory) = crate::infra::runtime_memory::locate(runtime, cwd) else {
        return Ok(None);
    };
    let Some(repo) = Repo::open(lineage.repo_dir()?) else {
        return Ok(None);
    };
    let step = if !fresh {
        Step::Reindex
    } else if repo.has_ref(&format!("refs/heads/{}", lineage.branch())) {
        Step::Materialize
    } else {
        refresh_copy(&repo, lineage, within).map_or(Step::Nothing, Step::Unborn)
    };
    Ok(Some(Plan {
        lineage: lineage.clone(),
        runtime: runtime.to_owned(),
        cwd: cwd.to_path_buf(),
        dir: runtime_memory.dir,
        step,
    }))
}

/// Writes `plan` into the runtime's index. A start takes the sections of the repository's
/// conversations outside `running` out of it first; a conversation that comes back lists its own
/// mirror again.
fn apply(plan: Plan, running: &BTreeSet<String>) -> crate::Result<()> {
    let (slug, branch, dir) = (plan.lineage.slug(), plan.lineage.branch(), &plan.dir);
    if let Step::Reindex = plan.step {
        return memory::reindex(dir, &slug, branch);
    }
    let mut keep = running.clone();
    keep.insert(branch.to_owned());
    memory::drop_sections(dir, &slug, &keep)?;
    match plan.step {
        Step::Materialize => {
            let repo = Repo::open(plan.lineage.repo_dir()?)
                .ok_or_else(|| anyhow::anyhow!("the conversation's repository is gone"))?;
            memory::materialize(&repo, branch, &slug, &plan.runtime, &plan.cwd)?;
        }
        Step::Unborn(source) => {
            let repo = Repo::open(plan.lineage.repo_dir()?)
                .ok_or_else(|| anyhow::anyhow!("the conversation's repository is gone"))?;
            memory::materialize_unborn(&repo, branch, &slug, dir, &source)?;
        }
        Step::Reindex | Step::Nothing => {}
    }
    Ok(())
}

/// The tree a new conversation's team memory comes from: `main` of the repository its turns
/// upload to, read into a private ref within `within`, or the copy read last when that read
/// fails or the folder's uploads are off. `None` with no destination or no copy.
fn refresh_copy(repo: &Repo, lineage: &AgitSession, within: Duration) -> Option<String> {
    let destination = match crate::rc::capture::destination(repo, lineage) {
        Ok(Some(destination)) => destination,
        Ok(None) => {
            eprintln!(
                "agitd: team memory: this folder has no upload destination yet; nothing is placed"
            );
            return None;
        }
        Err(error) => {
            eprintln!("agitd: team memory: the upload destination was not read: {error:#}");
            return None;
        }
    };
    let copy = copy_ref(&destination.identity.agent_id);
    let fallback = || {
        if repo.has_ref(&copy) {
            "placing the copy read earlier"
        } else {
            "there is no earlier copy, so nothing is placed"
        }
    };
    // A folder whose uploads are off exchanges nothing with the Hub, reads included: it gets the
    // copy read while they were on.
    if repo.uploads_turned_off().unwrap_or(true) {
        eprintln!(
            "agitd: team memory: uploads are off for this folder, so main is not read from the Hub; {}",
            fallback()
        );
    } else if let Err(cause) = fetch_main(repo, &destination, &copy, within) {
        eprintln!(
            "agitd: team memory: main was not read from the Hub ({cause}); {}",
            fallback()
        );
    }
    repo.has_ref(&copy).then_some(copy)
}

/// Reads `main` of `destination` into `copy`, waiting for it no longer than `within`.
///
/// The read goes on in a thread of its own, and a start that stops waiting leaves it to finish:
/// renewing an expired sign-in waits for the process's other renewals and keeps its own deadline,
/// and a renewal cut short could lose the single-use token the Hub has just rotated. A renewal
/// that finishes late is saved for the next start; the read itself keeps the start's deadline,
/// so nothing is fetched into the copy once the start has stopped waiting.
fn fetch_main(
    repo: &Repo,
    destination: &crate::rc::capture::PublicationDestination,
    copy: &str,
    within: Duration,
) -> Result<(), String> {
    let (answer, answered) = std::sync::mpsc::channel();
    let (root, destination, copy) = (
        repo.root().to_path_buf(),
        destination.clone(),
        copy.to_owned(),
    );
    std::thread::Builder::new()
        .name("team-memory-read".into())
        .spawn(move || {
            let _ = answer.send(read_main(&Repo::at(root), &destination, &copy, within));
        })
        .map_err(|_| "the read could not start".to_owned())?;
    answered
        .recv_timeout(within)
        .unwrap_or_else(|_| Err("the Hub did not answer in time".into()))
}

/// Reads `main` of `destination` into `copy` through agit's own transport, its requests and
/// fetch within `within`.
fn read_main(
    repo: &Repo,
    destination: &crate::rc::capture::PublicationDestination,
    copy: &str,
    within: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + within;
    let (owner, name) = crate::commands::parse_slug(&destination.repository)
        .map_err(|_| "the destination is not named owner/repository".to_owned())?;
    // The destination's own Hub, with its saved sign-in: the process's default Hub can be another.
    let client = crate::hub::Client::for_stored_hub_with_timeout(&destination.identity.hub, within);
    let remote = client
        .get_agent(&owner, &name)
        .map_err(|_| "the Hub did not answer for the repository".to_owned())?;
    if remote.agent_id != destination.identity.agent_id {
        return Err("that name belongs to another repository now".into());
    }
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err("the time to read it ran out".into());
    }
    crate::hub::git::capture_for_remote(
        repo,
        &[
            "fetch",
            "--no-tags",
            "--quiet",
            &remote.clone_url,
            &format!("+refs/heads/main:{copy}"),
        ],
        &destination.identity,
        left,
    )
    .map(|_| ())
    .map_err(|unread| unread.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rc::local_repository;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const TEAM: &str = "00000000-0000-0000-0000-00000000000b";
    const DESTINATION: &str = "simco/lumen-agents";

    /// A Hub that names [`DESTINATION`], with a local repository as its clone address. Once told
    /// to stop answering it still takes connections and never replies, as a Hub that hangs does.
    struct Hub {
        url: String,
        answering: Arc<AtomicBool>,
        /// Once set, the Hub refuses the device's token as expired and never answers the renewal
        /// that follows, as a Hub whose sign-in service hangs does.
        expired: Arc<AtomicBool>,
        stopping: Arc<AtomicBool>,
        /// Told each time the Hub takes a connection it won't answer.
        held: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl Hub {
        fn new(clone_url: &Path) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let agent = serde_json::json!({
                "agent_id": TEAM, "owner": "simco", "name": "lumen-agents",
                "visibility": "private", "clone_url": clone_url,
            })
            .to_string();
            let answering = Arc::new(AtomicBool::new(true));
            let expired = Arc::new(AtomicBool::new(false));
            let stopping = Arc::new(AtomicBool::new(false));
            let (answers, expires, stops) = (answering.clone(), expired.clone(), stopping.clone());
            let (hold, held) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let mut held = Vec::new();
                for stream in listener.incoming() {
                    if stops.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    if !answers.load(Ordering::Acquire) {
                        held.push(stream);
                        let _ = hold.send(());
                        continue;
                    }
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                    let mut reader = BufReader::new(&stream);
                    let mut request = String::new();
                    let _ = reader.read_line(&mut request);
                    let mut header = String::new();
                    while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
                        header.clear();
                    }
                    let reading = request.starts_with("GET /api/agents/simco/lumen-agents ");
                    if expires.load(Ordering::Acquire) && !reading {
                        held.push(stream);
                        let _ = hold.send(());
                        continue;
                    }
                    let mut stream = stream;
                    let _ = if reading && expires.load(Ordering::Acquire) {
                        write!(
                            stream,
                            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                    } else if reading {
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{agent}",
                            agent.len()
                        )
                    } else {
                        write!(
                            stream,
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                    };
                }
            });
            Self {
                url,
                answering,
                expired,
                stopping,
                held: Arc::new(Mutex::new(held)),
                worker: Some(worker),
            }
        }
    }

    impl Drop for Hub {
        fn drop(&mut self) {
            self.stopping.store(true, Ordering::Release);
            let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    /// A device signed in to [`Hub`], with a folder whose conversations upload to
    /// [`DESTINATION`], whose `main` holds the team's decisions.
    struct Folder {
        _root: tempfile::TempDir,
        hub: Hub,
        team: Repo,
        cwd: PathBuf,
        repository: local_repository::Repository,
    }

    impl Folder {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let base = root.path().canonicalize().unwrap();
            let team = Repo::init(&base.join("team")).unwrap();
            team.git(&["config", "commit.gpgsign", "false"]).unwrap();
            let hub = Hub::new(team.root());
            unsafe {
                std::env::set_var("AGIT_HOME", base.join("agit"));
                std::env::set_var("AGIT_HUB_URL", &hub.url);
                std::env::set_var("CLAUDE_CONFIG_DIR", base.join("claude"));
            }
            crate::rc::select_local_authority();
            crate::infra::credentials::save(
                &hub.url,
                &crate::infra::credentials::HubCredential {
                    account_id: Some("account-nadia".into()),
                    username: "nadia".into(),
                    email: None,
                    hub: Some(hub.url.clone()),
                    access_token: "fixture".into(),
                    refresh_token: "fixture".into(),
                    access_expires_at: "2099-01-01T00:00:00Z".into(),
                    refresh_expires_at: "2099-01-01T00:00:00Z".into(),
                },
            )
            .unwrap();
            let cwd = base.join("lumen");
            std::fs::create_dir(&cwd).unwrap();
            let repository = local_repository::ensure_repository("p", &cwd).unwrap();
            let folder = Self {
                _root: root,
                hub,
                team,
                cwd,
                repository,
            };
            let repo = folder.repo();
            local_repository::publication::Selection::prepare(
                &repo,
                Some(DESTINATION),
                &folder.hub.url,
            )
            .unwrap()
            .unwrap()
            .bind(
                &repo,
                &crate::hub::identity::RemoteIdentity::new(&folder.hub.url, TEAM).unwrap(),
            )
            .unwrap();
            folder.distill("credits are integer milli-credits\n");
            folder
        }

        fn lineage(&self, branch: &str) -> AgitSession {
            AgitSession::new(&self.repository.slug, &self.repository.agent_id, branch).unwrap()
        }

        fn repo(&self) -> Repo {
            local_repository::require(&self.lineage("main")).unwrap()
        }

        /// Lands `decisions` on the team's `main`, as a distillation does.
        fn distill(&self, decisions: &str) {
            let dir = self.team.root().join("memory");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("decisions.md"), decisions).unwrap();
            self.team.add_all().unwrap();
            self.team.commit("distill").unwrap();
        }

        fn memory(&self) -> PathBuf {
            crate::infra::runtime_memory::locate("claude-code", &self.cwd)
                .unwrap()
                .dir
        }

        /// Prepares the conversation on `branch` as a launch does: a start when `fresh`.
        fn launch(&self, branch: &str, fresh: bool, running: &[&str], within: Duration) {
            let running = running.iter().map(|branch| branch.to_string()).collect();
            prepare(
                &self.lineage(branch),
                "claude-code",
                &self.cwd,
                fresh,
                &running,
                within,
            );
        }

        /// The team file as the conversation on `branch` was given it.
        fn given(&self, branch: &str) -> Option<String> {
            std::fs::read_to_string(self.mirror(branch).join("decisions.md")).ok()
        }

        fn mirror(&self, branch: &str) -> PathBuf {
            let (owner, name) = self.repository.slug.split_once('/').unwrap();
            self.memory()
                .join("agit")
                .join(owner)
                .join(name)
                .join(branch)
        }

        /// The branches whose sections the runtime's index lists.
        fn sections(&self) -> Vec<String> {
            let index =
                std::fs::read_to_string(self.memory().join("MEMORY.md")).unwrap_or_default();
            let opening = format!("<!-- agit:memory:begin {}@", self.repository.slug);
            index
                .lines()
                .filter_map(|line| line.strip_prefix(opening.as_str()))
                .filter_map(|rest| rest.strip_suffix(" -->"))
                .map(str::to_owned)
                .collect()
        }
    }

    /// A conversation started from the Hub is given the memory on `main` of the repository its
    /// turns upload to, under its own branch, though the device-local repository it settles in
    /// has no `main`, and read from that repository's own Hub when the process's default Hub is
    /// another one. When the Hub stops answering, or refuses the sign-in as expired and never
    /// answers its renewal, the next start still returns within its bound and gives the copy read
    /// last, not nothing; a start that read the local repository's own refs, or waited on the Hub
    /// or on the renewal without a bound, fails one of the two. A folder whose uploads
    /// are off gets the copy read last while the Hub answers with a newer `main`: it does not
    /// read from the Hub at all.
    #[test]
    fn a_start_gets_main_of_its_destination_or_the_copy_read_last() {
        if crate::rc::in_isolated_test(
            "rc::team_memory::tests::a_start_gets_main_of_its_destination_or_the_copy_read_last",
        ) {
            return;
        }
        let folder = Folder::new();
        // The process's default Hub is another one: the destination's own Hub is read.
        unsafe { std::env::set_var("AGIT_HUB_URL", "https://elsewhere.example") };
        folder.launch("desktop-a", true, &[], FETCH_WITHIN);
        assert_eq!(
            folder.given("desktop-a").as_deref(),
            Some("credits are integer milli-credits\n")
        );
        assert_eq!(folder.sections(), ["desktop-a"]);

        folder.distill("credits are integer milli-credits\nrefunds go back as credits\n");
        folder.hub.answering.store(false, Ordering::Release);
        folder.launch(
            "desktop-b",
            true,
            &["desktop-a"],
            Duration::from_millis(300),
        );
        assert_eq!(
            folder.given("desktop-b").as_deref(),
            Some("credits are integer milli-credits\n")
        );
        assert_eq!(folder.sections(), ["desktop-a", "desktop-b"]);

        folder.hub.answering.store(true, Ordering::Release);
        folder.hub.expired.store(true, Ordering::Release);
        let started = Instant::now();
        folder.launch(
            "desktop-d",
            true,
            &["desktop-a", "desktop-b"],
            Duration::from_millis(300),
        );
        // The renewal keeps a deadline of its own, far past this bound and past this margin.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the start waited on the renewal: {:?}",
            started.elapsed()
        );
        assert_eq!(
            folder.given("desktop-d").as_deref(),
            Some("credits are integer milli-credits\n")
        );

        folder.hub.expired.store(false, Ordering::Release);
        folder.repo().turn_uploads_off(true).unwrap();
        folder.launch("desktop-c", true, &[], FETCH_WITHIN);
        assert_eq!(
            folder.given("desktop-c").as_deref(),
            Some("credits are integer milli-credits\n")
        );
    }

    /// A start takes out of the runtime's index the sections of the repository's conversations
    /// that are not running, and keeps a running one's. The mirror of one taken out stays as it
    /// is, an edit not collected yet included, and when that conversation comes back its section
    /// lists the mirror again. A section of another repository is left alone.
    #[test]
    fn a_start_takes_out_idle_conversations_sections_and_keeps_their_edits() {
        if crate::rc::in_isolated_test(
            "rc::team_memory::tests::a_start_takes_out_idle_conversations_sections_and_keeps_their_edits",
        ) {
            return;
        }
        let folder = Folder::new();
        let other = "<!-- agit:memory:begin simco/other@main -->\n- [x](agit/simco/other/main/x.md)\n<!-- agit:memory:end -->\n";
        std::fs::create_dir_all(folder.memory()).unwrap();
        std::fs::write(folder.memory().join("MEMORY.md"), other).unwrap();
        folder.launch("desktop-a", true, &[], FETCH_WITHIN);
        folder.launch("desktop-b", true, &["desktop-a"], FETCH_WITHIN);
        let edited = "credits are integer milli-credits; never floats\n";
        std::fs::write(folder.mirror("desktop-a").join("decisions.md"), edited).unwrap();

        folder.launch("desktop-c", true, &["desktop-b"], FETCH_WITHIN);
        assert_eq!(folder.sections(), ["desktop-b", "desktop-c"]);
        assert_eq!(folder.given("desktop-a").as_deref(), Some(edited));
        let index = std::fs::read_to_string(folder.memory().join("MEMORY.md")).unwrap();
        assert!(index.contains(other), "{index}");

        folder.launch(
            "desktop-a",
            false,
            &["desktop-b", "desktop-c"],
            FETCH_WITHIN,
        );
        assert_eq!(folder.sections(), ["desktop-b", "desktop-c", "desktop-a"]);
        assert_eq!(folder.given("desktop-a").as_deref(), Some(edited));
    }

    /// A start waiting on a Hub that doesn't answer holds up no other placing: the Hub is read
    /// before the memory index is locked, so a resume in the same folder lists its mirror while
    /// that start still waits. A placing that held the index across the Hub read, as one lock
    /// around the whole step does, would keep the resume waiting for the Hub's whole bound.
    #[tokio::test]
    async fn a_start_waiting_on_the_hub_holds_up_no_other_placing() {
        if crate::rc::in_isolated_test(
            "rc::team_memory::tests::a_start_waiting_on_the_hub_holds_up_no_other_placing",
        ) {
            return;
        }
        let folder = Folder::new();
        folder.launch("desktop-a", true, &[], FETCH_WITHIN);
        // Take its section out, so a resume has something to list again.
        folder.launch("desktop-b", true, &[], FETCH_WITHIN);
        assert_eq!(folder.sections(), ["desktop-b"]);

        folder.hub.answering.store(false, Ordering::Release);
        let waiting = tokio::spawn(place_for_launch(
            folder.lineage("desktop-c"),
            "claude-code".into(),
            folder.cwd.clone(),
            true,
            Duration::from_secs(60),
            |_| async { BTreeSet::new() },
        ));
        let held = folder.hub.held.clone();
        tokio::task::spawn_blocking(move || {
            held.lock().unwrap().recv_timeout(Duration::from_secs(30))
        })
        .await
        .unwrap()
        .expect("the start reaches the Hub");

        place_for_launch(
            folder.lineage("desktop-a"),
            "claude-code".into(),
            folder.cwd.clone(),
            false,
            FETCH_WITHIN,
            |_| async { BTreeSet::from(["desktop-b".to_owned()]) },
        )
        .await;
        assert!(!waiting.is_finished(), "the start still waits on the Hub");
        assert_eq!(folder.sections(), ["desktop-b", "desktop-a"]);
        waiting.abort();
    }
}
