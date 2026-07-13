//! Codex adapter —— 接口已预留，实现待补。
//!
//! 我们按 Claude Code 的真实结构把 export/validate 做实，Codex 在工程上留成这个桩：
//! trait 完全一致，拿到 Codex 的 session dump 格式后，只需在这里填方法，上层一行不用改。
//! 同时 src/session.rs 的 source_dir 也要加上 codex 的 dump 目录定位。
//!
//! 现状：方法都显式报「未实现」，而不是静默返回空。

use super::{Adapter, SessionIR};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

pub struct Codex;

const TODO: &str = "Codex adapter 尚未实现（接口已预留）。\n\
     拿到 Codex 的 session dump 格式后，在 src/adapter/codex.rs 填 export/validate、\n\
     并在 src/session.rs 的 source_dir 加上 codex 的 dump 目录即可，上层无需改动。";

impl Adapter for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }
    fn export(&self, _session: Option<&Path>, _cwd: &Path) -> Result<SessionIR> {
        bail!("{TODO}");
    }
    fn validate(&self, _session: &Path) -> Result<()> {
        bail!("{TODO}");
    }
    fn locate_default(&self, _cwd: &Path) -> Result<PathBuf> {
        bail!("{TODO}");
    }
}
