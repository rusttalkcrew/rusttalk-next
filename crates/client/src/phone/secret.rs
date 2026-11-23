//! The SIP password: in the OS keychain, beside this profile's keys. Without a
//! keychain it is kept only until the app closes.

use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use keyring::{Entry, Error};
use zeroize::Zeroizing;

use crate::audio::lock;

const ENTRY: &str = "sip-password-v1";

static SERVICE: OnceLock<String> = OnceLock::new();
/// The password while no keychain can hold it.
static MEMORY: Mutex<Option<Zeroizing<String>>> = Mutex::new(None);

/// The keychain service of this profile, set once at start.
pub fn set_service(service: &str) {
    let _ = SERVICE.set(service.to_owned());
}

fn entry() -> Result<Entry> {
    let service = SERVICE.get().map_or("RustTalk", String::as_str);
    Entry::new(service, ENTRY).context("open OS keychain entry")
}

/// The saved password. Asks the keychain, so keep it off hot paths.
pub fn password() -> Option<Zeroizing<String>> {
    if let Some(password) = lock(&MEMORY).clone() {
        return Some(password);
    }
    match entry().and_then(|entry| Ok(entry.get_secret()?)) {
        Ok(secret) => String::from_utf8(secret).ok().map(Zeroizing::new),
        Err(error) => {
            if !matches!(error.downcast_ref(), Some(Error::NoEntry)) {
                tracing::warn!(error = %format_args!("{error:#}"), "reading the SIP password failed");
            }
            None
        }
    }
}

/// Save the password. False when no keychain took it: then it lasts until the app closes.
pub fn save_password(password: &str) -> bool {
    match entry().and_then(|entry| Ok(entry.set_secret(password.as_bytes())?)) {
        Ok(()) => {
            *lock(&MEMORY) = None;
            true
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "saving the SIP password failed");
            *lock(&MEMORY) = Some(Zeroizing::new(password.to_owned()));
            false
        }
    }
}

pub fn forget_password() {
    *lock(&MEMORY) = None;
    match entry().and_then(|entry| Ok(entry.delete_credential()?)) {
        Ok(()) => {}
        Err(error) if matches!(error.downcast_ref(), Some(Error::NoEntry)) => {}
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "removing the SIP password failed");
        }
    }
}
