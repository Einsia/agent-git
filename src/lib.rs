//! The agit library — the core shared by the two binaries (`agit` and `agit-hub`).
//!
//! Key motivation: transcript parsing, scope, secret scanning, and the like have
//! **only one implementation**. Hub used to keep its own copy of Claude Code's jsonl
//! parsing (`parse_session`), so rules fixed on the adapter side (prompt filtering,
//! isCompactSummary exclusion) weren't synced to Hub. After extracting them into a
//! library, both bins call `adapter::claude_code::parse_jsonl`, so a rule changed in
//! one place takes effect in both.
//!
//! `#![allow(dead_code)]`: some modules (such as fields on gitx / environment) go
//! unused in one of the bins, but pub items don't produce dead_code warnings; this is
//! kept here to cover the few private helper items.


// Pedantic markdown-in-doc-comment lint; the comment style here is deliberate.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
#![allow(dead_code)]

pub mod adapter;
pub mod agent;
pub mod commands;
pub mod convo;
pub mod environment;
pub mod gitx;
pub mod harness;
pub mod hub;
pub mod init;
pub mod llm;
pub mod passthrough;
pub mod register;
pub mod scan;
pub mod scope;
pub mod session;
pub mod sync;
pub mod ui;
pub mod workspace;
