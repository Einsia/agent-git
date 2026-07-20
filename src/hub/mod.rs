//! The Hub's non-HTTP core: identity, authorization, persistence, audit.
//!
//! Why it lives in the lib rather than next to `src/bin/`: cargo autodiscovers every `src/bin/*.rs`
//! as **another binary** (autobins), so a module dropped there would not compile. Living here also
//! gets their unit tests picked up by a plain `cargo test`.
//!
//! The layering is deliberate — the HTTP layer (`src/bin/agit-hub.rs`) only parses and shuttles;
//! every "who may do what" judgement lives here, and most of it is pure functions:
//!
//! ```text
//!   credential ──auth::authenticate──> Caller ─┐
//!                                              ├─ acl::decide(caller, agent, action) ─> Allow / Deny(reason)
//!   agents.json ──store──> AgentMeta ──────────┘
//! ```
//!
//! `acl::decide` is the **only** decision point: the JSON API, git smart-http, and the CLI all go
//! through it.

pub mod acl;
pub mod audit;
pub mod blob;
pub mod auth;
pub mod identity;
pub mod kdf;
pub mod metrics;
pub mod mr;
pub mod net;
pub mod session;
pub mod store;
