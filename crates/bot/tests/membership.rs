mod bonus;
mod chat;
mod files;
mod index;
mod multi_server;
#[cfg(feature = "media")]
mod soak;
mod sync;
#[cfg(feature = "media")]
mod voice;

use std::{env, path::Path, process::Command, time::Duration};

use anyhow::{Context, Result, ensure};
use reqwest::{Method, Response, StatusCode};
use rustls_pki_types::{CertificateDer, pem::PemObject};
use rusttalk_core::{
    auth::{InviteLink, KnownServer, ProtocolError, Session, decode, encode},
    client::{KeyClient, Server, invite_name},
    identity::{DeviceIdentity, revoke_device},
    keys::{self, EnvelopeContext, Event, KeyState, Metadata, Scope},
    pinned_tls::fingerprint,
};
use serde_json::{Value, json};

fn compose_bytes(project: &str, args: &[&str]) -> Result<Vec<u8>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("docker")
        .current_dir(root)
        .args([
            "compose",
            "-f",
            "deploy/compose.dev.yml",
            "--profile",
            "index",
            "-p",
            project,
        ])
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "compose {} failed: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

fn compose(project: &str, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(compose_bytes(project, args)?)?
        .trim()
        .to_owned())
}

async fn rejected(reply: Response, status: StatusCode, code: &str) -> Result<()> {
    assert_eq!(reply.status(), status);
    let body: Value = reply.json().await?;
    assert!(body["error"] == code, "expected rejection {code}");
    Ok(())
}

async fn session(reply: Response, secrets: &mut Vec<String>) -> Result<Session> {
    assert_eq!(reply.status(), StatusCode::OK);
    let session: Session = reply.json().await?;
    secrets.push(session.token.clone());
    Ok(session)
}

async fn invite(
    owner: &mut KeyClient<'_>,
    secrets: &mut Vec<String>,
) -> Result<(String, InviteLink)> {
    let (id, link) = owner.invite(1, 300).await?;
    secrets.push(encode(&link.code));
    secrets.push(encode(
        link.link_key.as_ref().context("missing link key")?.as_ref(),
    ));
    Ok((id, InviteLink::parse(&link.to_string())?))
}

async fn check_me(bot: &Server, session: &Session, status: StatusCode) -> Result<()> {
    let reply = bot
        .request(Method::GET, "/v1/me")
        .bearer_auth(&session.token)
        .send()
        .await?;
    assert_eq!(reply.status(), status);
    Ok(())
}

const SERVER_NAME: &[u8] = b"W06 Lapis Private Server";
const CATEGORY_NAME: &[u8] = b"W06 Fern Private Category";
const CHANNEL_NAME: &[u8] = b"W06 Topaz Private Channel";
const TOPIC: &[u8] = b"W06 Saffron Private Topic";
const DISPLAY_NAME: &[u8] = b"W06 Garnet Private Display Name";
const ROLE_NAME: &[u8] = b"W06 Agate Private Role";
const DEVICE_NAME: &str = "W06 Aster Private Device";
const HISTORY_ID: &[u8] = b"w06 local history probe";

/// The peer of a DM someone else made pins it, which starts epoch 2 for just the two
/// of them; both then hold it, so either can post. Done up front, because the
/// peer's client would otherwise re-key at its next key work, mid-scenario.
async fn pin_dm_as_peer(
    peer: &mut KeyClient<'_>,
    maker: &mut KeyClient<'_>,
    dm: Scope,
) -> Result<()> {
    let maker_person = *maker.identity.certificate.person_id();
    ensure!(
        peer.bind_dm(dm.id, maker_person),
        "the peer couldn't pin the DM"
    );
    peer.sync().await?;
    receive_key(peer, dm, 2).await?;
    receive_key(maker, dm, 2).await?;
    ensure!(
        peer.dm_private(&dm.id, &maker_person),
        "the peer's re-keyed DM was not private"
    );
    Ok(())
}

