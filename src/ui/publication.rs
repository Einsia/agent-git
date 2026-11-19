//! Progress belongs to one push invocation and ends before its output capture closes.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

thread_local! {
    static CURRENT: RefCell<Option<Sender<Event>>> = const { RefCell::new(None) };
}

static NEXT: AtomicU64 = AtomicU64::new(0);
const HEARTBEAT: Duration = Duration::from_secs(10);

enum Event {
    Start(u64, String),
    Detail(u64, String),
    End(u64, bool),
    Notice(String),
    ServerChecking,
    Flush(mpsc::SyncSender<()>),
    Stop,
}

pub(crate) struct Progress {
    sender: Option<Sender<Event>>,
    previous: Option<Sender<Event>>,
    worker: Option<JoinHandle<()>>,
}

impl Progress {
    pub(crate) fn start() -> Self {
        let enabled = !super::quiet() && !crate::commands::json::is_capturing();
        let (sender, worker) = if enabled {
            let (sender, receiver) = mpsc::channel();
            match std::thread::Builder::new()
                .name("push-progress".into())
                .spawn(move || render(receiver))
            {
                Ok(worker) => (Some(sender), Some(worker)),
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        };
        let previous = CURRENT.replace(sender.clone());
        Self {
            sender,
            previous,
            worker,
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        CURRENT.replace(self.previous.take());
        if let Some(sender) = &self.sender {
            let _ = sender.send(Event::Stop);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) struct Phase {
    sender: Option<Sender<Event>>,
    id: u64,
}

pub(crate) fn stage(label: impl Into<String>) -> Phase {
    let sender = CURRENT.with(|current| current.borrow().clone());
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    if let Some(sender) = &sender {
        let _ = sender.send(Event::Start(id, label.into()));
    }
    Phase { sender, id }
}

pub(crate) fn notice(message: impl Into<String>) {
    CURRENT.with(|current| {
        if let Some(sender) = current.borrow().as_ref() {
            let _ = sender.send(Event::Notice(message.into()));
        }
    });
}

pub(crate) fn server_checking() {
    CURRENT.with(|current| {
        if let Some(sender) = current.borrow().as_ref() {
            let _ = sender.send(Event::ServerChecking);
        }
    });
}

impl Phase {
    pub(crate) fn detail(&self, detail: impl Into<String>) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(Event::Detail(self.id, detail.into()));
        }
    }

    pub(crate) fn finish(mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Event::End(self.id, true));
            let (done, wait) = mpsc::sync_channel(0);
            if sender.send(Event::Flush(done)).is_ok() {
                let _ = wait.recv();
            }
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(Event::End(self.id, false));
        }
    }
}

struct Active {
    id: u64,
    label: String,
    detail: String,
    started: Instant,
}

impl Active {
    fn message(&self) -> String {
        if self.detail.is_empty() {
            self.label.clone()
        } else {
            format!("{}: {}", self.label, self.detail)
        }
    }

    fn ended(&self, complete: bool) -> String {
        format!(
            "{} — {} ({:.1}s)",
            self.message(),
            if complete { "done" } else { "stopped" },
            self.started.elapsed().as_secs_f64()
        )
    }
}

