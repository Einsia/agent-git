//! Machine-readable CLI output.
//!
//! Every `agit --json <command>` invocation is represented by one JSON document
//! with this stable top-level shape.  Command implementations can keep their
//! human-oriented rendering for now; the entry point captures it and places it
//! under `result`, while commands that already produce JSON retain that value
//! as `result.value`.
//!
//! ```json
//! {
//!   "schema": "cli-output",
//!   "schema_version": 1,
//!   "command": "status",
//!   "ok": true,
//!   "exit_code": 0,
//!   "result": { "format": "text", "kind": "status", "lines": [] },
//!   "diagnostics": { "stderr": [] }
//! }
//! ```

use serde::Serialize;
use serde_json::Value;
use std::io::{Read, Write};

pub const SCHEMA_NAME: &str = "cli-output";
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
struct Document {
    schema: &'static str,
    schema_version: u32,
    command: String,
    ok: bool,
    exit_code: i32,
    result: ResultValue,
    diagnostics: Diagnostics,
}

#[derive(Debug, Serialize)]
#[serde(tag = "format", rename_all = "snake_case")]
enum ResultValue {
    /// The command emitted one complete JSON value.  Existing structured
    /// command output is preserved without a second JSON encoding layer.
    Json { kind: String, value: Value },
    /// The command emitted one JSON value per non-empty line (JSONL).
    /// Values are decoded so agents do not need a second line parser.
    JsonLines { kind: String, values: Vec<Value> },
    /// The command emitted ordinary terminal lines.  Blank and whitespace-only
    /// lines are omitted as presentation noise; non-empty lines keep their
    /// original content (including meaningful indentation).
    Text { kind: String, lines: Vec<String> },
    /// No stdout was produced (the useful information may be in diagnostics).
    Empty { kind: String },
}

#[derive(Debug, Serialize)]
struct Diagnostics {
    /// stderr is classified by the standard UI prefixes (`error`, `note`,
    /// and `→`) so agents can act on diagnostics without parsing prose.
    stderr: Vec<Diagnostic>,
}

#[derive(Debug, Serialize)]
struct Diagnostic {
    level: &'static str,
    message: String,
}

/// Capture a command's terminal output and emit one JSON document.
///
/// Unix file descriptors are used instead of changing every `println!` call in
/// the command tree.  Reader threads prevent a verbose command from blocking
/// when the pipe buffer fills.  A platform without this capture implementation
/// is rejected before the command runs; emitting an empty envelope after
/// leaking human output would violate the single-document contract.
pub fn capture(command: &str, f: impl FnOnce() -> i32) -> i32 {
    #[cfg(unix)]
    {
        capture_unix(command, f)
    }
    #[cfg(not(unix))]
    {
        let _ = f;
        emit_rejection(
            command,
            crate::ExitCode::Precondition.as_i32(),
            "--json output is not supported on this platform yet; the command was not run",
        )
    }
}

/// Emit one machine-readable rejection without invoking the command.
///
/// This is used for combinations whose lifecycle cannot be represented by a
/// single JSON document (for example, a long-running runtime attached to a
/// terminal), and for platforms where stdout/stderr capture is unavailable.
pub fn emit_rejection(command: &str, code: i32, message: &str) -> i32 {
    emit(Document {
        schema: SCHEMA_NAME,
        schema_version: SCHEMA_VERSION,
        command: command.to_string(),
        ok: false,
        exit_code: code,
        result: ResultValue::Empty {
            kind: "rejected".to_string(),
        },
        diagnostics: Diagnostics {
            stderr: vec![Diagnostic {
                level: "error",
                message: message.to_owned(),
            }],
        },
    });
    code
}

/// Return a reason when a command cannot honor the one-document JSON contract
/// because it owns an interactive or long-running terminal lifecycle.
///
/// Callers can opt into the preparation-only forms (`--no-launch`, `--manual`,
/// `--dry-run`, or `--detach`) and then receive a normal envelope.  Rejection
/// happens before migration or command dispatch, so no partial state or leaked
/// terminal output is produced.
pub fn incompatible(command: &super::Commands) -> Option<&'static str> {
    use super::Commands;

    match command {
        Commands::Login(args) if !args.with_token => Some(
            "--json cannot wrap the interactive login flow; use `agit login --with-token < token.txt>` or omit --json",
        ),
        Commands::Rc(args) => match &args.action {
            super::rc::Action::Start(start) if !start.detach => Some(
                "--json cannot wrap the foreground RC daemon; use `agit rc start --detach` or omit --json",
            ),
            _ => None,
        },
        Commands::Show(args) if args.tui => {
            Some("--json cannot wrap `show --tui`; omit --tui for line output or omit --json")
        }
        Commands::Run(args) if !args.no_launch => {
            Some("--json cannot wrap a launched runtime; use `agit run --no-launch` or omit --json")
        }
        Commands::Resume(args) if !args.no_launch => Some(
            "--json cannot wrap a launched runtime; use `agit resume --no-launch` or omit --json",
        ),
        Commands::New(args) if !args.no_launch => {
            Some("--json cannot wrap a launched runtime; use `agit new --no-launch` or omit --json")
        }
        Commands::Fork(args) if args.resume && !args.no_launch => Some(
            "--json cannot wrap a launched runtime; use `agit fork --resume --no-launch` or omit --json",
        ),
        Commands::Merge(args)
            if args.source.is_some()
                && !args.manual
                && !args.dry_run
                && !args.status
                && !args.continue_
                && !args.abort
                && args.cmd.is_none() =>
        {
            Some(
                "--json cannot wrap a launched merge agent; use `agit merge --manual` or `--dry-run`, or omit --json",
            )
        }
        _ => None,
    }
}

