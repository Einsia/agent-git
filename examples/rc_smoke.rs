//! End-to-end smoke test for the RC supervisor, without a hub.
//!
//! Launches a real harness in a temp directory, drives it the way a viewer
//! would, and prints every protocol frame the session emits. This is the test
//! that proves the two-source design actually works: ephemeral events from the
//! driver's stdout, authoritative `item.completed` from the transcript file.
//!
//! Usage: `cargo run --example rc_smoke -- [runtime] [prompt]`

use agit::protocol::{Frame, method};
use agit::protocol::{SessionInfo, SessionStatus};
use agit::rc::harness::LaunchSpec;
use agit::rc::supervisor::{Command, Session};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let runtime = args.next().unwrap_or_else(|| "claude-code".into());
    let prompt = args
        .next()
        .unwrap_or_else(|| "Run `echo agit-rc-works` with Bash, then reply with just: ok".into());

    let dir = std::env::temp_dir().join(format!("agit-rc-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    println!("cwd      {}", dir.display());
    println!("runtime  {runtime}");
    println!("prompt   {prompt}\n");

    let now = chrono::Utc::now().to_rfc3339();
    let info = SessionInfo {
        session_id: "agit-smoke".into(),
        workspace_id: "ws-smoke".into(),
        project_id: Some("p1".into()),
        runtime: runtime.clone(),
        agent: None,
        branch: None,
        status: SessionStatus::Idle,
        last_seq: 0,
        gist: None,
        dangerous: false,
        permission_mode: None,
        created_at: now.clone(),
        updated_at: now,
    };
    let spec = LaunchSpec {
        cwd: dir.clone(),
        resume_from: None,
        agit_session: None,
        model: std::env::var("AGIT_SMOKE_MODEL").ok(),
        dangerous: false,
        permission_mode: None,
    };

    let (ftx, mut frx) = mpsc::channel::<Frame>(4096);
    let (notes_tx, _notes_rx) = tokio::sync::mpsc::channel(16);
    // Confinement is live in the real daemon (an owner unbinding a directory takes effect
    // immediately). The smoke script has one workspace, so a single send is enough; `_conf_tx`
    // must stay alive — dropping it makes the session's `Receiver` see the channel close.
    let (_conf_tx, conf_rx) = tokio::sync::watch::channel(agit::rc::Confinement {
        roots: agit::rc::policy::CanonicalRoots::from_untrusted([dir.clone()]),
        operator_heads: Default::default(),
    });
    let (_settlement_tx, settlement_rx) =
        tokio::sync::watch::channel(agit::rc::supervisor::SettlementState {
            epoch: 1,
            // The smoke harness has no hub negotiation and no repository
            // lineage, so settlement remains disabled.
            agent_identity_v1: false,
            session_start_idempotency_v1: false,
        });
    // Generation number: distinguishes which generation exited when the same logical id is
    // relaunched. The smoke script has only one.
    let session = Session::launch(
        info,
        spec,
        ftx,
        notes_tx,
        conf_rx,
        settlement_rx,
        1,
        agit::domain::secret_filter::MatcherHandle::default(),
    )
    .await?;
    let (ctx, crx) = mpsc::channel::<Command>(16);
    tokio::spawn(session.run(crx));

    // Give the harness a moment to hand us its session id + transcript path.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let (rtx, mut rrx) = agit::rc::ticket::ticket();
    ctx.send(Command::Turn {
        message: prompt,
        by: Some("smoke".into()),
        guard_attempt: None,
        reply: rtx,
    })
    .await?;
    println!(
        "turn started: {:?}\n",
        rrx.wait(std::time::Duration::from_secs(60)).await
    );

    // Auto-approve everything, the way an operator clicking "allow" would.
    let ctx2 = ctx.clone();
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);
    let mut completed = 0usize;

    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            println!("\n[timeout]");
            break;
        }
        let Ok(Some(f)) = tokio::time::timeout(left, frx.recv()).await else {
            break;
        };
        *counts.entry(f.method().to_string()).or_default() += 1;

        match f.method() {
            method::ITEM_DELTA => { /* too chatty to print in full */ }
            method::APPROVAL_REQUEST => {
                let p: agit::protocol::ApprovalRequest = f.params_as().unwrap();
                println!("  APPROVAL  {:?}  {}", p.kind, p.summary);
                let (a, mut b) = agit::rc::ticket::ticket();
                ctx2.send(Command::Approve {
                    // The smoke script plays owner: it runs as this machine's owner.
                    caller_is_owner: true,
                    // This smoke path always answers one call, never a
                    // session-wide permission-mode suggestion.
                    danger: agit::rc::supervisor::DangerAuthorization::NotRequired,
                    response: agit::protocol::ApprovalResponse {
                        approval_id: p.approval_id,
                        session_id: p.session_id.clone(),
                        decision: agit::protocol::ApprovalDecision::Allow,
                        scope: agit::protocol::ApprovalScope::Once,
                        message: None,
                        by: Some("smoke".into()),
                    },
                    reply: a,
                })
                .await?;
                let _ = b.wait(std::time::Duration::from_secs(60)).await;
                println!("            → allowed");
            }
            method::ITEM_COMPLETED => {
                let p: serde_json::Value = f.params.clone().unwrap_or_default();
                completed += 1;
                let kind = p["event"]["kind"].as_str().unwrap_or("?");
                let text = p["event"]["text"].as_str().unwrap_or("");
                let hash = p["object_hash"].as_str().unwrap_or("");
                println!(
                    "  ITEM      {:16} line {:<4} hash {:12} {}",
                    kind,
                    p["line"].as_u64().unwrap_or(0),
                    &hash[..hash.len().min(12)],
                    text.chars().take(70).collect::<String>().replace('\n', " ")
                );
            }
            method::TURN_STARTED => {
                let p: serde_json::Value = f.params.clone().unwrap_or_default();
                println!(
                    "  TURN      started ({})",
                    p["source"].as_str().unwrap_or("?")
                );
            }
            method::TURN_COMPLETED => {
                let p: serde_json::Value = f.params.clone().unwrap_or_default();
                println!(
                    "  TURN      completed outcome={} cost=${:.4}",
                    p["outcome"].as_str().unwrap_or("?"),
                    p["cost_usd"].as_f64().unwrap_or(0.0)
                );
                // Let the tailer flush the last lines before we stop.
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                break;
            }
            method::SESSION_STATUS => {
                let p: serde_json::Value = f.params.clone().unwrap_or_default();
                println!("  STATUS    {}", p["status"].as_str().unwrap_or("?"));
            }
            _ => {}
        }
    }

    // Drain anything still queued.
    while let Ok(f) = frx.try_recv() {
        *counts.entry(f.method().to_string()).or_default() += 1;
        if f.method() == method::ITEM_COMPLETED {
            completed += 1;
        }
    }

    let _ = ctx.send(Command::Shutdown).await;
    println!("\n--- frame counts ---");
    for (k, v) in &counts {
        println!("  {k:24} {v}");
    }
    println!("\nitem.completed total: {completed}");
    assert!(
        completed > 0,
        "no transcript-sourced items — the tailer never saw the file"
    );
    println!("OK");
    Ok(())
}