fn render(receiver: Receiver<Event>) {
    let bar = indicatif::ProgressBar::new_spinner();
    // The draw target owns visibility; stdout may have a different terminal than stderr.
    let spinner = if bar.is_hidden() {
        None
    } else {
        bar.set_style(
            indicatif::ProgressStyle::with_template("{spinner} {msg} [{elapsed_precise}]")
                .expect("valid progress template"),
        );
        Some(bar)
    };
    let tty = spinner.is_some();
    let interval = if tty {
        Duration::from_millis(100)
    } else {
        HEARTBEAT
    };
    let mut active: Option<Active> = None;
    let mut last_notice = Instant::now();
    let mut elapsed = Duration::ZERO;
    // Every line ends in a newline of its own. The bar's own printing pads to the terminal width
    // it reads and leaves the cursor on that row, so whatever is written next, the command's
    // final error included, continues the same row and is lost wherever the real width differs.
    let emit = |message: String| {
        if let Some(bar) = &spinner {
            bar.suspend(|| eprintln!("{message}"));
        } else {
            eprintln!("{message}");
        }
    };
    loop {
        match receiver.recv_timeout(interval) {
            Ok(Event::Start(id, label)) => {
                if let Some(old) = active.take() {
                    elapsed += old.started.elapsed();
                    emit(old.ended(false));
                }
                if let Some(bar) = &spinner {
                    bar.reset();
                }
                if !tty {
                    emit(format!("{label}…"));
                }
                active = Some(Active {
                    id,
                    label,
                    detail: String::new(),
                    started: Instant::now(),
                });
                last_notice = Instant::now();
            }
            Ok(Event::Detail(id, detail)) => {
                if let Some(current) = &mut active
                    && current.id == id
                {
                    current.detail = detail;
                }
            }
            Ok(Event::End(id, complete)) => {
                if active.as_ref().is_some_and(|current| current.id == id) {
                    let current = active.take().expect("active phase");
                    elapsed += current.started.elapsed();
                    if let Some(bar) = &spinner {
                        bar.finish_and_clear();
                    }
                    emit(current.ended(complete));
                }
            }
            Ok(Event::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Event::Notice(message)) => emit(message),
            Ok(Event::ServerChecking) => {
                if let Some(current) = &mut active {
                    current.detail = "server is checking the publication".into();
                }
            }
            Ok(Event::Flush(done)) => {
                let _ = done.send(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some(current) = &active {
            if let Some(bar) = &spinner {
                bar.set_message(current.message());
                bar.tick();
            } else if last_notice.elapsed() >= HEARTBEAT {
                emit(format!(
                    "{} — elapsed {:.1}s",
                    current.message(),
                    current.started.elapsed().as_secs_f64()
                ));
                last_notice = Instant::now();
            }
        }
    }
    if let Some(bar) = &spinner {
        bar.finish_and_clear();
    }
    if let Some(current) = active {
        elapsed += current.started.elapsed();
        emit(current.ended(false));
    }
    emit(format!(
        "Publication processing time: {:.1}s",
        elapsed.as_secs_f64()
    ));
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::IsTerminal;
    use std::os::fd::FromRawFd;
    use std::process::{Child, Command, Stdio};

    struct OwnedChild(Child);

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Redirecting stderr must expose an active phase even while stdout remains a terminal.
    /// The parent releases the phase only after observing its start, so final output cannot
    /// satisfy the visibility assertion.
    #[test]
    fn redirected_progress_is_visible_before_the_phase_finishes() {
        const SIGNAL: &str = "AGIT_PROGRESS_TEST_RELEASE";
        const LABEL: &str = "Checking synthetic publication";
        const LIMIT: Duration = Duration::from_secs(10);
        if let Some(release) = std::env::var_os(SIGNAL) {
            assert!(std::io::stdout().is_terminal());
            assert!(!std::io::stderr().is_terminal());
            let _progress = Progress::start();
            let phase = stage(LABEL);
            let deadline = Instant::now() + LIMIT;
            while !std::path::Path::new(&release).exists() {
                assert!(Instant::now() < deadline, "phase was never observed");
                std::thread::sleep(Duration::from_millis(5));
            }
            phase.finish();
            return;
        }

        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let _master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let directory = tempfile::tempdir().unwrap();
        let release = directory.path().join("release");
        let stderr = directory.path().join("stderr");
        let mut child = OwnedChild(
            Command::new(std::env::current_exe().unwrap())
                .env_clear()
                .env("TERM", "xterm")
                .env(SIGNAL, &release)
                .args([
                    "--exact",
                    "ui::publication::tests::redirected_progress_is_visible_before_the_phase_finishes",
                    "--nocapture",
                ])
                .stdin(Stdio::null())
                .stdout(slave)
                .stderr(File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + LIMIT;
        loop {
            let output = fs::read_to_string(&stderr).unwrap();
            assert!(child.0.try_wait().unwrap().is_none(), "{output}");
            if output.contains(&format!("{LABEL}…")) {
                assert!(!output.contains("done"), "{output}");
                fs::write(&release, b"").unwrap();
                break;
            }
            assert!(Instant::now() < deadline, "no active progress: {output}");
            std::thread::sleep(Duration::from_millis(5));
        }
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                let output = fs::read_to_string(&stderr).unwrap();
                assert!(status.success(), "{output}");
                assert!(output.contains(&format!("{LABEL} — done")), "{output}");
                break;
            }
            assert!(Instant::now() < deadline, "phase did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
