//! Hub 的非 HTTP 内核：身份、授权、持久化、审计。
//!
//! 为什么在 lib 里而不是 `src/bin/` 旁边：`src/bin/*.rs` 会被 cargo 当成**另一个二进制**
//! 自动发现（autobins），放模块进去会编不过。放这里还顺带让 `cargo test` 直接跑到它们的单测。
//!
//! 分层是刻意的 —— HTTP 那一层（`src/bin/agit-hub.rs`）只做解析与搬运，所有"谁能做什么"
//! 的判断都在这里，且大多是纯函数：
//!
//! ```text
//!   凭据 ──auth::authenticate──> Caller ─┐
//!                                        ├─ acl::decide(caller, agent, action) ─> Allow / Deny(原因)
//!   agents.json ──store──> AgentMeta ────┘
//! ```
//!
//! `acl::decide` 是**唯一**的判定点：JSON API、git smart-http、CLI 全都过它。

pub mod acl;
pub mod audit;
pub mod auth;
pub mod identity;
pub mod kdf;
pub mod net;
pub mod session;
pub mod store;