/// Emit a JSON envelope for a command-line parse error. Clap normally exits
/// before `main` can enter [`capture`], but `--json` callers still need one
/// machine-readable document rather than a usage blob on stderr.
pub fn emit_parse_error(command: String, code: i32, message: &str) -> i32 {
    emit(Document {
        schema: SCHEMA_NAME,
        schema_version: SCHEMA_VERSION,
        command,
        ok: false,
        exit_code: code,
        result: ResultValue::Empty {
            kind: "parse_error".to_string(),
        },
        diagnostics: Diagnostics {
            stderr: diagnostics(message),
        },
    });
    code
}

/// Best-effort command name for a parse error, before clap has produced a
/// typed [`Commands`] value.  Global options with values are skipped so `-C
/// path --json status` still reports `status`.
pub fn command_from_argv(args: &[std::ffi::OsString]) -> String {
    let mut skip_value = false;
    for raw in args.iter().skip(1) {
        let arg = raw.to_string_lossy();
        if skip_value {
            skip_value = false;
            continue;
        }
        if arg == "-C" || arg == "--directory" {
            skip_value = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return arg.into_owned();
    }
    "cli".to_string()
}

#[cfg(unix)]
fn capture_unix(command: &str, f: impl FnOnce() -> i32) -> i32 {
    use std::os::fd::RawFd;
    use std::thread;

    fn pipe() -> (RawFd, RawFd) {
        let mut fds = [0; 2];
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "creating a JSON capture pipe failed");
        (fds[0], fds[1])
    }

    fn duplicate(fd: RawFd) -> RawFd {
        let copy = unsafe { libc::dup(fd) };
        assert!(copy >= 0, "duplicating a standard stream failed");
        copy
    }

    fn redirect(fd: RawFd, replacement: RawFd) {
        let rc = unsafe { libc::dup2(replacement, fd) };
        assert_eq!(rc, fd, "redirecting a standard stream failed");
    }

    let (out_read, out_write) = pipe();
    let (err_read, err_write) = pipe();
    let saved_out = duplicate(libc::STDOUT_FILENO);
    let saved_err = duplicate(libc::STDERR_FILENO);
    redirect(libc::STDOUT_FILENO, out_write);
    redirect(libc::STDERR_FILENO, err_write);
    unsafe {
        libc::close(out_write);
        libc::close(err_write);
    }

    let out_thread = thread::spawn(move || read_fd(out_read));
    let err_thread = thread::spawn(move || read_fd(err_read));
    let code = f();
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    redirect(libc::STDOUT_FILENO, saved_out);
    redirect(libc::STDERR_FILENO, saved_err);
    unsafe {
        libc::close(saved_out);
        libc::close(saved_err);
    }

    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    emit(document(command, code, stdout, stderr));
    code
}

#[cfg(unix)]
fn read_fd(fd: std::os::fd::RawFd) -> Vec<u8> {
    use std::os::fd::FromRawFd;
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    bytes
}

fn document(command: &str, code: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> Document {
    let stdout_text = String::from_utf8_lossy(&stdout).into_owned();
    let stderr_text = String::from_utf8_lossy(&stderr).into_owned();
    let stdout_lines = lines(&stdout_text);
    let stderr_lines = diagnostics(&stderr_text);
    let kind = command.to_string();
    let result = match serde_json::from_slice::<Value>(&stdout) {
        Ok(value) if !stdout_text.trim().is_empty() => ResultValue::Json { kind, value },
        _ if stdout_text.trim().is_empty() => ResultValue::Empty { kind },
        _ => match json_lines(&stdout_text) {
            Some(values) => ResultValue::JsonLines { kind, values },
            None => ResultValue::Text {
                kind,
                lines: stdout_lines,
            },
        },
    };
    Document {
        schema: SCHEMA_NAME,
        schema_version: SCHEMA_VERSION,
        command: command.to_string(),
        ok: code == 0,
        exit_code: code,
        result,
        diagnostics: Diagnostics {
            stderr: stderr_lines,
        },
    }
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn json_lines(text: &str) -> Option<Vec<Value>> {
    let mut values = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        values.push(serde_json::from_str(line).ok()?);
    }
    (!values.is_empty()).then_some(values)
}

fn diagnostics(text: &str) -> Vec<Diagnostic> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (level, message) = if let Some(message) = line
                .strip_prefix("error ")
                .or_else(|| line.strip_prefix("error: "))
            {
                ("error", message)
            } else if let Some(message) = line
                .strip_prefix("note ")
                .or_else(|| line.strip_prefix("warning: "))
            {
                ("warning", message)
            } else if let Some(message) = line.trim_start().strip_prefix("→ ") {
                ("hint", message)
            } else {
                ("info", line)
            };
            Diagnostic {
                level,
                message: message.to_owned(),
            }
        })
        .collect()
}

