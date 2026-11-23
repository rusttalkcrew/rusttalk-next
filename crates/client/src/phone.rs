//! Phone calls into a voice room over SIP: the account and contacts kept on this
//! device, and turning what the person typed into a SIP address. The call itself is in
//! `call`. Phone audio is not end-to-end encrypted: it goes to the person's own SIP
//! provider (docs/privacy.md).

// Phones only keep the settings' types; calling is desktop only for now.
#![cfg_attr(any(target_os = "android", target_os = "ios"), allow(dead_code))]

use serde::{Deserialize, Serialize};

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod call;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod g711;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod resample;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod rtp;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod sdp;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod secret;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use secret::{forget_password, password, save_password, set_service};

/// Whether microphone export has been granted, or is still being safely withdrawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsentStatus {
    Off,
    On,
    Withdrawing,
    Failed,
}

/// The SIP account calls go out from. The password is kept apart (see `secret`).
/// This device only, never synced.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    /// The provider's server: `host` or `host:port`.
    pub server: String,
    pub user: String,
    /// The caller name the other side may see; optional.
    pub name: String,
}

impl Account {
    pub fn filled(&self) -> bool {
        !self.server.trim().is_empty() && !self.user.trim().is_empty()
    }

    /// The server's host, without a port.
    pub fn host(&self) -> &str {
        let server = self.server.trim();
        let server = server.strip_prefix("sip:").unwrap_or(server);
        match server.rsplit_once(':') {
            Some((host, port)) if port.parse::<u16>().is_ok() => host,
            _ => server,
        }
    }

    /// The server's port; 5060 when none is given.
    pub fn port(&self) -> u16 {
        let server = self.server.trim();
        server
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse().ok())
            .unwrap_or(5060)
    }
}

/// Someone to call; this device only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Contact {
    pub name: String,
    pub number: String,
}

/// The keypad, row by row.
pub const KEYS: [char; 12] = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '*', '0', '#'];

/// A typed number without the spaces, dashes, dots, slashes and brackets people add;
/// `None` when no digit is left or something else is in it.
pub fn clean_number(typed: &str) -> Option<String> {
    let mut number = String::new();
    for (at, c) in typed.trim().chars().enumerate() {
        match c {
            '0'..='9' | '*' | '#' => number.push(c),
            '+' if at == 0 => number.push(c),
            ' ' | '-' | '.' | '/' | '(' | ')' => {}
            _ => return None,
        }
    }
    number.chars().any(|c| c.is_ascii_digit()).then_some(number)
}

/// The SIP address for what the person typed or picked: a `sip:` address or
/// `user@host` as it is, a plain number on the account's server.
pub fn target(typed: &str, account: &Account) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() || typed.chars().any(char::is_whitespace) && typed.contains('@') {
        return None;
    }
    if let Some(rest) = typed.strip_prefix("sip:") {
        return rest.contains('@').then(|| typed.to_owned());
    }
    if typed.contains('@') {
        return Some(format!("sip:{typed}"));
    }
    let number = clean_number(typed)?;
    // `#` starts the headers part of a URI, so it is escaped.
    let user = number.replace('#', "%23");
    Some(format!("sip:{user}@{}", account.host()))
}

/// The name a call shows under the caller: the contact's name, else the number.
pub fn label(typed: &str, contacts: &[Contact]) -> String {
    let typed = typed.trim();
    let same = |contact: &&Contact| {
        contact.number.trim() == typed
            || clean_number(&contact.number).is_some_and(|n| Some(n) == clean_number(typed))
    };
    match contacts.iter().find(same) {
        Some(contact) if !contact.name.trim().is_empty() => contact.name.trim().to_owned(),
        _ => typed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(server: &str) -> Account {
        Account {
            server: server.into(),
            user: "alice".into(),
            name: String::new(),
        }
    }

    #[test]
    fn numbers_lose_what_people_type_around_them() {
        assert_eq!(
            clean_number(" +49 (30) 123-45/67 ").as_deref(),
            Some("+493012345 67".replace(' ', "").as_str())
        );
        assert_eq!(clean_number("*21#0").as_deref(), Some("*21#0"));
        assert_eq!(clean_number("12+3"), None);
        assert_eq!(clean_number("abc"), None);
        assert_eq!(clean_number("*#"), None);
    }

    #[test]
    fn targets_go_to_the_accounts_server() {
        let a = account("sip.example.com:5070");
        assert_eq!(a.host(), "sip.example.com");
        assert_eq!(a.port(), 5070);
        assert_eq!(account("sip.example.com").port(), 5060);
        assert_eq!(
            target("030 1234", &a).as_deref(),
            Some("sip:0301234@sip.example.com")
        );
        assert_eq!(
            target("*21#", &a).as_deref(),
            Some("sip:*21%23@sip.example.com")
        );
        assert_eq!(
            target("bob@other.net", &a).as_deref(),
            Some("sip:bob@other.net")
        );
        assert_eq!(
            target("sip:bob@other.net", &a).as_deref(),
            Some("sip:bob@other.net")
        );
        assert_eq!(target("sip:nobody", &a), None);
        assert_eq!(target("bob @x", &a), None);
        assert_eq!(target("", &a), None);
        assert_eq!(target("hello", &a), None);
    }

    #[test]
    fn calls_show_the_contacts_name() {
        let contacts = [Contact {
            name: "Anna".into(),
            number: "030 1234".into(),
        }];
        assert_eq!(label("0301234", &contacts), "Anna");
        assert_eq!(label("0309999", &contacts), "0309999");
        assert!(!account(" ").filled());
        assert!(account("sip.example.com").filled());
    }
}
