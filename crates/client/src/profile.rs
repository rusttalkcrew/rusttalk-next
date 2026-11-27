//! This person's name and bio (#201): one default name for every server, which a
//! server can override with a name of its own, and one bio. The default, the bio and
//! which servers have their own name sync to the person's other devices. Each device
//! also keeps the servers it still has to tell about a change made there, like
//! `server::HideOffline`, so a server that was offline catches up when it loads and two
//! devices never write a server back and forth.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::audio::Settings;

/// The longest name, as the name fields allow.
pub const NAME_CHARS: usize = 100;
/// A bio is at most this many characters: two or three sentences, like Discord's
/// 190. Any 190 characters (4 bytes at most each) fit the server's 1024-byte cap.
pub const BIO_CHARS: usize = 190;
pub const BIO_LINES: usize = 8;

/// What servers still have to be told, by server address.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pending {
    pub name: BTreeMap<String, String>,
    pub bio: BTreeMap<String, String>,
}

/// `text` cut to the bio's line and character caps, as the editor keeps it.
pub fn fit_bio(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').take(BIO_LINES).collect();
    lines.join("\n").chars().take(BIO_CHARS).collect()
}

/// Why `name` can't be saved, if it can't: it holds a link or invite, which every
/// member would see (#658).
pub fn name_problem(name: &str) -> Option<String> {
    rusttalk_core::names::contains_link(name).then(|| t!("names.no_links").into())
}

/// Someone else's name as decrypted: one holding a link is dropped, so they show as
/// "Member ####", and an overlong one is cut to what the name fields allow.
pub fn received_name(name: Option<String>) -> Option<String> {
    let name = name.filter(|name| !rusttalk_core::names::contains_link(name))?;
    Some(match name.char_indices().nth(NAME_CHARS) {
        Some((end, _)) => format!("{}…", &name[..end]),
        None => name,
    })
}

/// The bio as it is saved.
pub fn clean_bio(text: &str) -> String {
    fit_bio(text.trim()).trim_end().to_owned()
}

impl Settings {
    /// Whether this person picked a name of its own on `server`.
    pub fn own_name(&self, server: &Url) -> bool {
        self.name_overrides.iter().any(|s| s == server.as_str())
    }

    fn set_own_name(&mut self, server: &Url, own: bool) {
        self.name_overrides.retain(|s| s != server.as_str());
        if own {
            self.name_overrides.push(server.to_string());
            self.name_overrides.sort();
        }
    }

