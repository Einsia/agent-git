//! Noninteractive helpers communicate through handles without creating a console window.

pub(crate) fn command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    command
}

/// How far [`keep_inherited_handles_private`] looks. Handle values are multiples of 4, so this is
/// 65,536 probes at most, and a process with that many open handles at start-up is not one a caller
/// would start. A handle above the limit keeps its flag and reaches children as it did before.
#[cfg(windows)]
const HANDLE_SCAN_LIMIT: usize = 0x4_0000;

/// Every child gets the handles its command names and no others. Windows hands each inheritable
/// handle of the parent to every child it starts, and this process may have received a caller's
/// handles the same way, so a helper that runs on after a command's output has been read would keep
/// that output open, and the reader waits on it. This clears the flag on everything open at
/// start-up. It holds for start-up only: a handle made inheritable later still reaches children. A
/// child that asks for `Stdio::inherit()` still gets its own copy. Called at the start of `main`,
/// before any thread or child starts.
pub fn keep_inherited_handles_private() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
        // The scan ends once it has found every open handle, which is almost always early.
        let mut open = 0;
        if unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut open) } == 0 {
            open = u32::MAX;
        }
        let mut found = 0;
        for value in (4..HANDLE_SCAN_LIMIT).step_by(4) {
            if found >= open {
                break;
            }
            let handle = value as HANDLE;
            let mut flags = 0;
            if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
                continue;
            }
            found += 1;
            if flags & HANDLE_FLAG_INHERIT != 0 {
                // A handle that cannot change keeps its flag; the process still runs.
                unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn helper_has_no_console_window_and_keeps_output() {
        const TEST: &str =
            "infra::background::tests::helper_has_no_console_window_and_keeps_output";
        const CHILD: &str = "AGIT_BACKGROUND_CONSOLE_TEST";
        if std::env::var_os(CHILD).is_some() {
            assert!(unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() }.is_null());
            println!("background-output");
            return;
        }
        let output = super::command(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("background-output"));
    }

    /// The parent holds two things a helper could keep open: its own output, and a pipe it
    /// received as a plain inherited handle, the way a caller's own output reaches it.
    #[test]
    fn a_helper_that_outlives_its_parent_does_not_hold_the_parent_output() {
        use std::io::Read;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        const TEST: &str = "infra::background::tests::\
            a_helper_that_outlives_its_parent_does_not_hold_the_parent_output";
        const ROLE: &str = "AGIT_BACKGROUND_HANDLES_TEST";
        let exe = std::env::current_exe().unwrap();
        match std::env::var(ROLE).as_deref() {
            Ok("helper") => {
                std::thread::sleep(std::time::Duration::from_secs(60));
                return;
            }
            Ok("parent") => {
                super::keep_inherited_handles_private();
                super::command(&exe)
                    .args(["--exact", TEST, "--nocapture"])
                    .env(ROLE, "helper")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                println!("parent-output");
                return;
            }
            _ => {}
        }
        let (mut reader, writer) = std::io::pipe().unwrap();
        assert_ne!(
            unsafe {
                SetHandleInformation(
                    writer.as_raw_handle(),
                    HANDLE_FLAG_INHERIT,
                    HANDLE_FLAG_INHERIT,
                )
            },
            0
        );
        let started = std::time::Instant::now();
        let output = super::command(&exe)
            .args(["--exact", TEST, "--nocapture"])
            .env(ROLE, "parent")
            .output()
            .unwrap();
        drop(writer);
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("parent-output"));
        let mut passed_on = Vec::new();
        reader.read_to_end(&mut passed_on).unwrap();
        // The helper sleeps for a minute, so this fails at about a minute if it holds either one.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the parent's handles stayed open for {:?}",
            started.elapsed()
        );
    }

    /// A child that shares its parent's output, a pipe or a console, still reaches it.
    #[cfg(feature = "rc")]
    #[test]
    fn a_child_that_inherits_output_still_writes_to_it_after_the_sweep() {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        use std::io::{Read, Write};
        const TEST: &str = "infra::background::tests::\
            a_child_that_inherits_output_still_writes_to_it_after_the_sweep";
        const ROLE: &str = "AGIT_BACKGROUND_SWEEP_TEST";
        const MARK: &str = "inherited-output-arrived";
        let exe = std::env::current_exe().unwrap();
        match std::env::var(ROLE).as_deref() {
            Ok("child") => {
                println!("{MARK}");
                return;
            }
            Ok("parent") => {
                super::keep_inherited_handles_private();
                let status = std::process::Command::new(&exe)
                    .args(["--exact", TEST, "--nocapture"])
                    .env(ROLE, "child")
                    .status()
                    .unwrap();
                assert!(status.success());
                return;
            }
            _ => {}
        }
        let output = std::process::Command::new(&exe)
            .args(["--exact", TEST, "--nocapture"])
            .env(ROLE, "parent")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(MARK),
            "{output:?}"
        );

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let screen = std::thread::spawn(move || {
            // The console asks where its cursor is before it starts the child, and waits for this.
            const QUERY: &[u8] = b"\x1b[6n";
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let mut answered = 0;
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
                let asked = bytes
                    .windows(QUERY.len())
                    .filter(|part| *part == QUERY)
                    .count();
                while answered < asked {
                    let _ = writer.write_all(b"\x1b[1;1R");
                    let _ = writer.flush();
                    answered += 1;
                }
            }
            bytes
        });
        let mut command = CommandBuilder::new(&exe);
        command.args(["--exact", TEST, "--nocapture"]);
        command.env(ROLE, "parent");
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                panic!("the child on the console did not finish");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        drop(pair.master);
        let screen = String::from_utf8_lossy(&screen.join().unwrap()).into_owned();
        assert!(status.success(), "{screen}");
        assert!(screen.contains(MARK), "{screen}");
    }
}
