//! Identity, crypto and protocol types shared by client, bot and api.

// On phones, desktop code without a phone version yet is unused (docs/mobile.md).
#![cfg_attr(
    any(target_os = "android", target_os = "ios"),
    allow(dead_code, unused_imports)
)]

pub mod auth;
#[cfg(feature = "voice")]
pub mod camera;
#[cfg(feature = "capture")]
pub mod capture;
#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "voice")]
pub mod control;
pub mod identity;
pub mod index;
pub mod keys;
pub mod messages;
#[cfg(any(feature = "https-client", feature = "livekit-tls"))]
pub mod pinned_tls;
#[cfg(all(target_os = "linux", any(feature = "capture", feature = "voice")))]
mod pipewire_helper;
pub mod presence;
#[cfg(feature = "voice")]
pub mod screen;
pub mod sync;
#[cfg(feature = "voice")]
pub mod terminal;
