//! CRT descriptors and Win32 standard handles must describe the same capture boundary.

use super::{Version, document, emit_rejection_version, emit_version_checked};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};
use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};

const OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
static CAPTURE: Mutex<()> = Mutex::new(());

type InvalidParameterHandler =
    Option<unsafe extern "C" fn(*const u16, *const u16, *const u16, u32, usize)>;

unsafe extern "C" {
    fn _set_thread_local_invalid_parameter_handler(
        handler: InvalidParameterHandler,
    ) -> InvalidParameterHandler;
}

unsafe extern "C" fn invalid_parameter(
    _: *const u16,
    _: *const u16,
    _: *const u16,
    _: u32,
    _: usize,
) {
}

fn crt_handle(fd: i32) -> io::Result<HANDLE> {
    // An absent standard descriptor is a setup error, not a CRT process termination.
    let old = unsafe { _set_thread_local_invalid_parameter_handler(Some(invalid_parameter)) };
    let handle = unsafe { libc::get_osfhandle(fd) };
    unsafe { _set_thread_local_invalid_parameter_handler(old) };
    if handle == -1 || handle == -2 {
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "a standard output descriptor is unavailable",
        ))
    } else {
        Ok(handle as HANDLE)
    }
}

struct Descriptor(i32);

struct PreserveInput(HANDLE);

impl Drop for PreserveInput {
    fn drop(&mut self) {
        unsafe { SetStdHandle(STD_INPUT_HANDLE, self.0) };
    }
}

