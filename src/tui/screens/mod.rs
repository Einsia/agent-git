//! One file per screen. Each screen answers only "what goes in the list, what goes in the detail,
//! how keys respond"; the shell and the layout always come from [`super::widgets`].

pub mod adopt;
pub mod configuration;
pub mod history;
pub mod initialize;
pub mod naming;
pub mod repos;
pub(crate) mod selector;
pub mod sessions;
pub mod sharing;
pub mod timeline;
pub mod transcript;
