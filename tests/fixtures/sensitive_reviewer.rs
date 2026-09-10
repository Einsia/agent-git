use std::io::{Read, Write};
use std::path::PathBuf;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("synthetic reviewer contract failed: {error}");
            std::process::ExitCode::from(91)
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args == ["--safe-mode", "--help"] {
        println!(
            "--print --safe-mode --settings --tools --disable-slash-commands \
             --strict-mcp-config --mcp-config --no-chrome --no-session-persistence \
             --permission-mode --permission-prompts --output-format --json-schema"
        );
        return Ok(());
    }
    let expected = [
        "--print",
        "--safe-mode",
        "--settings",
        r#"{"disableAllHooks":true}"#,
        "--tools",
        "",
        "--disable-slash-commands",
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers":{}}"#,
        "--no-chrome",
        "--no-session-persistence",
        "--permission-mode",
        "dontAsk",
        "--permission-prompts",
        "none",
        "--output-format",
        "json",
        "--json-schema",
    ];
    if args.len() != expected.len() + 1 || args[..expected.len()] != expected {
        return Err("native argument boundaries were not preserved".into());
    }
    if std::env::vars_os().any(|(key, _)| {
        let key = key.to_string_lossy().to_ascii_uppercase();
        key == "AGIT_SESSION" || key == "AGIT_MERGE_TX" || key.starts_with("GIT_")
    }) {
        return Err("workspace identity reached the reviewer".into());
    }
    let root = PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR").ok_or("no fixture root")?);
    let cwd = std::env::current_dir()?;
    let home = PathBuf::from(std::env::var_os("AGIT_HOME").ok_or("no private agent home")?);
    if home.parent() != Some(cwd.as_path()) || !home.is_dir() {
        return Err("agent state is not isolated inside the review directory".into());
    }
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    std::fs::write(root.join("input"), input)?;
    std::fs::write(root.join("cwd"), cwd.to_str().ok_or("non-Unicode cwd")?)?;
    std::fs::write(
        root.join("agit-home"),
        home.to_str().ok_or("non-Unicode home")?,
    )?;
    std::fs::write(root.join("schema"), &args[expected.len()])?;
    std::io::stdout().write_all(&std::fs::read(root.join("response.json"))?)?;
    Ok(())
}
