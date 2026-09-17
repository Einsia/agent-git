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
}