fn emit(document: Document) {
    let mut stdout = std::io::stdout().lock();
    // Serialization of these in-memory values cannot fail; if stdout itself
    // is closed, the normal process-level write error is the only useful one.
    let _ = serde_json::to_writer_pretty(&mut stdout, &document);
    let _ = writeln!(stdout);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_output_is_stored_as_lines() {
        let d = document("status", 0, b"\none\n\n  two\n \t \n".to_vec(), Vec::new());
        match d.result {
            ResultValue::Text { lines, .. } => assert_eq!(lines, vec!["one", "  two"]),
            _ => panic!("expected text result"),
        }
    }

    #[test]
    fn blank_stdout_is_empty_instead_of_a_noisy_text_result() {
        let d = document("status", 0, b" \n\n\t\n".to_vec(), Vec::new());
        assert!(matches!(d.result, ResultValue::Empty { .. }));
    }

    #[test]
    fn existing_json_is_kept_as_a_value() {
        let d = document("view", 0, br#"[{"index":1}]"#.to_vec(), Vec::new());
        match d.result {
            ResultValue::Json { value, .. } => assert_eq!(value[0]["index"], 1),
            _ => panic!("expected JSON result"),
        }
    }

    #[test]
    fn invalid_or_empty_output_never_breaks_the_envelope() {
        let empty = document("x", 4, Vec::new(), b"error\n".to_vec());
        assert!(matches!(empty.result, ResultValue::Empty { .. }));
        assert!(!empty.ok);
        let text = document("x", 0, b"not json\n".to_vec(), Vec::new());
        assert!(matches!(text.result, ResultValue::Text { .. }));
    }

    #[test]
    fn jsonl_output_is_decoded_without_a_second_line_parser() {
        let d = document(
            "export",
            0,
            br#"{"n":1}
{"n":2}
"#
            .to_vec(),
            Vec::new(),
        );
        match d.result {
            ResultValue::JsonLines { values, .. } => {
                assert_eq!(
                    values,
                    vec![serde_json::json!({"n": 1}), serde_json::json!({"n": 2})]
                );
            }
            _ => panic!("expected JSONL result"),
        }
    }

    #[test]
    fn stderr_diagnostics_have_actionable_levels() {
        let d = document(
            "x",
            2,
            Vec::new(),
            "error failed\nnote check again\n  → agit login\nraw detail\n"
                .as_bytes()
                .to_vec(),
        );
        assert_eq!(d.diagnostics.stderr.len(), 4);
        assert_eq!(d.diagnostics.stderr[0].level, "error");
        assert_eq!(d.diagnostics.stderr[1].level, "warning");
        assert_eq!(d.diagnostics.stderr[2].level, "hint");
        assert_eq!(d.diagnostics.stderr[2].message, "agit login");
        assert_eq!(d.diagnostics.stderr[3].level, "info");
    }

    #[test]
    fn parse_errors_have_a_single_machine_readable_document() {
        let d = diagnostics("error: unexpected argument '--wat'\n\nUsage: agit status");
        assert_eq!(d[0].level, "error");
        assert_eq!(d[0].message, "unexpected argument '--wat'");
        assert_eq!(d[1].level, "info");
    }

    #[test]
    fn command_name_skips_global_option_values() {
        let args = vec![
            "agit".into(),
            "-C".into(),
            "/tmp/project".into(),
            "--json".into(),
            "status".into(),
        ];
        assert_eq!(command_from_argv(&args), "status");
    }

    #[test]
    fn implementation_points_at_the_checked_in_schema() {
        let schema: Value = serde_json::from_str(include_str!("../../docs/cli-json-schema.json"))
            .expect("the checked-in CLI schema must remain valid JSON");
        assert_eq!(schema["title"], "agit CLI machine-readable output");
        assert_eq!(
            schema["$defs"]["json_result"]["properties"]["format"]["const"],
            "json"
        );
        assert_eq!(
            schema["$defs"]["json_lines_result"]["properties"]["format"]["const"],
            "json_lines"
        );
        assert_eq!(
            schema["$defs"]["text_result"]["properties"]["format"]["const"],
            "text"
        );
        assert_eq!(
            schema["$defs"]["empty_result"]["properties"]["format"]["const"],
            "empty"
        );
    }
}
