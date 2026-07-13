//! 透明 git 透传。
//!
//! PRD：「默认 scope 必须是透明 Git wrapper：参数、退出码、stdout、hook、remote 和
//! credential helper 均保持兼容。」
//!
//! 所以对非原生子命令，我们 spawn（不是 exec）对应库的 git，继承 stdio，传播退出码，
//! 跑完再做 post-hook（ref 动了就写 WorkspaceRevision）。用 spawn 不用 exec 是为了留住
//! post-hook 的机会；继承 stdio 让 credential helper、交互式 prompt、hook 全部照常。

use crate::scope::{self, Scope};
use crate::workspace;
use anyhow::Result;
use std::path::Path;
use std::process::{Command, Stdio};

pub fn run(scope: Scope, args: &[String]) -> Result<i32> {
    let root = scope::root_for(scope)?;
    let subcommand = args.first().cloned().unwrap_or_default();

    // ref 移动前的 HEAD，用来判断是否真的动了。
    let before = head_of(&root);

    let status = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    let code = status.code().unwrap_or(-1);

    // post-hook：命令成功、且是会移动 ref 的子命令、且 HEAD 确实变了 → 记一条配对。
    if code == 0 && workspace::moves_ref(&subcommand) {
        let after = head_of(&root);
        if after != before {
            // 配对失败不该让主命令失败，只提示。
            if let Err(e) = workspace::record(&workspace::trigger_label(scope, &subcommand)) {
                eprintln!("agit: 已执行，但生成 WorkspaceRevision 失败: {e:#}");
            }
        }
    }

    Ok(code)
}

fn head_of(root: &Path) -> String {
    scope::git_in_status(root, &["rev-parse", "HEAD"]).1
}
