//! Codex adapter —— 接口已预留，实现待补。
//!
//! 我们按 Claude Code 的真实结构把 export/import/validate 做实，Codex 在工程上留成这个桩：
//! trait 完全一致，一旦拿到 Codex 的 session 格式样本（以及 PRD 引用但尚缺的
//! codex-session-state-research.md），只需在这里填三个方法，上层一行都不用改。
//!
//! 现状：三个方法都显式报「未实现」，而不是静默返回空 —— PRD 要求「Adapter 不兼容必须显式报告」。

use super::{Adapter, SessionIR};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

pub struct Codex;

const TODO: &str = "Codex adapter 尚未实现（接口已预留）。\n\
     需要：一份 Codex session 样本 + PRD 引用的 codex-session-state-research.md。\n\
     拿到后在 src/adapter/codex.rs 填 export/import/validate 即可，上层无需改动。";

impl Adapter for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }
    fn export(&self, _session: Option<&Path>, _cwd: &Path) -> Result<SessionIR> {
        bail!("{TODO}");
    }
    fn import(&self, _state_dir: &Path, _out: &Path) -> Result<()> {
        bail!("{TODO}");
    }
    fn validate(&self, _session: &Path) -> Result<()> {
        bail!("{TODO}");
    }
    fn locate_default(&self, _cwd: &Path) -> Result<PathBuf> {
        bail!("{TODO}");
    }
}
