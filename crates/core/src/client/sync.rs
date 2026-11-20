//! End-to-end encrypted settings, joined servers and contacts for one person.

use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use futures_util::{StreamExt, stream};
use reqwest::{Method, Response, StatusCode, header::CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;
use zeroize::Zeroizing;

use crate::{
    auth::{KnownServer, ProtocolError, Session, check_pin},
    identity::random_bytes,
    sync::{MAX_BODY, MAX_CIPHERTEXT, MIN_CIPHERTEXT, NONCE_SIZE, PATH, split_blob},
};

use super::{
    Server,
    server::{load_pins, validate_origin},
};

#[derive(Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    pub settings: BTreeMap<String, Value>,
    pub servers: Vec<KnownServer>,
    pub contacts: BTreeMap<String, Value>,
}

/// Local version zero represents a profile that has never synced.
#[derive(Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: i64,
    pub state: State,
}

/// Use the current session for each of this person's joined servers.
pub type Peer<'a> = (&'a Server, &'a Session);
pub type Failures<'a> = Vec<(&'a Server, anyhow::Error)>;

pub struct PinConflict {
    pub address: Url,
    pub reason: ProtocolError,
}

#[derive(Debug, thiserror::Error)]
#[error("server already holds sync version {0}; reload before retrying")]
pub struct VersionConflict(pub i64);

/// Fetch every peer and return the newest authenticated snapshot and any per-server failures.
/// Equal versions are ordered by origin so a tie does not depend on response order.
pub async fn load<'a>(peers: &[Peer<'a>], key: &[u8; 32]) -> (Option<Snapshot>, Failures<'a>) {
    let mut newest: Option<(&Server, Snapshot)> = None;
    let mut failures = Vec::new();
    let mut replies =
        stream::iter(peers.iter().map(|&(server, session)| async move {
            (server, load_one(server, session, key).await)
        }))
        .buffer_unordered(8);
    while let Some((server, result)) = replies.next().await {
        match result {
            Ok(Some(snapshot)) => {
                if newest.as_ref().is_none_or(|(chosen, current)| {
                    (snapshot.version, &server.known_server().address)
                        > (current.version, &chosen.known_server().address)
                }) {
                    newest = Some((server, snapshot));
                }
            }
            Ok(None) => {}
            Err(error) => failures.push((server, error)),
        }
    }
    (newest.map(|(_, snapshot)| snapshot), failures)
}

/// Union joined servers, preserving local and previously saved pins on conflict.
/// For settings and contacts the newer snapshot wins per field, with local winning ties.
/// Apply pending local edits to the merged state before pushing.
pub fn merge(data: &Path, local: &mut Snapshot, remote: Snapshot) -> Result<Vec<PinConflict>> {
    let pins = load_pins(&data.join("servers.json"))?;
    let mut conflicts = Vec::new();
    for candidate in remote.state.servers {
        let joined = local
            .state
            .servers
            .iter()
            .find(|server| server.address == candidate.address);
        let saved = pins
            .iter()
            .find(|server| server.address == candidate.address);
        if let Err(reason) =
            check_pin(saved, &candidate).and_then(|()| check_pin(joined, &candidate))
        {
            conflicts.push(PinConflict {
                address: candidate.address,
                reason,
            });
        } else if joined.is_none() {
            local.state.servers.push(candidate);
        }
    }
    let prefer_remote = remote.version > local.version;
    for (local_fields, remote_fields) in [
        (&mut local.state.settings, remote.state.settings),
        (&mut local.state.contacts, remote.state.contacts),
    ] {
        for (field, value) in remote_fields {
            if prefer_remote {
                local_fields.insert(field, value);
            } else {
                local_fields.entry(field).or_insert(value);
            }
        }
    }
    local.version = local.version.max(remote.version);
    Ok(conflicts)
}

