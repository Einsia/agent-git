//! File-only pulls retain a manual transaction only when reconciliation needs a person.

use super::*;

pub(in crate::commands) fn reconcile(
    repo: &Repo,
    store: &crate::domain::store::Store,
    slug: &str,
    branch: &str,
    target: &str,
    source: &str,
) -> CmdResult {
    #[cfg(windows)]
    use crate::ui::quote_powershell_argument as quote_arg;
    #[cfg(not(windows))]
    use crate::ui::session::shell_arg as quote_arg;
    let target_ref = format!("{slug}@{branch}");
    let quoted_target = quote_arg(&target_ref);
    let guard = crate::domain::link::lock_branch(store, slug, branch)?;
    anyhow::ensure!(
        repo.git(&["rev-parse", &format!("refs/heads/{branch}")])? == target,
        "the pull target changed; run `agit pull {quoted_target}` again"
    );
    anyhow::ensure!(
        meta::is_file_line_at(repo, target) && meta::is_file_line_at(repo, source),
        "automatic reconciliation requires file branches on both sides"
    );
    if let Some(open) = mergetx::read(repo.root())? {
        let selected = quote_arg(&format!("{slug}@{}", open.target));
        anyhow::bail!(
            "a merge transaction is already open; inspect it with `agit merge --into {selected} --status`"
        );
    }
    let base = repo.git(&["merge-base", "--all", target, source]).context(
        "file branches have no available common ancestor; reconcile their histories explicitly",
    )?;
    anyhow::ensure!(
        !base.is_empty() && !base.contains('\n'),
        "file branches require a unique common ancestor for automatic pull reconciliation"
    );
    super::super::resume::require_merge_claims(repo, store, slug, branch, target, false)?;
    let checkout = super::super::worktree::checkout(repo, branch)?;
    let dirty =
        checkout.git_bytes_result(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    anyhow::ensure!(
        dirty.is_empty(),
        "file checkout has uncommitted or staged changes; commit or save them before `agit pull {quoted_target}`"
    );
    super::super::plumbing::ensure_v1_upgrade_preflight(&checkout, target)?;
    super::super::plumbing::ensure_v1_namespace_available_at(repo, source)?;
    let conflicts = file_reconciliation::conflicts(repo, target, source)?;
    let tx = Tx {
        mode: Some(mergetx::Mode::Manual),
        exploration: None,
        generation: Some(uuid::Uuid::now_v7().to_string()),
        target: branch.into(),
        source: format!("{slug}@{source}"),
        source_repo: Some(slug.into()),
        source_branch: None,
        base,
        target_head: target.into(),
        source_head: source.into(),
        picked: vec![],
        summary: Some("Merge remote file changes".into()),
    };
    mergetx::create(repo.root(), &tx)?;
    drop(guard);
    if conflicts.is_empty() {
        return continue_tx(checkout.root(), Some(&target_ref), &[]).with_context(|| format!(
            "file merge did not finish cleanly; inspect it with `agit merge --into {quoted_target} --status` or cancel its open transaction with `agit merge --into {quoted_target} --abort`"
        ));
    }
    ui::error("file changes conflict; the target branch has not moved");
    ui::info(format_args!(
        "edit the conflicting files in {} (current target contents are kept):",
        ui::tilde(checkout.root())
    ));
    let mut resolved = String::new();
    for path in conflicts {
        println!("  {}", quote_arg(&path));
        resolved.push_str(&format!(" --resolved {}", quote_arg(&path)));
    }
    let target_ref = quoted_target;
    ui::hint(&format!(
        "copy a source file for comparison: `agit file --into {target_ref} get <path> --ref {source} --output <separate-file>`"
    ));
    ui::hint(&format!(
        "after resolving: `agit merge --into {target_ref} --continue{resolved}`"
    ));
    ui::hint(&format!(
        "cancel: `agit merge --into {target_ref} --abort` (checkout edits are kept)"
    ));
    Ok(ExitCode::Precondition)
}
