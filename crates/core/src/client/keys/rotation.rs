//! Work out which locally verified removals apply here, independently of the server's
//! rotate flag. A sibling reads the same scopes. For peers use the pinned DM pair
//! or the channel's read permission, including the owner's access.
use super::*;
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::Path,
    sync::Arc,
};

use crate::keys::Excluded;

type Proof = (String, String);
#[derive(Default)]
pub(super) struct Rotations(HashMap<Scope, HashMap<String, Arc<Proof>>>);

// Keep the scope association after a restart even when the server then omits the
// person, their roles or the device. Store one signed record per revoked device.
#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    proof: Proof,
    scopes: HashSet<Scope>,
}

type SavedScopes = BTreeMap<String, Saved>;

fn read(path: &Path) -> Result<SavedScopes> {
    let file = match fs::File::open(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
        other => other?,
    };
    ensure!(
        file.metadata()?.len() <= 16 * 1024 * 1024,
        "oversized saved rotation list"
    );
    Ok(serde_json::from_reader(file.take(16 * 1024 * 1024))?)
}

impl Rotations {
    pub(super) fn load(path: &Path) -> Result<Self> {
        let mut result = Self::default();
        for (device, saved) in read(path)? {
            let owner = decode(&saved.proof.0)?;
            let signed = super::super::revocations::verify(&owner, &saved.proof.1)?;
            ensure!(
                device == keys::exclusion_id(&owner, signed.device_signing_key()),
                "invalid saved rotation device"
            );
            let proof = Arc::new(saved.proof);
            for scope in saved.scopes {
                result
                    .0
                    .entry(scope)
                    .or_default()
                    .insert(device.clone(), Arc::clone(&proof));
            }
        }
        Ok(result)
    }

    fn persist(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("missing rotation directory")?;
        fs::create_dir_all(dir)?;
        let lock = fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("store.lock"))?;
        lock.lock()?;
        let mut saved = read(path)?;
        for (scope, records) in &self.0 {
            for (device, proof) in records {
                saved
                    .entry(device.clone())
                    .or_insert_with(|| Saved {
                        proof: proof.as_ref().clone(),
                        scopes: Default::default(),
                    })
                    .scopes
                    .insert(*scope);
            }
        }
        ensure!(
            saved.len() <= 4096 && saved.values().map(|s| s.scopes.len()).sum::<usize>() <= 65_536,
            "rotation profile limit reached"
        );
        crate::client::save_json(dir, path, &saved)?;
        #[cfg(unix)]
        for parent in [dir, dir.parent().context("missing profile revocations")?] {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    pub(super) fn covered(&self, scope: Scope, excluded: &Excluded) -> bool {
        self.0
            .get(&scope)
            .is_none_or(|needed| needed.keys().all(|d| excluded.contains(d)))
    }

    pub(super) fn proposal<'a>(
        &'a self,
        scope: Scope,
        previous: &Excluded,
    ) -> (Excluded, Vec<&'a Proof>) {
        let mut excluded = previous.clone();
        let mut proofs = Vec::new();
        for (device, proof) in self.0.get(&scope).into_iter().flatten() {
            if excluded.insert(device.clone()) {
                proofs.push(proof.as_ref());
            }
        }
        (excluded, proofs)
    }
}

#[derive(serde::Deserialize)]
struct Member {
    person_id: String,
    role_ids: Vec<String>,
}

fn could_read(
    scope: Scope,
    channel: Option<&crate::client::Channel>,
    pin: Option<&DmPin>,
    roles: &[String],
    me: &PersonId,
    person: &PersonId,
) -> bool {
    if person == me {
        return true;
    }
    if scope.kind == Kind::Space {
        return !roles.is_empty();
    }
    if let Some(pin) = pin {
        return matches!(pin, DmPin::With {peer, ..} if peer == person);
    }
    channel.is_some_and(|ch| {
        ch.kind != "dm"
            && (roles.iter().any(|r| r == "owner")
                || ch
                    .flags
                    .iter()
                    .any(|f| f.flags & 1 != 0 && roles.contains(&f.role_id)))
    })
}