async fn receive_key(client: &mut KeyClient<'_>, scope: Scope, epoch: i64) -> Result<()> {
    // An envelope can already be stored before the bot starts waiting for events.
    // Several peers may also take successive randomized pauses before wrapping.
    client.sync().await?;
    tokio::time::timeout(Duration::from_secs(45), async {
        while client.key(scope, epoch).is_none() {
            ensure!(
                client.next_event().await?.is_some(),
                "socket closed waiting for key"
            );
        }
        Ok(())
    })
    .await
    .context("waiting for envelope delivery")?
}

async fn wait_for_device(client: &mut KeyClient<'_>, device: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next_event().await? {
                Some(Event::KeysNeeded(need)) if need.device == device => return Ok(()),
                Some(Event::MemberChanged {
                    device: Some(changed),
                    ..
                }) if changed == device => return Ok(()),
                Some(_) => (),
                None => anyhow::bail!("socket closed waiting for a wrap request"),
            }
        }
    })
    .await
    .context("waiting for the keys_needed event")?
}

async fn expect_close(client: &mut KeyClient<'_>, reason: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.next_event().await?.is_some() {}
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("waiting for event socket closure")??;
    assert_eq!(client.close_reason.as_deref(), Some(reason));
    Ok(())
}

async fn docker_keys(
    project: &str,
    bot: &Server,
    a: &mut KeyClient<'_>,
    secrets: &mut Vec<String>,
) -> Result<()> {
    let space = a.space();
    a.set_server_name(SERVER_NAME).await?;
    a.set_display_name(DISPLAY_NAME).await?;
    let role_name = a.encrypt_metadata(Metadata::RoleName, b"member", ROLE_NAME)?;
    a.request(Method::PUT, "/v1/roles/member/name")
        .json(&json!({"name_ct": role_name}))
        .send()
        .await?
        .error_for_status()?;
    let category = a.create_category(CATEGORY_NAME).await?;
    let channel = a
        .create_channel(&category, "text", CHANNEL_NAME, TOPIC)
        .await?;
    let history = Metadata::ChannelTopic.seal(
        &space.id,
        HISTORY_ID,
        1,
        a.key(channel, 1).context("no channel key")?,
        b"old history",
    )?;
    println!("W06 owner uploaded encrypted server, category, channel, topic and display names");

    let (invite_id, link) = invite(a, secrets).await?;
    assert_eq!(invite_name(bot, &link).await?.as_deref(), Some(SERVER_NAME));
    println!("W06 B decrypted the server name from its invite before sign-in");
    let listed: Value = a
        .request(Method::GET, "/v1/invites")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let row = listed
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == invite_id.as_str()))
        .context("new invite not listed")?;
    assert_eq!(
        row["created_by"],
        encode(a.identity.certificate.person_id()).as_str()
    );
    assert_eq!(row["remaining_uses"], 1);
    assert!(row.get("code").is_none());
    println!("#68 the owner listed its invite with creator and uses, without the code");

    // Edit a named custom role: new flags and a new encrypted name in one change.
    let role_id = encode(&[68; 16]);
    let first = a.encrypt_metadata(Metadata::RoleName, role_id.as_bytes(), b"Hosts")?;
    a.request(Method::POST, "/v1/roles")
        .json(&json!({"id": role_id, "rank": 5, "permissions": ["invite"], "name_ct": first}))
        .send()
        .await?
        .error_for_status()?;
    let renamed = a.encrypt_metadata(Metadata::RoleName, role_id.as_bytes(), b"Event hosts")?;
    a.request(Method::PUT, &format!("/v1/roles/{role_id}"))
        .json(&json!({"permissions": ["kick", "manage_channels"], "name_ct": renamed}))
        .send()
        .await?
        .error_for_status()?;
    let roles: Value = a
        .request(Method::GET, "/v1/roles")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let role = roles["roles"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == role_id.as_str()))
        .context("edited role missing")?;
    assert_eq!(role["permissions"], json!(["kick", "manage_channels"]));
    let name = role["name_ct"].as_str().context("role name missing")?;
    assert_eq!(
        a.decrypt_metadata(Metadata::RoleName, role_id.as_bytes(), name)?,
        b"Event hosts"
    );
    a.request(Method::DELETE, &format!("/v1/roles/{role_id}"))
        .send()
        .await?
        .error_for_status()?;
    println!("#68 the owner edited a custom role's flags and encrypted name");

    let (_, b_identity) = DeviceIdentity::create(DEVICE_NAME)?;
    let b_token = session(bot.sign_in(&b_identity, Some(&link.code)).await?, secrets).await?;
    let mut b = bonus::presence(a, bot, &b_identity, &b_token).await?;
    receive_key(&mut b, channel, 1).await?;
    receive_key(&mut b, space, 1).await?;
    bonus::icons(a, &b).await?;
    bonus::profile(a, &b).await?;
    bonus::emoji(a, &b).await?;
    assert_eq!(
        Metadata::ChannelTopic.open(&space.id, HISTORY_ID, b.key(channel, 1).unwrap(), &history)?,
        b"old history"
    );
    let data: Value = b
        .request(Method::GET, "/v1/catalog")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let entry = &data["channels"][0];
    assert!(entry["id"] == encode(&channel.id));
    for (kind, id, ct, expected) in [
        (
            Metadata::ChannelName,
            channel.id,
            &entry["name_ct"],
            CHANNEL_NAME,
        ),
        (
            Metadata::ChannelTopic,
            channel.id,
            &entry["topic_ct"],
            TOPIC,
        ),
        (
            Metadata::CategoryName,
            category,
            &data["categories"][0]["name_ct"],
            CATEGORY_NAME,
        ),
    ] {
        assert_eq!(
            b.decrypt_metadata(kind, &id, ct.as_str().context("missing encrypted name")?)?,
            expected
        );
    }
    let members: Vec<Value> = b
        .request(Method::GET, "/v1/members")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let owner = a.identity.certificate.person_id();
    let profile = members
        .iter()
        .find(|row| row["person_id"] == encode(owner))
        .context("missing owner profile")?;
    assert_eq!(
        b.decrypt_metadata(
            Metadata::DisplayName,
            owner,
            profile["display_name_ct"]
                .as_str()
                .context("missing display name")?
        )?,
        DISPLAY_NAME,
    );
    rejected(
        b.request(Method::GET, "/v1/channels").send().await?,
        StatusCode::FORBIDDEN,
        "permission_denied",
    )
    .await?;
    println!("W06 B decrypted channel metadata, the owner's display name and epoch 1 history");

    let (_, link) = invite(a, secrets).await?;
    let (c_words, c_identity) = DeviceIdentity::create("W06 C")?;
    let c_token = session(bot.sign_in(&c_identity, Some(&link.code)).await?, secrets).await?;
    let c_device = encode(c_identity.certificate.signing_key().as_bytes());
    let context = EnvelopeContext::new(space.id, channel, 1, b.key(channel, 1).unwrap());
    let bad = encode(&context.wrap(&keys::random_key()?, &c_identity.certificate, &b_identity)?);
    let upload = || {
        b.request(Method::POST, "/v1/keys")
            .json(&json!({"scope": channel, "epoch": 1, "device": c_device, "envelope": bad}))
    };
    upload().send().await?.error_for_status()?;
    let poisoned: KeyState = bot
        .request(Method::GET, "/v1/keys")
        .bearer_auth(&c_token.token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(poisoned.envelopes.iter().any(|row| row.envelope == bad));
    let mut c = KeyClient::connect(bot, &c_identity, &c_token).await?;
    assert!(c.key(channel, 1).is_none());
    upload().send().await?.error_for_status()?;
    wait_for_device(a, &c_device).await?;
    receive_key(&mut c, channel, 1).await?;
    assert!(c.key(channel, 1) == a.key(channel, 1));
    println!("W06 C rejected B's bad first wrap and received the real key");
    receive_key(&mut c, space, 1).await?;
    let (dm, history_message) = chat::scenario(a, &mut b, &mut c, channel).await?;
    files::scenario(project, bot, a, &mut b, &c, &category).await?;
    let channel_path = format!("/v1/channels/{}/permissions", encode(&channel.id));
    let b_person = encode(b_identity.certificate.person_id());
    a.request(Method::PUT, &format!("/v1/members/{b_person}/roles"))
        .json(&json!({"role_ids": ["admin", "member"]}))
        .send()
        .await?
        .error_for_status()?;
    for role in ["admin", "owner"] {
        let reply = b
            .request(Method::PUT, &format!("{channel_path}/{role}"))
            .json(&json!({"flags": 0}))
            .send()
            .await?;
        rejected(reply, StatusCode::FORBIDDEN, "role_hierarchy").await?;
    }
    let reply = a
        .request(Method::PUT, &format!("{channel_path}/member"))
        .json(&json!({"flags": 2}))
        .send()
        .await?;
    rejected(reply, StatusCode::BAD_REQUEST, "invalid_channel_flags").await?;

    a.request(Method::DELETE, &format!("/v1/members/{b_person}"))
        .send()
        .await?
        .error_for_status()?;
    let reply = a.request(Method::PUT, "/v1/space").json(&json!({
        "name_ct": encode(&Metadata::ServerName.seal(&space.id, &space.id, 1, a.key(space, 1).unwrap(), SERVER_NAME)?),
    })).send().await?;
    rejected(reply, StatusCode::CONFLICT, "space_key_unavailable").await?;

    tokio::time::timeout(Duration::from_secs(5), async {
        while a.key(channel, 2).is_none() {
            match a.next_event().await? {
                Some(Event::RotateNeeded { scope }) if scope == channel => return Ok(()),
                Some(_) => (),
                None => anyhow::bail!("owner socket closed"),
            }
        }
        Ok(())
    })
    .await
    .context("waiting for the rotate_needed event")??;
    receive_key(&mut c, channel, 2).await?;
    receive_key(&mut c, space, 2).await?;
    expect_close(&mut b, "kicked").await?;
    rejected(
        b.request(Method::GET, "/v1/keys").send().await?,
        StatusCode::UNAUTHORIZED,
        "invalid_session",
    )
    .await?;
    rejected(
        bot.sign_in(&b_identity, None).await?,
        StatusCode::FORBIDDEN,
        "invite_required",
    )
    .await?;

    chat::after_kick(a, &mut c, channel, dm, history_message).await?;
    let key = c.key(channel, 2).context("C missing epoch 2")?;
    let context = EnvelopeContext::new(space.id, channel, 2, key);
    let for_b = context.wrap(key, &b_identity.certificate, &c_identity)?;
    let reply = c.request(Method::POST, "/v1/keys")
        .json(&json!({"scope": channel, "epoch": 2, "device": encode(b_identity.certificate.signing_key().as_bytes()), "envelope": encode(&for_b)}))
        .send().await?;
    rejected(reply, StatusCode::FORBIDDEN, "recipient_read_denied").await?;
    let state: KeyState = c
        .request(Method::GET, "/v1/keys")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        state
            .envelopes
            .iter()
            .all(|e| e.need.device == encode(c_identity.certificate.signing_key().as_bytes()))
    );
    let after = Metadata::ChannelTopic.seal(&space.id, HISTORY_ID, 2, key, b"after kick")?;
    assert_eq!(
        Metadata::ChannelTopic.open(&space.id, HISTORY_ID, key, &after)?,
        b"after kick"
    );
    assert!(
        Metadata::ChannelTopic
            .open(&space.id, HISTORY_ID, b.key(channel, 1).unwrap(), &after)
            .is_err()
    );
    assert!(b.key(channel, 2).is_none() && b.key(space, 2).is_none());
    let stale = context.wrap(key, &c_identity.certificate, &c_identity)?;
    let reply = c
        .request(Method::POST, "/v1/keys/epochs")
        .json(&json!({"scope": channel, "epoch": 2, "commitment": encode(&context.commitment), "envelope": encode(&stale)}))
        .send()
        .await?;
    rejected(reply, StatusCode::CONFLICT, "epoch_changed").await?;
    println!("W06 kick closed B's socket, blocked B's epoch 2 envelopes, and C decrypted epoch 2");

    let sibling = DeviceIdentity::add_device(&c_words, "W06 C second device")?;
    let sibling_token = session(bot.sign_in(&sibling, None).await?, secrets).await?;
    let mut sibling_keys = KeyClient::connect(bot, &sibling, &sibling_token).await?;
    wait_for_device(a, &encode(sibling.certificate.signing_key().as_bytes())).await?;
    for scope in [space, channel] {
        for epoch in [1, 2] {
            receive_key(&mut sibling_keys, scope, epoch).await?;
            assert!(sibling_keys.key(scope, epoch) == c.key(scope, epoch));
        }
    }
    let revoked = revoke_device(&c_words, sibling.certificate.signing_key().to_bytes())?;
    c.request(Method::POST, "/v1/me/revocations")
        .json(&json!({"record": encode(&revoked.to_bytes())}))
        .send()
        .await?
        .error_for_status()?;
    expect_close(&mut sibling_keys, "revoked").await?;
    receive_key(a, space, 3).await?;
    receive_key(a, channel, 3).await?;
    receive_key(&mut c, channel, 3).await?;
    assert!(sibling_keys.key(channel, 3).is_none());
    println!(
        "W06 a new device received both historical epochs, then revocation closed its socket and rotated keys"
    );

    a.request(
        Method::PUT,
        &format!("/v1/channels/{}/permissions/member", encode(&channel.id)),
    )
    .json(&json!({"flags": 0}))
    .send()
    .await?
    .error_for_status()?;
    receive_key(a, channel, 4).await?;
    let state: KeyState = c
        .request(Method::GET, "/v1/keys")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        state
            .scopes
            .iter()
            .all(|s| s.scope == space && s.epoch == 3 && !s.rotate)
    );
    assert!(!state.scopes.is_empty() && state.envelopes.iter().all(|e| e.need.scope == space));
    println!("W06 losing channel read permission hid its envelopes and rotated only the channel");

    let (_, link) = invite(a, secrets).await?;
    let (_, d_identity) = DeviceIdentity::create("W14 D")?;
    let d_token = session(bot.sign_in(&d_identity, Some(&link.code)).await?, secrets).await?;
    let mut d = KeyClient::connect(bot, &d_identity, &d_token).await?;
    wait_for_device(a, &encode(d_identity.certificate.signing_key().as_bytes())).await?;
    receive_key(&mut d, space, 3).await?;
    rejected(
        a.request(Method::DELETE, "/v1/me/membership")
            .send()
            .await?,
        StatusCode::CONFLICT,
        "owner_cannot_leave",
    )
    .await?;
    let reply = d
        .request(Method::DELETE, "/v1/me/membership")
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    expect_close(&mut d, "kicked").await?;
    receive_key(a, space, 4).await?;
    receive_key(&mut c, space, 4).await?;
    assert!(d.key(space, 4).is_none());
    rejected(
        bot.sign_in(&d_identity, None).await?,
        StatusCode::FORBIDDEN,
        "invite_required",
    )
    .await?;
    let left: Vec<Value> = a
        .request(Method::GET, "/v1/audit?action=left")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let d_person = encode(d_identity.certificate.person_id());
    assert!(
        left.iter()
            .any(|entry| entry["actor_person_id"] == d_person)
    );
    println!(
        "W14 D left by itself: its socket closed, the space key rotated, and it needs a new invite"
    );

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
    let logs = compose_bytes(project, &["logs", "--no-color"])?;
    for bytes in [raw, logs] {
        let folded = bytes.to_ascii_lowercase();
        for secret in [
            SERVER_NAME,
            CATEGORY_NAME,
            CHANNEL_NAME,
            TOPIC,
            DISPLAY_NAME,
            ROLE_NAME,
            DEVICE_NAME.as_bytes(),
        ] {
            let encoded = encode(secret);
            for needle in [secret, encoded.as_bytes()] {
                let needle = needle.to_ascii_lowercase();
                assert!(
                    !folded.windows(needle.len()).any(|window| window == needle),
                    "a test name or topic leaked"
                );
            }
        }
        for secret in chat::TEXTS {
            let encoded = encode(secret.as_bytes());
            for needle in [secret.as_bytes(), encoded.as_bytes()] {
                let needle = needle.to_ascii_lowercase();
                assert!(
                    !folded.windows(needle.len()).any(|window| window == needle),
                    "test message plaintext leaked"
                );
            }
        }
    }
    println!("W06 SQLite database, WAL and stack logs contained none of the test names or topics");
    println!("W07 SQLite database, WAL and stack logs contained none of the test message text");

    Ok(())
}

