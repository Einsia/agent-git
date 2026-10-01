//! Optional local privacy processing. Business operations own their success criteria.

#[cfg(feature = "secret-vault")]
pub mod continuity;
#[cfg(feature = "secret-vault")]
pub mod crypto;
pub mod detector;
#[cfg(feature = "secret-vault")]
pub mod dictionary;
#[cfg(feature = "secret-vault")]
pub mod keys;
#[cfg(feature = "secret-vault")]
pub(crate) mod legacy;
#[cfg(feature = "secret-vault")]
pub mod management;
pub mod policy;
#[cfg(feature = "secret-vault")]
pub mod projector;
#[cfg(feature = "cli")]
pub mod service;
#[cfg(feature = "secret-vault")]
pub mod storage;
#[cfg(feature = "secret-vault")]
pub mod sync;
#[cfg(feature = "cli")]
pub mod worker;

pub mod mandatory;
mod repository;
pub use repository::*;