impl KeyClient<'_> {
    pub(super) async fn find_rotations(&mut self, scopes: &[ScopeState]) -> Result<bool> {
        let me = *self.identity.certificate.person_id();
        let mut people: HashSet<_> = self.devices.values().map(|d| *d.person_id()).collect();
        people.insert(me);
        for pin in self.dms.values() {
            if let DmPin::With { peer, .. } = pin {
                people.insert(*peer);
            }
        }
        let mut removed = Vec::new();
        let mut own = false;
        for person in people {
            for record in self.revocations.records(&person)? {
                own |= person == me;
                removed.push((
                    person,
                    keys::exclusion_id(&person, record.device_signing_key()),
                    Arc::new((encode(&person), encode(&record.to_bytes()))),
                ));
            }
        }
        if self.rotation_save_pending {
            self.rotations
                .persist(&self.revocations.rotation_file(&self.space().id))?;
            self.rotation_save_pending = false;
        }
        if removed.is_empty() {
            return Ok(false);
        }
        let catalog = self.catalog().await?;
        let members: Vec<Member> = self
            .request(Method::GET, "/v1/members")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let roles: HashMap<_, _> = members
            .into_iter()
            .map(|m| (m.person_id, m.role_ids))
            .collect();
        for channel in &catalog.channels {
            if channel.kind == "dm"
                && let Some(peer) = &channel.peer
            {
                self.bind_dm(channel.id, decode(peer)?);
            }
        }
        let channels: HashMap<_, _> = catalog.channels.iter().map(|c| (c.id, c)).collect();
        let mut added = false;
        for state in scopes {
            let scope = state.scope;
            let channel = channels.get(&scope.id).copied();
            for (person, device, proof) in &removed {
                let roles = roles.get(&proof.0).map_or(&[][..], Vec::as_slice);
                if could_read(scope, channel, self.dms.get(&scope.id), roles, &me, person) {
                    added |= self
                        .rotations
                        .0
                        .entry(scope)
                        .or_default()
                        .insert(device.clone(), Arc::clone(proof))
                        .is_none();
                }
            }
        }
        if added {
            self.rotation_save_pending = true;
            self.rotations
                .persist(&self.revocations.rotation_file(&self.space().id))?;
            self.rotation_save_pending = false;
        }
        Ok(own)
    }

    pub(super) fn rotation_proposal(&self, scope: Scope) -> (Excluded, Vec<&Proof>) {
        match self.scopes.iter().find(|s| s.scope == scope) {
            Some(state) => {
                // Inherit a verified envelope's policy. Without its key, only already
                // verified root signatures can corroborate the server's list.
                if let Some(held) = self.keys.get(&(scope, state.epoch)) {
                    return self.rotations.proposal(scope, &held.excluded);
                }
                let mut previous = state.excluded.clone();
                previous.retain(|id| {
                    self.rotations
                        .0
                        .get(&scope)
                        .is_some_and(|known| known.contains_key(id))
                });
                self.rotations.proposal(scope, &previous)
            }
            None => Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_keeps_exposure_and_replays_need_no_new_proof() {
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join("scopes").join("server");
        let (words, identity) = DeviceIdentity::create("sibling").unwrap();
        let owner = identity.certificate.person_id();
        let target = identity.certificate.signing_key().to_bytes();
        let signed = crate::identity::revoke_device(&words, target).unwrap();
        let device = keys::exclusion_id(owner, &target);
        let scope = Scope {
            kind: Kind::Channel,
            id: [9; 32],
        };
        let mut rotations = Rotations::default();
        rotations.0.entry(scope).or_default().insert(
            device.clone(),
            Arc::new((encode(owner), encode(&signed.to_bytes()))),
        );
        rotations.persist(&path).unwrap();
        let restarted = Rotations::load(&path).unwrap();
        let (excluded, proofs) = restarted.proposal(scope, &Excluded::new());
        assert_eq!(proofs.len(), 1);
        assert!(restarted.covered(scope, &excluded));
        assert!(restarted.proposal(scope, &excluded).1.is_empty());
        assert!(!restarted.covered(scope, &Excluded::new()));
        // A different root naming the same public key cannot exclude this certificate.
        assert!(!restarted.covered(
            scope,
            &Excluded::from([keys::exclusion_id(&[7; 32], &target)])
        ));
        std::fs::write(&path, b"invalid").unwrap();
        assert!(Rotations::load(&path).is_err());
    }

    #[test]
    fn read_permission_and_the_pinned_pair_select_scopes() {
        let channel = crate::client::Channel {
            id: [3; 32],
            kind: "voice".into(),
            category_id: None,
            position: 0,
            name_ct: None,
            topic_ct: None,
            peer: None,
            audio_kbps: None,
            flags: vec![crate::client::RoleFlags {
                role_id: "friends".into(),
                flags: 1,
            }],
        };
        let scope = Scope {
            kind: Kind::Channel,
            id: channel.id,
        };
        let me = [1; 32];
        let peer = [2; 32];
        assert!(could_read(
            scope,
            Some(&channel),
            None,
            &["friends".into()],
            &me,
            &peer
        ));
        assert!(!could_read(scope, Some(&channel), None, &[], &me, &peer));
        let pin = DmPin::With { peer, anchor: None };
        assert!(could_read(scope, None, Some(&pin), &[], &me, &peer));
        assert!(!could_read(
            scope,
            None,
            Some(&pin),
            &["owner".into()],
            &me,
            &[4; 32]
        ));
    }
}