    /// The person changed their default name here: every server without its own
    /// name gets it. Returns those servers.
    pub fn set_default_name<'a>(
        &mut self,
        name: &str,
        servers: impl Iterator<Item = &'a Url>,
    ) -> Vec<Url> {
        self.display_name = name.to_owned();
        let follow: Vec<Url> = servers.filter(|s| !self.own_name(s)).cloned().collect();
        for server in &follow {
            (self.profile_pending.name).insert(server.to_string(), name.to_owned());
        }
        follow
    }

    /// The person changed their bio here: every server gets it.
    pub fn set_bio<'a>(&mut self, bio: &str, servers: impl Iterator<Item = &'a Url>) {
        self.bio = bio.to_owned();
        for server in servers {
            (self.profile_pending.bio).insert(server.to_string(), bio.to_owned());
        }
    }

    /// The person saved `name` for `server` only. Their default name there counts as
    /// no name of its own, so it follows later changes.
    pub fn name_on(&mut self, server: &Url, name: &str) {
        if self.display_name.is_empty() {
            self.display_name = name.to_owned();
        }
        let own = name != self.display_name;
        self.set_own_name(server, own);
        self.profile_pending.name.remove(server.as_str());
    }

    /// "Use my default name" on `server`: returns the name to send it.
    pub fn use_default_name(&mut self, server: &Url) -> Option<String> {
        self.set_own_name(server, false);
        let name = self.display_name.clone();
        (!name.is_empty()).then(|| {
            (self.profile_pending.name).insert(server.to_string(), name.clone());
            name
        })
    }

    /// Joined `server` with `name` (the join page fills in the default). The first
    /// name becomes the default; another one is that server's own. The server starts
    /// without a bio, so it is told this one.
    pub fn joined(&mut self, server: &Url, name: &str) {
        if name.is_empty() {
            self.use_default_name(server);
        } else {
            self.name_on(server, name);
        }
        if !self.bio.is_empty() {
            (self.profile_pending.bio).insert(server.to_string(), self.bio.clone());
        }
    }

    /// A server this person is no longer in has nothing left to be told.
    pub fn left(&mut self, server: &Url) {
        self.profile_pending.name.remove(server.as_str());
        self.profile_pending.bio.remove(server.as_str());
    }

    /// After taking over synced values from another device: what this device still
    /// had to send is the newest default and bio now, so both devices send the same.
    pub fn adopt_pending(&mut self) {
        let overrides = self.name_overrides.clone();
        let pending = &mut self.profile_pending;
        pending.name.retain(|server, _| !overrides.contains(server));
        for name in pending.name.values_mut() {
            name.clone_from(&self.display_name);
        }
        for bio in pending.bio.values_mut() {
            bio.clone_from(&self.bio);
        }
    }

    /// A server reported this person's name and bio there; returns what to send it.
    /// Without anything pending for that server, nothing is sent, so a change made
    /// on another device is never written back; only a server without a name at all
    /// gets the default. `bios` is false on servers that don't store bios.
    pub fn reported(
        &mut self,
        server: &Url,
        name: Option<&str>,
        bio: Option<&str>,
        bios: bool,
    ) -> (Option<String>, Option<String>) {
        let key = server.as_str();
        let unnamed = name.is_none() && !self.display_name.is_empty() && !self.own_name(server);
        let pending = &mut self.profile_pending;
        let send_name = match pending.name.get(key) {
            Some(want) if Some(want.as_str()) == name => {
                pending.name.remove(key);
                None
            }
            Some(want) => Some(want.clone()),
            None if unnamed => Some(self.display_name.clone()),
            None => None,
        };
        let pending = &mut self.profile_pending;
        let send_bio = match pending.bio.get(key) {
            Some(want) if bios && want.as_str() != bio.unwrap_or_default() => Some(want.clone()),
            Some(_) => {
                pending.bio.remove(key);
                None
            }
            None => None,
        };
        (send_name, send_bio)
    }

    /// Once, for people from before default names: the name used on most servers
    /// becomes the default and servers with another name keep it as their own.
    /// `names` has each server's name, or `None` for a server not loaded yet, which
    /// keeps its name too. Returns whether there was anything to migrate.
    pub fn migrate(&mut self, names: &[(&Url, Option<Option<&str>>)]) -> bool {
        if !self.display_name.is_empty() {
            return false;
        }
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for name in names.iter().filter_map(|(_, name)| name.flatten()) {
            match counts.iter_mut().find(|(seen, _)| *seen == name) {
                Some((_, count)) => *count += 1,
                None => counts.push((name, 1)),
            }
        }
        // The first one listed wins a tie.
        let Some(most) = counts.iter().rev().max_by_key(|(_, count)| *count) else {
            return false;
        };
        self.display_name = most.0.to_owned();
        for (server, name) in names {
            match name {
                Some(None) => {
                    self.use_default_name(server);
                }
                Some(Some(name)) if *name == self.display_name => self.set_own_name(server, false),
                _ => self.set_own_name(server, true),
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(n: u8) -> Url {
        format!("https://server{n}.test/").parse().unwrap()
    }

    #[test]
    fn received_names_drop_links_and_stay_short() {
        assert_eq!(received_name(Some("Ann".into())).as_deref(), Some("Ann"));
        assert_eq!(received_name(Some("rusttalk://a.test/i/x#y".into())), None);
        assert_eq!(received_name(Some("go www.a.test".into())), None);
        assert_eq!(received_name(None), None);
        let long = received_name(Some("é".repeat(NAME_CHARS + 5))).unwrap();
        assert_eq!(long, format!("{}…", "é".repeat(NAME_CHARS)));
        assert!(name_problem("J. Smith").is_none());
        assert!(name_problem("https://a.test").is_some());
    }

    #[test]
    fn a_new_default_reaches_servers_without_their_own_name() {
        let (a, b, c) = (server(1), server(2), server(3));
        let mut me = Settings::default();
        me.joined(&a, "Ann");
        me.joined(&b, "Annie");
        me.joined(&c, "Ann");
        assert_eq!(me.display_name, "Ann");
        assert!(me.own_name(&b) && !me.own_name(&a));
        let told = me.set_default_name("Anna", [&a, &b, &c].into_iter());
        assert_eq!(told, [a.clone(), c.clone()]);
        // A took it; C was offline and still gets it when it loads, B keeps its name.
        assert_eq!(me.reported(&a, Some("Anna"), None, true), (None, None));
        assert_eq!(me.reported(&b, Some("Annie"), None, true), (None, None));
        let restarted: Settings =
            serde_json::from_value(serde_json::to_value(&me).unwrap()).unwrap();
        let mut me = restarted;
        assert_eq!(
            me.reported(&c, Some("Ann"), None, true).0.as_deref(),
            Some("Anna")
        );
        assert_eq!(me.reported(&c, Some("Anna"), None, true).0, None);
        assert!(me.profile_pending.name.is_empty());
    }

    #[test]
    fn clearing_an_override_sends_the_default_and_follows_it_again() {
        let (a, b) = (server(1), server(2));
        let mut me = Settings::default();
        me.joined(&a, "Ann");
        me.joined(&b, "Boss");
        assert_eq!(me.use_default_name(&b).as_deref(), Some("Ann"));
        assert!(!me.own_name(&b));
        assert_eq!(
            me.reported(&b, Some("Boss"), None, true).0.as_deref(),
            Some("Ann")
        );
        assert_eq!(me.reported(&b, Some("Ann"), None, true).0, None);
        assert_eq!(me.set_default_name("Anna", [&a, &b].into_iter()).len(), 2);
        // Saving the default as a server's own name keeps it following.
        me.name_on(&a, "Anna");
        assert!(!me.own_name(&a));
    }

    #[test]
    fn a_change_from_another_device_is_not_written_back() {
        let a = server(1);
        let mut first = Settings::default();
        first.joined(&a, "Ann");
        first.profile_pending = Pending::default();
        let mut second = first.clone();
        first.set_default_name("Anna", [&a].into_iter());
        // The second device hasn't synced yet: the server's new name is left alone.
        assert_eq!(second.reported(&a, Some("Anna"), None, true), (None, None));
        // Both had a change pending: once synced, both send the newest one.
        second.set_default_name("Annie", [&a].into_iter());
        second.display_name = "Anna".into();
        second.adopt_pending();
        assert_eq!(second.profile_pending.name[a.as_str()], "Anna");
        // A server without any name gets the default.
        assert_eq!(
            second.reported(&a, None, None, true).0.as_deref(),
            Some("Anna")
        );
    }

    #[test]
    fn bios_reach_every_server_that_stores_them() {
        let (a, b, old) = (server(1), server(2), server(3));
        let mut me = Settings::default();
        me.set_bio("Hi :wave:", [&a, &b, &old].into_iter());
        assert_eq!(
            me.reported(&a, None, None, true).1.as_deref(),
            Some("Hi :wave:")
        );
        assert_eq!(me.reported(&a, Some("x"), Some("Hi :wave:"), true).1, None);
        // An older server drops bios: nothing to wait for there.
        assert_eq!(me.reported(&old, None, None, false).1, None);
        me.set_bio("", [&a].into_iter());
        assert_eq!(
            me.reported(&a, None, Some("Hi :wave:"), true).1.as_deref(),
            Some("")
        );
        assert_eq!(me.reported(&a, None, None, true).1, None);
        me.left(&b);
        assert!(me.profile_pending.bio.is_empty());
        // A new server is told the bio.
        me.set_bio("Hello", std::iter::empty());
        me.joined(&b, "Ann");
        assert_eq!(me.profile_pending.bio[b.as_str()], "Hello");
    }

    #[test]
    fn migration_takes_the_most_used_name_and_keeps_the_others() {
        let (a, b, c, d, e) = (server(1), server(2), server(3), server(4), server(5));
        let mut me = Settings::default();
        let names = [
            (&a, Some(Some("Boss"))),
            (&b, Some(Some("Ann"))),
            (&c, Some(Some("Ann"))),
            (&d, Some(None)),
            (&e, None),
        ];
        assert!(me.migrate(&names));
        assert_eq!(me.display_name, "Ann");
        assert_eq!(me.name_overrides, [a.to_string(), e.to_string()]);
        // The server without a name gets the default; nothing else is sent.
        assert_eq!(
            me.profile_pending.name.keys().collect::<Vec<_>>(),
            [d.as_str()]
        );
        assert!(!me.migrate(&names));
        // Nothing to go by: no default yet.
        assert!(!Settings::default().migrate(&[(&a, Some(None)), (&b, None)]));
        // A tie goes to the first server.
        let mut tie = Settings::default();
        tie.migrate(&[(&a, Some(Some("X"))), (&b, Some(Some("Y")))]);
        assert_eq!(tie.display_name, "X");
    }

    #[test]
    fn bios_keep_eight_lines_and_190_characters() {
        let long = "a\n".repeat(12);
        assert_eq!(fit_bio(&long).lines().count(), BIO_LINES);
        let emoji = "😀".repeat(300);
        assert_eq!(fit_bio(&emoji).chars().count(), BIO_CHARS);
        assert!(fit_bio(&emoji).len() <= rusttalk_core::keys::SMALL_METADATA);
        // The editor keeps a trailing newline while typing; saving trims it.
        assert_eq!(fit_bio("hi\n"), "hi\n");
        assert_eq!(clean_bio("  hi :wave:\n\n"), "hi :wave:");
    }
}
