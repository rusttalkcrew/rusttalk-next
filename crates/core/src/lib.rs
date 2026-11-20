//! Identity, crypto and protocol types shared by client, bot and api.

pub mod auth;
#[cfg(feature = "voice")]
pub mod camera;
#[cfg(feature = "capture")]
pub mod capture;
#[cfg(feature = "client")]
pub mod client;
pub mod identity;
pub mod keys;
pub mod messages;
#[cfg(any(feature = "client", feature = "livekit-tls"))]
pub mod pinned_tls;
#[cfg(all(target_os = "linux", any(feature = "capture", feature = "voice")))]
mod pipewire_helper;
#[cfg(feature = "voice")]
pub mod screen;
pub mod sync;
