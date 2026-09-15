//! Authenticated daemon endpoints share no executor or relay business state.

pub mod access;
#[cfg(feature = "cloud-client")]
pub mod client;
pub mod cloud;
pub mod identity;
pub mod protocol;
#[cfg(feature = "tunnel")]
pub mod transport;

pub use identity::{Identity, PeerCertificate};

pub const VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[cfg(test)]
mod tests;
