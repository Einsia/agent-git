//! agit 库 —— 两个二进制(`agit` 与 `agit-hub`)共享的核心。
//!
//! 关键动机:转录解析、scope、密钥扫描等**只有一份实现**。Hub 曾经把 Claude Code
//! 的 jsonl 解析抄了一份(`parse_session`),于是 adapter 侧修的规则(prompt 过滤、
//! isCompactSummary 排除)没同步到 Hub。抽成库后,两个 bin 都调 `adapter::claude_code::parse_jsonl`,
//! 规则改一处、两处生效。
//!
//! `#![allow(dead_code)]`:部分模块(如 gitx / environment 的字段)在某个 bin 里用不到,
//! 但 pub 项不产生 dead_code 警告;这里保留以覆盖少量私有辅助项。

#![allow(dead_code)]

pub mod adapter;
pub mod commands;
pub mod environment;
pub mod gitx;
pub mod init;
pub mod llm;
pub mod passthrough;
pub mod scan;
pub mod scope;
pub mod session;
pub mod workspace;