/// Seal one new version and try every peer. Update the local version if any peer accepts it.
/// A version conflict calls for another load and merge before retrying.
pub async fn push_all<'a>(
    peers: &[Peer<'a>],
    key: &[u8; 32],
    local: &mut Snapshot,
) -> Result<Failures<'a>> {
    if peers.is_empty() {
        return Ok(Vec::new());
    }
    let version = local
        .version
        .checked_add(1)
        .context("sync version exhausted")?;
    let body = seal(key, version, &local.state)?;
    let mut replies = stream::iter(peers.iter().map(|&(server, session)| {
        let body = &body;
        async move { (server, push_one(server, session, body.clone()).await) }
    }))
    .buffer_unordered(8);
    let mut failures = Vec::new();
    while let Some((server, result)) = replies.next().await {
        match result {
            Ok(()) => local.version = version,
            Err(error) => failures.push((server, error)),
        }
    }
    Ok(failures)
}

async fn load_one(server: &Server, session: &Session, key: &[u8; 32]) -> Result<Option<Snapshot>> {
    let reply = server
        .request(Method::GET, PATH)
        .bearer_auth(&session.token)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    match reply.status() {
        StatusCode::NO_CONTENT => Ok(None),
        StatusCode::OK => Ok(Some(open(key, &read_body(reply, MAX_BODY).await?)?)),
        status => anyhow::bail!("unexpected sync GET status {status}"),
    }
}

async fn push_one(server: &Server, session: &Session, body: Vec<u8>) -> Result<()> {
    let reply = server
        .request(Method::PUT, PATH)
        .bearer_auth(&session.token)
        .header(CONTENT_TYPE, "application/octet-stream")
        .body(body)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    match reply.status() {
        StatusCode::NO_CONTENT => Ok(()),
        StatusCode::CONFLICT => {
            let stored: [u8; 8] = read_body(reply, 8)
                .await?
                .try_into()
                .ok()
                .context("invalid sync conflict response")?;
            let version = i64::from_be_bytes(stored);
            ensure!(version > 0, "invalid sync conflict version");
            Err(VersionConflict(version).into())
        }
        status => anyhow::bail!("unexpected sync PUT status {status}"),
    }
}

async fn read_body(mut reply: Response, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = reply.chunk().await.map_err(reqwest::Error::without_url)? {
        ensure!(chunk.len() <= limit - body.len(), "oversized sync response");
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn associated_data(version: i64) -> [u8; 24] {
    let mut ad = *b"rusttalk-sync-v1\0\0\0\0\0\0\0\0";
    ad[16..].copy_from_slice(&version.to_be_bytes());
    ad
}

fn validate_servers(state: &State) -> Result<()> {
    let mut origins = HashSet::new();
    for server in &state.servers {
        validate_origin(&server.address)?;
        ensure!(
            origins.insert(&server.address),
            "duplicate sync server origin"
        );
    }
    Ok(())
}

fn seal(key: &[u8; 32], version: i64, state: &State) -> Result<Vec<u8>> {
    ensure!(version > 0, "invalid sync version");
    validate_servers(state)?;
    let plaintext = Zeroizing::new(serde_json::to_vec(state)?);
    ensure!(
        plaintext.len() <= MAX_CIPHERTEXT - MIN_CIPHERTEXT,
        "oversized sync blob"
    );
    let mut nonce = [0; NONCE_SIZE];
    random_bytes(&mut nonce)?;
    let ciphertext = XChaCha20Poly1305::new(&(*key).into())
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &plaintext,
                aad: &associated_data(version),
            },
        )
        .map_err(|_| anyhow::anyhow!("sync encryption failed"))?;
    Ok([version.to_be_bytes().as_slice(), &nonce, &ciphertext].concat())
}