impl Descriptor {
    fn duplicate(fd: i32) -> io::Result<Self> {
        let _input = PreserveInput(unsafe { GetStdHandle(STD_INPUT_HANDLE) });
        let mut reserved = Vec::new();
        loop {
            let duplicate = unsafe { libc::dup(fd) };
            if duplicate < 0 {
                return Err(io::Error::last_os_error());
            }
            let duplicate = Self(duplicate);
            if duplicate.0 < 3 {
                reserved.push(duplicate);
                continue;
            }
            if unsafe { SetHandleInformation(crt_handle(duplicate.0)?, HANDLE_FLAG_INHERIT, 0) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
            return Ok(duplicate);
        }
    }

    fn from_handle(handle: OwnedHandle) -> io::Result<Self> {
        let _input = PreserveInput(unsafe { GetStdHandle(STD_INPUT_HANDLE) });
        let fd = unsafe {
            libc::open_osfhandle(
                handle.as_raw_handle() as isize,
                libc::O_WRONLY | libc::O_BINARY | libc::O_NOINHERIT,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            let _ = handle.into_raw_handle();
            let descriptor = Self(fd);
            if fd < 3 {
                Self::duplicate(fd)
            } else {
                Ok(descriptor)
            }
        }
    }

    fn install(&self, target: i32) -> io::Result<()> {
        if unsafe { libc::dup2(self.0, target) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe { libc::close(self.0) };
        }
    }
}

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    incomplete: bool,
}

fn read_pipe(mut file: File, done: Arc<AtomicBool>, limit: usize) -> Output {
    let mut output = Output::default();
    let mut buffer = [0; 16 * 1024];
    let mut tail = None;
    loop {
        let finished = done.load(Ordering::Acquire);
        let mut available = 0;
        let peek = unsafe {
            PeekNamedPipe(
                file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if peek == 0 {
            output.incomplete |=
                io::Error::last_os_error().raw_os_error() != Some(ERROR_BROKEN_PIPE as i32);
            return output;
        }
        if finished && tail.is_none() {
            tail = Some(available as usize);
        }
        if tail == Some(0) {
            // A descendant retaining a writer cannot extend the caller's capture lifetime.
            output.incomplete = true;
            return output;
        }
        if available == 0 {
            thread::sleep(std::time::Duration::from_millis(2));
            continue;
        }
        let amount = buffer
            .len()
            .min(available as usize)
            .min(tail.unwrap_or(usize::MAX));
        // This thread owns the only reader, so the peeked bytes cannot be consumed elsewhere.
        match file.read(&mut buffer[..amount]) {
            Ok(0) => {
                output.incomplete = true;
                return output;
            }
            Ok(count) => {
                if let Some(tail) = &mut tail {
                    *tail -= count;
                }
                let remaining = limit.saturating_sub(output.bytes.len());
                output
                    .bytes
                    .extend_from_slice(&buffer[..count.min(remaining)]);
                output.incomplete |= count > remaining;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                output.incomplete = true;
                return output;
            }
        }
    }
}

struct Pipe {
    writer: OwnedHandle,
    descriptor: Descriptor,
    reader: Option<JoinHandle<Output>>,
}

impl Pipe {
    fn new(done: Arc<AtomicBool>, limit: usize) -> io::Result<Self> {
        let mut read = std::ptr::null_mut();
        let mut write = std::ptr::null_mut();
        if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let reader = unsafe { File::from_raw_handle(read) };
        let writer = unsafe { OwnedHandle::from_raw_handle(write) };
        let descriptor = Descriptor::from_handle(writer.try_clone()?)?;
        let reader = thread::Builder::new()
            .name("agit-json-output".into())
            .spawn(move || read_pipe(reader, done, limit))?;
        Ok(Self {
            writer,
            descriptor,
            reader: Some(reader),
        })
    }
}

struct Capture {
    _lock: MutexGuard<'static, ()>,
    original_windows: [HANDLE; 2],
    original_crt: [HANDLE; 2],
    saved: [Descriptor; 2],
    pipes: [Option<Pipe>; 2],
    redirected: [bool; 2],
    done: Arc<AtomicBool>,
    restored: bool,
    incomplete: bool,
    retained: [bool; 2],
}

fn flush() -> bool {
    let out = io::stdout().flush();
    let err = io::stderr().flush();
    let crt = unsafe { libc::fflush(std::ptr::null_mut()) };
    out.is_ok() && err.is_ok() && crt == 0
}

impl Capture {
    fn start(limit: usize) -> io::Result<Self> {
        Self::start_with(limit, |slot, handle| {
            if unsafe { SetStdHandle(slot, handle) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        })
    }

    fn start_with(
        limit: usize,
        mut install: impl FnMut(u32, HANDLE) -> io::Result<()>,
    ) -> io::Result<Self> {
        let lock = match CAPTURE.try_lock() {
            Ok(lock) => lock,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "JSON capture is already active",
                ));
            }
        };
        let original_windows = unsafe {
            [
                GetStdHandle(STD_OUTPUT_HANDLE),
                GetStdHandle(STD_ERROR_HANDLE),
            ]
        };
        let original_crt = [crt_handle(1)?, crt_handle(2)?];
        let saved = [Descriptor::duplicate(1)?, Descriptor::duplicate(2)?];
        let mut capture = Self {
            _lock: lock,
            original_windows,
            original_crt,
            saved,
            pipes: [None, None],
            redirected: [false, false],
            done: Arc::new(AtomicBool::new(false)),
            restored: false,
            incomplete: false,
            retained: [false, false],
        };
        capture.pipes[0] = Some(Pipe::new(capture.done.clone(), limit)?);
        capture.pipes[1] = Some(Pipe::new(capture.done.clone(), limit)?);
        // Pending output belongs to the old destination and must not enter the command envelope.
        if !flush() {
            return Err(io::Error::other("cannot flush output before JSON capture"));
        }
        for (index, slot) in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .into_iter()
            .enumerate()
        {
            let pipe = capture.pipes[index].as_ref().unwrap();
            pipe.descriptor.install(index as i32 + 1)?;
            capture.redirected[index] = true;
            install(slot, pipe.writer.as_raw_handle())?;
        }
        Ok(capture)
    }

    fn restore(&mut self) {
        self.restore_with(|descriptor, target| descriptor.install(target));
    }

    fn restore_with(&mut self, mut install: impl FnMut(&Descriptor, i32) -> io::Result<()>) {
        if self.restored {
            return;
        }
        self.incomplete |= !flush();
        let mut restored_crt = self.original_crt;
        for (index, redirected) in self.redirected.iter().copied().enumerate() {
            if redirected {
                if install(&self.saved[index], index as i32 + 1).is_err() {
                    self.incomplete = true;
                    // Failed CRT replacement must not install the closed pre-capture handle.
                    restored_crt[index] =
                        crt_handle(self.saved[index].0).unwrap_or(std::ptr::null_mut());
                    self.retained[index] = true;
                } else if let Ok(handle) = crt_handle(index as i32 + 1) {
                    restored_crt[index] = handle;
                }
            }
        }
        for (index, slot) in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .into_iter()
            .enumerate()
        {
            let original = self.original_windows[index];
            let replacement = self
                .original_crt
                .iter()
                .position(|handle| *handle == original)
                .map(|index| restored_crt[index])
                .unwrap_or(original);
            self.incomplete |= unsafe { SetStdHandle(slot, replacement) } == 0;
        }
        self.restored = true;
    }

    fn finish(&mut self) -> (Output, Output) {
        self.restore();
        let readers = self.pipes.each_mut().map(|pipe| {
            pipe.take().and_then(|mut pipe| {
                let reader = pipe.reader.take();
                drop(pipe);
                reader
            })
        });
        self.done.store(true, Ordering::Release);
        let [mut out, mut err] = readers.map(|reader| {
            reader
                .and_then(|reader| reader.join().ok())
                .unwrap_or_else(|| Output {
                    incomplete: true,
                    ..Output::default()
                })
        });
        out.incomplete |= self.incomplete;
        err.incomplete |= self.incomplete;
        (out, err)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.restored || self.pipes.iter().any(Option::is_some) {
            let _ = self.finish();
        }
        for (index, retained) in self.retained.iter().copied().enumerate() {
            if retained {
                // A surviving standard destination belongs to the process after failed recovery.
                self.saved[index].0 = -1;
            }
        }
    }
}

pub(super) fn capture(command: &str, version: Version, f: impl FnOnce() -> i32) -> i32 {
    let mut capture = match Capture::start(OUTPUT_LIMIT) {
        Ok(capture) => capture,
        Err(error) => {
            return emit_rejection_version(
                command,
                version,
                crate::ExitCode::Precondition.as_i32(),
                &format!("cannot capture JSON output; the command was not run: {error}"),
                Vec::new(),
            );
        }
    };
    let scope = (version == Version::V2).then(crate::commands::fix::Scope::enter);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    let mut code = result.as_ref().copied().unwrap_or(101);
    let fixes = if result.is_ok() {
        scope.map(|scope| scope.finish(code)).unwrap_or_default()
    } else {
        drop(scope);
        Vec::new()
    };
    let (mut out, mut err) = capture.finish();
    if result.is_err() {
        err.bytes.extend_from_slice(b"\nerror command panicked\n");
    }
    if out.incomplete || err.incomplete {
        if code == 0 {
            code = crate::ExitCode::Precondition.as_i32();
        }
        out.bytes.clear();
        err.bytes
            .extend_from_slice(b"\nerror JSON output capture is incomplete\n");
    }
    if emit_version_checked(
        document(command, code, out.bytes, err.bytes),
        version,
        fixes,
    )
    .is_err()
        && code == 0
    {
        code = crate::ExitCode::Precondition.as_i32();
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

    const HELPER: &str = "commands::json::windows::tests::native_helper";
    // The non-ASCII characters are synthetic transcript and terminal-output fixture data.
    const TEXT: &str = "SYNTHETIC-UTF8-字🙂";

    fn helper(case: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", HELPER, "--ignored", "--nocapture"])
            .stdin(Stdio::null())
            .env("AGIT_WINDOWS_CAPTURE_CASE", case);
        command
    }

    fn native_case(case: &str) {
        let mut child = helper(case)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                child.kill().unwrap();
                panic!("native capture case did not finish: {case}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{case}: {output:?}");
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("RESTORED-STDOUT"),
            "{case} did not restore stdout"
        );
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("RESTORED-STDERR"),
            "{case} did not restore stderr"
        );
    }

