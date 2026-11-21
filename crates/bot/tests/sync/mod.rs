use std::path::Path;

use anyhow::{Context, Result, ensure};
use reqwest::{Method, StatusCode};
use rusttalk_core::{
    auth::KnownServer,
    client::{Server, save_json, sync},
    identity::DeviceIdentity,
    sync::{MAX_BODY, PATH},
};
use serde_json::json;

use super::{compose_bytes, session};

pub async fn exercise(
    project: &str,
    data: &Path,
    known: &KnownServer,
    words: &str,
    secrets: &mut Vec<String>,
) -> Result<()> {
    const SETTING: &str = "W14 Ember private theme";
    const OTHER_ORIGIN: &str = "https://w14-private-server.invalid/";
    let dir_a = data.join("sync-a");
    let connect = |dir| {
        Server::connect(
            known.address.clone(),
            known.tls_fingerprint,
            Some(known),
            dir,
        )
    };
    let a = connect(&dir_a).await?;
    let device_a = DeviceIdentity::add_device(words, "W14 device A")?;
    let token_a = session(a.sign_in(&device_a, None).await?, secrets).await?;
    let peers_a = [(&a, &token_a)];
    let (remote, failures) = sync::load(&peers_a, &device_a.sync_key).await;
    assert!(remote.is_none() && failures.is_empty());
    let mut first = sync::Snapshot::default();
    first.state.servers = vec![
        known.clone(),
        KnownServer {
            address: OTHER_ORIGIN.parse()?,
            server_id: [42; 32],
            tls_fingerprint: Some([33; 32]),
        },
    ];
    first.state.settings.insert("theme".into(), json!(SETTING));
    first
        .state
        .contacts
        .insert("friend".into(), json!({"alias": "W14 private contact"}));
    save_json(&dir_a, &dir_a.join("joined.json"), &first.state.servers)?;
    ensure!(
        sync::push_all(&peers_a, &device_a.sync_key, &mut first)
            .await?
            .is_empty(),
        "device A sync PUT failed"
    );
    assert_eq!(first.version, 1);
    let old_blob = a
        .request(Method::GET, PATH)
        .bearer_auth(&token_a.token)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    let dir_b = data.join("sync-b");
    let b = connect(&dir_b).await?;
    let device_b = DeviceIdentity::add_device(words, "W14 device B")?;
    assert_ne!(
        device_a.certificate.signing_key(),
        device_b.certificate.signing_key()
    );
    let token_b = session(b.sign_in(&device_b, None).await?, secrets).await?;
    let peers_b = [(&b, &token_b)];
    let mut second = sync::Snapshot::default();
    second.state.servers.push(known.clone());
    let (remote, failures) = sync::load(&peers_b, &device_b.sync_key).await;
    assert!(failures.is_empty());
    assert!(
        sync::merge(
            &dir_b,
            &mut second,
            remote.context("device B did not recover a sync blob")?
        )?
        .is_empty()
    );
    assert!(second.state == first.state);
    save_json(&dir_b, &dir_b.join("joined.json"), &second.state.servers)?;
    ensure!(
        sync::push_all(&peers_b, &device_b.sync_key, &mut second)
            .await?
            .is_empty(),
        "device B sync PUT failed"
    );
    assert_eq!(second.version, 2);

    // A device restored from the words knows no servers. Signing in to one it is a member
    // of needs no invite (so the app asks no name), and that server's copy brings back the rest.
    let dir_c = data.join("sync-c");
    let c = connect(&dir_c).await?;
    let device_c = DeviceIdentity::add_device(words, "W14 restored device C")?;
    let token_c = session(c.sign_in(&device_c, None).await?, secrets).await?;
    let mut restored = sync::Snapshot::default();
    let (remote, failures) = sync::load(&[(&c, &token_c)], &device_c.sync_key).await;
    assert!(failures.is_empty());
    let remote = remote.context("restored device C found no sync blob")?;
    assert!(sync::merge(&dir_c, &mut restored, remote)?.is_empty());
    assert!(restored.state == second.state);
    println!(
        "W14 a restored device signed in without an invite and got the whole server list back"
    );

    let stale = a
        .request(Method::PUT, PATH)
        .bearer_auth(&token_a.token)
        .body(old_blob)
        .send()
        .await?;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(stale.bytes().await?.as_ref(), second.version.to_be_bytes());
    let oversized = a
        .request(Method::PUT, PATH)
        .bearer_auth(&token_a.token)
        .body(vec![0; MAX_BODY + 1])
        .send()
        .await?;
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let raw = compose_bytes(
        project,
        &[
            "exec",
            "-T",
            "api",
            "sh",
            "-ec",
            "cat /data/rusttalk.sqlite3 /data/rusttalk.sqlite3-wal",
        ],
    )?;
    for secret in [SETTING, OTHER_ORIGIN, "W14 private contact"] {
        assert!(
            !raw.windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "device sync plaintext leaked into SQLite"
        );
    }
    println!(
        "W14 two profiles recovered the same settings, pins and contacts, and SQLite held no sync plaintext"
    );
    println!("W14 the API rejected a stale version and an oversized blob");
    Ok(())
}