fn open(key: &[u8; 32], body: &[u8]) -> Result<Snapshot> {
    let (version, blob) = split_blob(body).context("invalid sync blob size or version")?;
    let (nonce, ciphertext) = blob
        .split_first_chunk::<NONCE_SIZE>()
        .expect("checked blob size");
    let plaintext = Zeroizing::new(
        XChaCha20Poly1305::new(&(*key).into())
            .decrypt(
                &XNonce::from(*nonce),
                Payload {
                    msg: ciphertext,
                    aad: &associated_data(version),
                },
            )
            .map_err(|_| anyhow::anyhow!("sync authentication failed"))?,
    );
    let state =
        serde_json::from_slice(&plaintext).map_err(|_| anyhow::anyhow!("invalid sync state"))?;
    validate_servers(&state)?;
    Ok(Snapshot { version, state })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KEY: [u8; 32] = [7; 32];

    fn server(host: &str, id: u8, pin: u8) -> KnownServer {
        KnownServer {
            address: format!("https://{host}.example/").parse().unwrap(),
            server_id: [id; 32],
            tls_fingerprint: Some([pin; 32]),
        }
    }

    #[test]
    fn authenticated_snapshot() {
        let state = State {
            settings: [("theme".into(), json!("night"))].into(),
            servers: vec![server("home", 1, 2)],
            contacts: [("friend".into(), json!({"name": "Finch"}))].into(),
        };
        let body = seal(&KEY, 1, &state).unwrap();
        let decoded = open(&KEY, &body).unwrap();
        assert_eq!(decoded.version, 1);
        assert!(decoded.state == state);
        assert_ne!(body, seal(&KEY, 1, &state).unwrap());
        assert!(open(&[8; 32], &body).is_err());
        assert!(open(&KEY, &body[..body.len() - 1]).is_err());

        let mut tampered = body.clone();
        tampered[8 + NONCE_SIZE] ^= 1;
        assert!(open(&KEY, &tampered).is_err());
        let mut replayed = body;
        replayed[..8].copy_from_slice(&2i64.to_be_bytes());
        assert!(open(&KEY, &replayed).is_err());
    }

    #[test]
    fn ciphertext_limit() {
        let mut state = State::default();
        state.settings.insert("large".into(), json!(""));
        let overhead = serde_json::to_vec(&state).unwrap().len() + MIN_CIPHERTEXT;
        state
            .settings
            .insert("large".into(), json!("a".repeat(MAX_CIPHERTEXT - overhead)));
        let mut full = seal(&KEY, 1, &state).unwrap();
        assert_eq!(full.len(), MAX_BODY);
        assert!(open(&KEY, &full).is_ok());
        full.push(0);
        assert!(open(&KEY, &full).is_err());
        state.settings.insert(
            "large".into(),
            json!("a".repeat(MAX_CIPHERTEXT - overhead + 1)),
        );
        assert!(seal(&KEY, 1, &state).is_err());
    }

    #[test]
    fn merge_preserves_local_and_saved_pins() {
        let dir = tempfile::tempdir().unwrap();
        let home = server("home", 1, 2);
        let archived = server("archived", 3, 4);
        let work = server("work", 5, 6);
        super::super::save_json(
            dir.path(),
            &dir.path().join("servers.json"),
            &std::slice::from_ref(&archived),
        )
        .unwrap();
        let mut local = Snapshot {
            version: 1,
            state: State {
                servers: vec![home.clone()],
                settings: [
                    ("theme".into(), json!("day")),
                    ("local".into(), json!(true)),
                ]
                .into(),
                ..State::default()
            },
        };
        let remote = Snapshot {
            version: 2,
            state: State {
                servers: vec![server("home", 9, 2), server("archived", 3, 9), work.clone()],
                settings: [("theme".into(), json!("night"))].into(),
                ..State::default()
            },
        };
        let conflicts = merge(dir.path(), &mut local, remote).unwrap();
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].address, home.address);
        assert_eq!(conflicts[0].reason, ProtocolError::ServerIdChanged);
        assert_eq!(conflicts[1].address, archived.address);
        assert_eq!(conflicts[1].reason, ProtocolError::TlsChanged);
        assert!(local.state.servers == vec![home, work]);
        assert_eq!(local.state.settings["theme"], "night");
        assert_eq!(local.state.settings["local"], true);
        assert_eq!(local.version, 2);
    }
}