#[tokio::test]
async fn docker_membership() -> Result<()> {
    let Ok(project) = env::var("RUSTTALK_E2E_PROJECT") else {
        return Ok(());
    };
    let data = env::var("RUSTTALK_DEV_DATA")?;
    let data = Path::new(&data);
    let cert = CertificateDer::from_pem_file(data.join("tls/cert.pem"))?;
    let port = env::var("RUSTTALK_DEV_HTTPS_PORT").unwrap_or_else(|_| "8443".into());
    let address = format!("https://127.0.0.1:{port}/").parse()?;
    let cli = |command| compose(&project, &["exec", "-T", "api", "rusttalk-api", command]);
    let known = KnownServer {
        address,
        server_id: decode(&cli("server-id")?)?,
        tls_fingerprint: Some(fingerprint(cert.as_ref())),
    };
    let owner_pins = data.join("bot-a");
    let member_pins = data.join("bot-b");
    let owner_bot = Server::connect(
        known.address.clone(),
        known.tls_fingerprint,
        Some(&known),
        &owner_pins,
    )
    .await?;
    let (_, owner_id) = DeviceIdentity::create("Bot A")?;
    let old_code = cli("owner-invite")?;
    let owner_code = cli("owner-invite")?;
    let mut secrets = vec![old_code.clone(), owner_code.clone()];
    rejected(
        owner_bot
            .sign_in(&owner_id, Some(&decode(&old_code)?))
            .await?,
        StatusCode::FORBIDDEN,
        "invalid_invite",
    )
    .await?;
    let owner = session(
        owner_bot
            .sign_in(&owner_id, Some(&decode(&owner_code)?))
            .await?,
        &mut secrets,
    )
    .await?;
    assert!(cli("owner-invite")?.is_empty());
    println!("owner claimed via CLI invite; the previous invite and further issuance were refused");

    index::exercise(&project, &owner_bot, &owner).await?;

    let mut owner_keys = KeyClient::connect(&owner_bot, &owner_id, &owner).await?;
    docker_keys(&project, &owner_bot, &mut owner_keys, &mut secrets).await?;
    if let Ok(second_project) = env::var("RUSTTALK_E2E_SECOND_PROJECT") {
        multi_server::exercise(
            &project,
            &second_project,
            &owner_bot,
            &mut owner_keys,
            &owner_pins,
            &mut secrets,
        )
        .await
        .context("W15 second server")?;
    }
    #[cfg(feature = "media")]
    voice::exercise(&project, &owner_bot, &mut owner_keys, &mut secrets).await?;

    let (_, link) = invite(&mut owner_keys, &mut secrets).await?;
    let first_invite = link.code;
    let member_bot = Server::connect(
        link.server.address.clone(),
        link.server.tls_fingerprint,
        Some(&link.server),
        &member_pins,
    )
    .await?;
    let (member_words, member_id) = DeviceIdentity::create("Bot B")?;
    let initial = session(
        member_bot.sign_in(&member_id, Some(&link.code)).await?,
        &mut secrets,
    )
    .await?;
    check_me(&member_bot, &initial, StatusCode::OK).await?;
    rejected(
        member_bot
            .request(Method::PUT, index::SETTINGS)
            .bearer_auth(&initial.token)
            .json(&json!({"enabled": false}))
            .send()
            .await?,
        StatusCode::FORBIDDEN,
        "owner_required",
    )
    .await?;
    let initial = {
        let reply = member_bot
            .request(Method::POST, "/v1/auth/refresh")
            .header("x-forwarded-for", "192.0.2.1, 192.0.2.2")
            .bearer_auth(&initial.token)
            .send()
            .await?;
        let refreshed = session(reply, &mut secrets).await?;
        check_me(&member_bot, &initial, StatusCode::UNAUTHORIZED).await?;
        refreshed
    };
    println!(
        "owner issued a member invite; bot B joined and rotated its token through Caddy with a spoofed XFF header"
    );
    let mut prior_events = KeyClient::connect(&member_bot, &member_id, &initial).await?;
    let initial = session(member_bot.sign_in(&member_id, None).await?, &mut secrets).await?;
    expect_close(&mut prior_events, "session_replaced").await?;
    sync::exercise(&project, data, &known, &member_words, &mut secrets).await?;
    let owner_sync = owner_bot
        .request(Method::GET, rusttalk_core::sync::PATH)
        .bearer_auth(&owner.token)
        .send()
        .await?;
    assert_eq!(owner_sync.status(), StatusCode::NO_CONTENT);

    let member_person = encode(member_id.certificate.person_id());
    let member_path = format!("/v1/members/{member_person}");
    let ban_path = format!("/v1/bans/{member_person}");
    let reply = owner_bot
        .request(Method::DELETE, &member_path)
        .bearer_auth(&owner.token)
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    check_me(&member_bot, &initial, StatusCode::UNAUTHORIZED).await?;
    rejected(
        member_bot.sign_in(&member_id, None).await?,
        StatusCode::FORBIDDEN,
        "invite_required",
    )
    .await?;
    rejected(
        member_bot.sign_in(&member_id, Some(&first_invite)).await?,
        StatusCode::FORBIDDEN,
        "invalid_invite",
    )
    .await?;
    println!("kick dropped bot B's token and required an unused invite");

    let (_, second_invite) = invite(&mut owner_keys, &mut secrets).await?;
    let rejoined = session(
        member_bot
            .sign_in(&member_id, Some(&second_invite.code))
            .await?,
        &mut secrets,
    )
    .await?;
    println!("bot B rejoined with a fresh invite");

    let reply = owner_bot
        .request(Method::PUT, &ban_path)
        .bearer_auth(&owner.token)
        .json(&json!({"ban_ip": true}))
        .send()
        .await?;
    rejected(reply, StatusCode::FORBIDDEN, "role_hierarchy").await?;
    let reply = owner_bot
        .request(Method::PUT, &ban_path)
        .bearer_auth(&owner.token)
        .json(&json!({"ban_ip": false}))
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    check_me(&member_bot, &rejoined, StatusCode::UNAUTHORIZED).await?;
    let (_, third_invite) = invite(&mut owner_keys, &mut secrets).await?;
    rejected(
        member_bot
            .sign_in(&member_id, Some(&third_invite.code))
            .await?,
        StatusCode::FORBIDDEN,
        "person_banned",
    )
    .await?;
    println!("ban dropped bot B's token and blocked redemption of a valid invite");

    let reply = owner_bot
        .request(Method::DELETE, &ban_path)
        .bearer_auth(&owner.token)
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    let unbanned = session(
        member_bot
            .sign_in(&member_id, Some(&third_invite.code))
            .await?,
        &mut secrets,
    )
    .await?;
    check_me(&member_bot, &unbanned, StatusCode::OK).await?;
    println!("unban let bot B redeem the same invite and sign in again");

    let (invite_id, withdrawn) = invite(&mut owner_keys, &mut secrets).await?;
    let reply = owner_bot
        .request(Method::DELETE, &format!("/v1/invites/{invite_id}"))
        .bearer_auth(&owner.token)
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    let (_, outsider) = DeviceIdentity::create("Outsider")?;
    rejected(
        member_bot.sign_in(&outsider, Some(&withdrawn.code)).await?,
        StatusCode::FORBIDDEN,
        "invalid_invite",
    )
    .await?;
    let sibling = DeviceIdentity::add_device(&member_words, "Sibling")?;
    let sibling_token = session(member_bot.sign_in(&sibling, None).await?, &mut secrets).await?;
    let revocation = revoke_device(
        &member_words,
        member_id.certificate.signing_key().to_bytes(),
    )?;
    let reply = member_bot
        .request(Method::POST, "/v1/me/revocations")
        .bearer_auth(&sibling_token.token)
        .json(&json!({"record": encode(&revocation.to_bytes())}))
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    check_me(&member_bot, &unbanned, StatusCode::UNAUTHORIZED).await?;
    check_me(&member_bot, &sibling_token, StatusCode::OK).await?;
    rejected(
        member_bot.sign_in(&member_id, None).await?,
        StatusCode::FORBIDDEN,
        "device_revoked",
    )
    .await?;
    println!(
        "revoked invite was refused; a sibling device revoked bot B's original device and token"
    );

    let reply = owner_bot
        .request(Method::POST, "/v1/roles")
        .bearer_auth(&owner.token)
        .json(&json!({"rank": 25, "permissions": ["invite", "manage_roles"]}))
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::CREATED);
    let role: Value = reply.json().await?;
    let role_id = role["id"].as_str().context("role ID missing")?;
    let reply = owner_bot
        .request(Method::PUT, "/v1/roles/default")
        .bearer_auth(&owner.token)
        .json(&json!({"role_id": role_id}))
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    let listed: Value = owner_bot
        .request(Method::GET, "/v1/roles")
        .bearer_auth(&owner.token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(listed["default_role_id"] == role_id);
    let reply = owner_bot
        .request(Method::PUT, &format!("{member_path}/roles"))
        .bearer_auth(&owner.token)
        .json(&json!({"role_ids": [role_id, "member"]}))
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    let owner_person = encode(owner_id.certificate.person_id());
    let reply = member_bot
        .request(Method::PUT, &format!("/v1/members/{owner_person}/roles"))
        .bearer_auth(&sibling_token.token)
        .json(&json!({"role_ids": ["member"]}))
        .send()
        .await?;
    rejected(reply, StatusCode::FORBIDDEN, "role_hierarchy").await?;
    let transfer = json!({"person_id": member_person});
    let reply = owner_bot
        .request(Method::PUT, "/v1/owner")
        .bearer_auth(&owner.token)
        .json(&transfer)
        .send()
        .await?;
    assert_eq!(reply.status(), StatusCode::NO_CONTENT);
    let reply = owner_bot
        .request(Method::PUT, "/v1/owner")
        .bearer_auth(&owner.token)
        .json(&transfer)
        .send()
        .await?;
    rejected(reply, StatusCode::FORBIDDEN, "owner_required").await?;

    let reply = member_bot
        .request(Method::GET, "/v1/audit")
        .bearer_auth(&sibling_token.token)
        .send()
        .await?;
    let audit: Vec<Value> = reply.error_for_status()?.json().await?;
    for action in [
        "member_kicked",
        "member_banned",
        "member_unbanned",
        "device_revoked",
        "member_roles_changed",
        "ownership_transferred",
    ] {
        assert!(
            audit.iter().any(|event| event["action"] == action),
            "missing audit action {action}"
        );
    }
    println!(
        "custom role assignment and default worked, ownership transferred, and audit recorded moderation"
    );

    compose(&project, &["stop", "api"])?;
    compose(
        &project,
        &[
            "run",
            "--rm",
            "--no-deps",
            "--entrypoint",
            "rm",
            "api",
            "/data/server.key",
        ],
    )?;
    compose(&project, &["up", "--wait", "--wait-timeout", "60"])?;
    assert!(decode::<32>(&cli("server-id")?)? != known.server_id);
    let result = Server::connect(
        known.address.clone(),
        known.tls_fingerprint,
        None,
        &member_pins,
    )
    .await;
    assert!(
        matches!(result, Err(error) if error.downcast_ref::<ProtocolError>() == Some(&ProtocolError::ServerIdChanged))
    );
    println!("bot refused the replacement server key using its saved pin");

    let logs = compose(&project, &["logs", "--no-color", "api"])?;
    #[cfg(feature = "media")]
    {
        let livekit_logs = compose(&project, &["logs", "--no-color", "livekit"])?;
        assert!(
            !logs.contains("rusttalk-dev"),
            "LiveKit credential in API logs"
        );
        assert!(
            !livekit_logs.contains("rusttalk-dev"),
            "LiveKit credential in relay logs"
        );
    }
    for secret in secrets {
        assert!(
            !logs.contains(&secret),
            "invite or token appeared in API logs"
        );
    }
    for reason in ["refreshed", "kicked", "banned", "revoked"] {
        assert!(
            logs.contains(&format!("\"reason\":\"{reason}\"")),
            "missing session log reason {reason}"
        );
    }
    println!("API logs contained session reasons and none of the issued invite codes or tokens");
    Ok(())
}