    #[test]
    fn native_streams_capture_rust_crt_children_and_split_utf8() {
        native_case("streams");
    }

    #[test]
    fn native_capture_restores_partial_setup_and_unwind() {
        native_case("rollback");
    }

    #[test]
    fn native_capture_restores_distinct_crossed_and_absent_windows_slots() {
        native_case("aliases");
    }

    #[test]
    fn native_capture_bounds_output_and_inherited_writer_lifetimes() {
        native_case("bounds");
    }

    #[test]
    fn native_capture_closes_owned_handles_and_rejects_absent_crt() {
        native_case("handles");
    }

    #[test]
    fn native_panic_emits_one_failure_and_closes_the_reporter() {
        native_case("panic");
    }

    #[test]
    fn native_failed_restore_keeps_the_original_destination_live() {
        native_case("restore_failure");
    }

    #[test]
    fn native_console_handles_are_captured_and_restored() {
        native_case("conpty_host");
    }

    fn conpty_host() {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let output = thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let query = b"\x1b[6n";
            let mut matched = 0;
            loop {
                let count = reader.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
                for byte in &buffer[..count] {
                    if *byte == query[matched] {
                        matched += 1;
                        if matched == query.len() {
                            writer.write_all(b"\x1b[1;1R").unwrap();
                            writer.flush().unwrap();
                            matched = 0;
                        }
                    } else {
                        matched = usize::from(*byte == query[0]);
                    }
                }
            }
            bytes
        });
        let mut command = portable_pty::CommandBuilder::new(std::env::current_exe().unwrap());
        command.args(["--exact", HELPER, "--ignored", "--nocapture"]);
        command.env("AGIT_WINDOWS_CAPTURE_CASE", "console");
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                child.kill().unwrap();
                panic!("native console capture did not finish");
            }
            thread::sleep(Duration::from_millis(10));
        };
        drop(pair.master);
        let output = String::from_utf8_lossy(&output.join().unwrap()).into_owned();
        assert!(status.success(), "{output}");
        assert!(output.contains("RESTORED-STDOUT"), "{output}");
    }

    fn assert_complete(output: &(Output, Output)) {
        assert!(!output.0.incomplete);
        assert!(!output.1.incomplete);
    }

    fn bytes(fd: i32, text: &[u8]) {
        assert_eq!(
            unsafe { libc::write(fd, text.as_ptr().cast(), text.len() as u32) },
            text.len() as i32
        );
    }

    fn streams() {
        let mut capture = Capture::start(OUTPUT_LIMIT).unwrap();
        let encoded = format!("RUST-{TEXT}\n");
        for byte in encoded.bytes() {
            io::stdout().write_all(&[byte]).unwrap();
        }
        bytes(2, format!("CRT-{TEXT}\n").as_bytes());
        // This literal is synthetic Unicode data written through the buffered CRT.
        unsafe { libc::printf(c"BUFFERED-CRT-%s\n".as_ptr(), c"字🙂".as_ptr()) };
        let status = helper("child").status().unwrap();
        assert!(status.success());
        let output = capture.finish();
        assert_complete(&output);
        let out = String::from_utf8(output.0.bytes).unwrap();
        let err = String::from_utf8(output.1.bytes).unwrap();
        assert!(out.contains(&format!("RUST-{TEXT}")), "{out}");
        assert!(out.contains("BUFFERED-CRT-字🙂"), "{out}");
        assert!(out.contains(&format!("CHILD-{TEXT}")), "{out}");
        assert!(err.contains(&format!("CRT-{TEXT}")), "{err}");
        assert!(err.contains(&format!("CHILD-ERR-{TEXT}")), "{err}");
    }

    fn rollback() {
        let mut calls = 0;
        let failed = Capture::start_with(OUTPUT_LIMIT, |slot, handle| {
            calls += 1;
            if calls == 2 {
                Err(io::Error::other(
                    "synthetic standard handle installation failure",
                ))
            } else {
                assert_ne!(unsafe { SetStdHandle(slot, handle) }, 0);
                Ok(())
            }
        });
        assert!(failed.is_err());
        assert_eq!(calls, 2);
        let unwound = std::panic::catch_unwind(|| {
            let _capture = Capture::start(OUTPUT_LIMIT).unwrap();
            io::stdout().write_all(TEXT.as_bytes()).unwrap();
            panic!("synthetic capture unwind");
        });
        assert!(unwound.is_err());
        let mut capture = Capture::start(OUTPUT_LIMIT).unwrap();
        bytes(1, b"CRT-RESTORED\n");
        bytes(2, b"CRT-ERR-RESTORED\n");
        let output = capture.finish();
        assert_complete(&output);
        assert_eq!(output.0.bytes, b"CRT-RESTORED\n");
        assert_eq!(output.1.bytes, b"CRT-ERR-RESTORED\n");
    }

    fn restore_failure() {
        let mut capture = Capture::start(OUTPUT_LIMIT).unwrap();
        io::stdout().write_all(TEXT.as_bytes()).unwrap();
        capture.restore_with(|descriptor, target| {
            if target == 1 {
                Err(io::Error::other("synthetic descriptor restoration failure"))
            } else {
                descriptor.install(target)
            }
        });
        let output = capture.finish();
        assert!(output.0.incomplete);
        assert!(output.1.incomplete);
        drop(capture);
        io::stdout().write_all(b"SURVIVING-DESTINATION\n").unwrap();
    }

    fn aliases() {
        let original_out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        let original_err = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
        let out_matches = original_out == crt_handle(1).unwrap();
        let err_matches = original_err == crt_handle(2).unwrap();
        let separate = tempfile::tempfile().unwrap();
        for arrangement in [0, 1, 2, 3] {
            let current = [crt_handle(1).unwrap(), crt_handle(2).unwrap()];
            let slots = match arrangement {
                0 => [current[1], current[0]],
                1 => [current[0], current[0]],
                2 => [separate.as_raw_handle(), separate.as_raw_handle()],
                _ => [std::ptr::null_mut(), INVALID_HANDLE_VALUE],
            };
            unsafe {
                assert_ne!(SetStdHandle(STD_OUTPUT_HANDLE, slots[0]), 0);
                assert_ne!(SetStdHandle(STD_ERROR_HANDLE, slots[1]), 0);
            }
            let mut capture = Capture::start(OUTPUT_LIMIT).unwrap();
            io::stdout().write_all(TEXT.as_bytes()).unwrap();
            bytes(2, TEXT.as_bytes());
            let output = capture.finish();
            assert_complete(&output);
            assert_eq!(output.0.bytes, TEXT.as_bytes());
            assert_eq!(output.1.bytes, TEXT.as_bytes());
            let restored = [crt_handle(1).unwrap(), crt_handle(2).unwrap()];
            let expected = match arrangement {
                0 => [restored[1], restored[0]],
                1 => [restored[0], restored[0]],
                _ => slots,
            };
            unsafe {
                assert_eq!(GetStdHandle(STD_OUTPUT_HANDLE), expected[0]);
                assert_eq!(GetStdHandle(STD_ERROR_HANDLE), expected[1]);
            }
        }
        unsafe {
            SetStdHandle(
                STD_OUTPUT_HANDLE,
                if out_matches {
                    crt_handle(1).unwrap()
                } else {
                    original_out
                },
            );
            SetStdHandle(
                STD_ERROR_HANDLE,
                if err_matches {
                    crt_handle(2).unwrap()
                } else {
                    original_err
                },
            );
        }
    }

    fn bounds() {
        let mut capture = Capture::start(1024).unwrap();
        let out = thread::spawn(|| {
            for _ in 0..64 {
                io::stdout().write_all(&[b'o'; 8192]).unwrap();
            }
        });
        let err = thread::spawn(|| {
            for _ in 0..64 {
                bytes(2, &[b'e'; 8192]);
            }
        });
        out.join().unwrap();
        err.join().unwrap();
        let output = capture.finish();
        assert!(output.0.incomplete && output.1.incomplete);
        assert_eq!(output.0.bytes.len(), 1024);
        assert_eq!(output.1.bytes.len(), 1024);
        drop(capture);
        for child_case in ["sleep", "write_forever"] {
            let mut capture = Capture::start(1024).unwrap();
            let mut child = helper(child_case).spawn().unwrap();
            thread::sleep(Duration::from_millis(50));
            let start = Instant::now();
            let output = capture.finish();
            let elapsed = start.elapsed();
            let _ = child.kill();
            let _ = child.wait();
            assert!(elapsed < Duration::from_secs(5));
            assert!(output.0.incomplete || output.1.incomplete);
        }
    }

    fn handle_count() -> u32 {
        let mut count = 0;
        assert_ne!(
            unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) },
            0
        );
        count
    }

    fn handles() {
        let mut warmup = Capture::start(OUTPUT_LIMIT).unwrap();
        assert_complete(&warmup.finish());
        drop(warmup);
        let before = handle_count();
        for _ in 0..32 {
            let mut capture = Capture::start(OUTPUT_LIMIT).unwrap();
            bytes(1, TEXT.as_bytes());
            assert_complete(&capture.finish());
        }
        assert_eq!(handle_count(), before);
        let input = Descriptor::duplicate(0).unwrap();
        let input_slot = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        assert_eq!(unsafe { libc::close(0) }, 0);
        let missing_input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut capture = Capture::start(OUTPUT_LIMIT).unwrap();
        assert!(capture.saved.iter().all(|fd| fd.0 >= 3));
        assert!(crt_handle(0).is_err());
        assert_eq!(unsafe { GetStdHandle(STD_INPUT_HANDLE) }, missing_input);
        bytes(1, TEXT.as_bytes());
        assert_complete(&capture.finish());
        drop(capture);
        input.install(0).unwrap();
        unsafe {
            SetStdHandle(
                STD_INPUT_HANDLE,
                if input_slot.is_null() || input_slot == INVALID_HANDLE_VALUE {
                    input_slot
                } else {
                    crt_handle(0).unwrap()
                },
            )
        };
        let saved = Descriptor::duplicate(1).unwrap();
        assert_eq!(unsafe { libc::close(1) }, 0);
        assert!(Capture::start(OUTPUT_LIMIT).is_err());
        saved.install(1).unwrap();
        unsafe { SetStdHandle(STD_OUTPUT_HANDLE, crt_handle(1).unwrap()) };
    }

    fn panic_document() {
        let saved = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        let mut file = tempfile::tempfile().unwrap();
        unsafe { SetStdHandle(STD_OUTPUT_HANDLE, file.as_raw_handle()) };
        let mut reporter = None;
        let code = capture("synthetic", Version::V2, || {
            reporter = crate::commands::fix::current_reporter();
            panic!("synthetic command panic");
        });
        assert_eq!(code, 101);
        assert!(crate::commands::fix::current_reporter().is_none());
        assert!(reporter.is_some());
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0)).unwrap();
        let value: serde_json::Value = serde_json::from_reader(&mut file).unwrap();
        assert_eq!(value["schema_version"], 2);
        assert_eq!(value["exit_code"], 101);
        assert_eq!(value["ok"], false);
        assert_eq!(value["fix"], serde_json::json!([]));
        assert!(
            value["diagnostics"]["stderr"]
                .to_string()
                .contains("synthetic command panic")
        );
        // The original CRT destination remains owned by its restored descriptor.
        let restored = if saved == INVALID_HANDLE_VALUE || saved.is_null() {
            saved
        } else {
            crt_handle(1).unwrap()
        };
        unsafe { SetStdHandle(STD_OUTPUT_HANDLE, restored) };
    }

    fn console() {
        use windows_sys::Win32::System::Console::GetConsoleMode;
        let mut mode = 0;
        assert_ne!(
            unsafe { GetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), &mut mode) },
            0
        );
        let mut capture = Capture::start(OUTPUT_LIMIT).unwrap();
        assert_eq!(
            unsafe { GetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), &mut mode) },
            0
        );
        io::stdout().write_all(TEXT.as_bytes()).unwrap();
        let output = capture.finish();
        assert_complete(&output);
        assert_eq!(output.0.bytes, TEXT.as_bytes());
        assert_ne!(
            unsafe { GetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), &mut mode) },
            0
        );
    }

    #[test]
    #[ignore]
    fn native_helper() {
        match std::env::var("AGIT_WINDOWS_CAPTURE_CASE").unwrap().as_str() {
            "streams" => streams(),
            "rollback" => rollback(),
            "restore_failure" => restore_failure(),
            "aliases" => aliases(),
            "bounds" => bounds(),
            "handles" => handles(),
            "panic" => panic_document(),
            "console" => console(),
            "conpty_host" => conpty_host(),
            "child" => {
                io::stdout()
                    .write_all(format!("CHILD-{TEXT}\n").as_bytes())
                    .unwrap();
                io::stderr()
                    .write_all(format!("CHILD-ERR-{TEXT}\n").as_bytes())
                    .unwrap();
                std::process::exit(0);
            }
            "sleep" => thread::sleep(Duration::from_secs(60)),
            "write_forever" => loop {
                if io::stdout().write_all(&[b'w'; 8192]).is_err() {
                    std::process::exit(0);
                }
            },
            case => panic!("unknown native case: {case}"),
        }
        io::stdout().write_all(b"RESTORED-STDOUT\n").unwrap();
        io::stderr().write_all(b"RESTORED-STDERR\n").unwrap();
        std::process::exit(0);
    }
}
