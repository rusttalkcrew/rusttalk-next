use super::{receive_key, rejected};
use anyhow::{Context, Result, ensure};
use reqwest::{Method, StatusCode};
use rusttalk_core::{
    auth::encode,
    client::{Event, KeyClient, MessageBody, MessageError},
    keys::{self, KeyState, Scope},
    messages::MessageContext,
};
use serde_json::{Value, json};
use std::time::Duration;

const TEXT_A: &str = "W07 Quartz Text from A";
const TEXT_B: &str = "W07 Amethyst Text from B";
const DM_A: &str = "W07 Citrine Private Message from A";
const DM_B: &str = "W07 Opal Private Message from B";
const EDIT: &str = "W07 Turquoise Edited Message";
const FORGED_EDIT: &str = "W07 Garnet Impostor Edit";
const DELETE: &str = "W07 Pearl Author Deleted Message";
const MOD_DELETE: &str = "W07 Amber Moderator Deleted Message";
pub(super) const TEXTS: [&str; 8] = [
    TEXT_A,
    TEXT_B,
    DM_A,
    DM_B,
    EDIT,
    FORGED_EDIT,
    DELETE,
    MOD_DELETE,
];

fn body(text: &str) -> MessageBody {
    MessageBody {
        text: text.to_owned(),
        ..Default::default()
    }
}

fn path(scope: Scope, suffix: &str) -> String {
    format!("/v1/channels/{}/messages{suffix}", encode(&scope.id))
}

async fn wait_event(
    client: &mut KeyClient<'_>,
    scope: Scope,
    id: &[u8; 32],
    deleted: bool,
) -> Result<()> {
    let channel = encode(&scope.id);
    let id = encode(id);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = client.next_event().await?.context("chat socket closed")?;
            match event {
                Event::MessageCreated { channel: c, id: m }
                    if !deleted && c == channel && m == id =>
                {
                    return Ok(());
                }
                Event::MessageDeleted { channel: c, id: m }
                    if deleted && c == channel && m == id =>
                {
                    return Ok(());
                }
                _ => (),
            }
        }
    })
    .await
    .context("waiting for message event")?
}

pub(super) async fn exchange(
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    channel: Scope,
    texts: [&str; 2],
) -> Result<[[u8; 32]; 2]> {
    let first = a.send_message(&channel.id, &body(texts[0])).await?;
    wait_event(b, channel, &first.id, false).await?;
    let received = b.message(&channel.id, &first.id).await?;
    assert!(b.open_message(&channel.id, &received).await?.text == texts[0]);
    // Scroll-back and resync reopen the same record.
    assert!(b.open_message(&channel.id, &received).await?.text == texts[0]);

    let mut reply = body(texts[1]);
    reply.reply_to = Some(first.id);
    let second = b.send_message(&channel.id, &reply).await?;
    wait_event(a, channel, &second.id, false).await?;
    let recent = a.message_page(&channel.id, None, 1).await?;
    let [received] = recent.as_slice() else {
        anyhow::bail!("expected one latest message");
    };
    assert_eq!(received.id, second.id);
    assert!(a.open_message(&channel.id, received).await? == reply);
    let older = a.message_page(&channel.id, Some(&second.id), 1).await?;
    assert!(older.len() == 1 && older[0].id == first.id);
    Ok([first.id, second.id])
}

pub(super) async fn scenario(
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &mut KeyClient<'_>,
    text: Scope,
) -> Result<(Scope, [u8; 32])> {
    let [original, b_message] = exchange(a, b, text, [TEXT_A, TEXT_B]).await?;
    let b_person = *b.identity.certificate.person_id();
    let b_id = encode(&b_person);
    let a_person = *a.identity.certificate.person_id();
    let dm = a.create_dm(&b_person).await?;
    receive_key(b, dm, 1).await?;
    assert_eq!(b.create_dm(&a_person).await?, dm);
    let [dm_message, _] = exchange(a, b, dm, [DM_A, DM_B]).await?;
    for (client, peer) in [(&*a, b_person), (&*b, a_person)] {
        let direct = client
            .catalog()
            .await?
            .channels
            .into_iter()
            .find(|channel| channel.id == dm.id)
            .context("DM missing from participant catalog")?;
        assert!(direct.kind == "dm" && direct.category_id.is_none() && direct.name_ct.is_none());
        assert_eq!(direct.peer, Some(encode(&peer)));
    }
    assert!(
        c.catalog()
            .await?
            .channels
            .iter()
            .all(|channel| channel.id != dm.id)
    );
    rejected(
        c.request(Method::GET, &path(dm, "")).send().await?,
        StatusCode::FORBIDDEN,
        "channel_read_denied",
    )
    .await?;
    let state: KeyState = c
        .request(Method::GET, "/v1/keys")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(state.scopes.iter().all(|row| row.scope != dm));
    assert!(state.pending.iter().all(|row| row.scope != dm));
    a.delete_message(&dm.id, &dm_message).await?;
    wait_event(b, dm, &dm_message, true).await?;

    // The owner is also an outsider to this pair, including on the management route and socket.
    let private = b.create_dm(c.identity.certificate.person_id()).await?;
    assert!(
        a.catalog()
            .await?
            .channels
            .iter()
            .all(|channel| channel.id != private.id)
    );
    let admin_channels: Vec<Value> = a
        .request(Method::GET, "/v1/channels")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(admin_channels.iter().all(|channel| channel["kind"] != "dm"));
    for request in [
        a.request(Method::GET, &path(private, "")),
        a.request(
            Method::DELETE,
            &format!("/v1/channels/{}", encode(&private.id)),
        ),
    ] {
        let reply = request.send().await?;
        assert!(matches!(
            reply.status(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ));
    }

    let mut edit = body(EDIT);
    edit.edit_of = Some(original);
    let record = a.send_message(&text.id, &edit).await?;
    wait_event(b, text, &record.id, false).await?;
    let dm_id = encode(&dm.id);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match c.next_event().await?.context("outsider socket closed")? {
                Event::DmCreated { channel }
                | Event::MessageCreated { channel, .. }
                | Event::MessageDeleted { channel, .. }
                    if channel == dm_id =>
                {
                    anyhow::bail!("DM notice reached an outsider")
                }
                Event::MessageCreated { id, .. } if id == encode(&record.id) => return Ok(()),
                Event::KeysNeeded(need) if need.scope == dm => {
                    anyhow::bail!("DM key notice reached an outsider")
                }
                _ => (),
            }
        }
    })
    .await
    .context("waiting for a public-channel event on outsider socket")??;
    let delivered = b.message(&text.id, &record.id).await?;
    assert!(b.open_message(&text.id, &delivered).await? == edit);
    assert_eq!(
        b.send_message(&text.id, &edit)
            .await
            .err()
            .and_then(|error| error.downcast::<MessageError>().ok()),
        Some(MessageError::EditAuthor)
    );

    // A custom client can upload a signed edit, but the encrypted reference only makes sense to recipients.
    let mut forged = body(FORGED_EDIT);
    forged.edit_of = Some(original);
    let key = b.key(text, 1).context("missing channel key")?;
    let context = MessageContext {
        server_id: b.space().id,
        channel_id: text.id,
        epoch: 1,
        id: *keys::random_key()?,
        sender_device: b.identity.certificate.signing_key().to_bytes(),
        person: b_person,
    };
    let envelope = context.seal(key, b.identity, &forged)?;
    let malicious = b
        .request(Method::POST, &path(text, ""))
        .json(&json!({ "id": encode(&context.id), "epoch": 1, "envelope": encode(&envelope) }))
        .send()
        .await?;
    assert_eq!(malicious.status(), StatusCode::CREATED);
    wait_event(a, text, &context.id, false).await?;
    let delivered = a.message(&text.id, &context.id).await?;
    assert_eq!(
        a.open_message(&text.id, &delivered)
            .await
            .err()
            .and_then(|error| error.downcast::<MessageError>().ok()),
        Some(MessageError::EditAuthor)
    );

    // Deleting an original also deletes the edits its author's client sent.
    a.delete_message(&text.id, &original).await?;
    wait_event(b, text, &record.id, true).await?;
    rejected(
        b.request(
            Method::GET,
            &path(text, &format!("/{}", encode(&record.id))),
        )
        .send()
        .await?,
        StatusCode::NOT_FOUND,
        "message_not_found",
    )
    .await?;

    let delete = b.send_message(&text.id, &body(DELETE)).await?;
    b.delete_message(&text.id, &delete.id).await?;
    wait_event(a, text, &delete.id, true).await?;
    let moderated = a.send_message(&text.id, &body(MOD_DELETE)).await?;
    let suffix = format!("/{}", encode(&moderated.id));
    rejected(
        b.request(Method::DELETE, &path(text, &suffix))
            .send()
            .await?,
        StatusCode::FORBIDDEN,
        "permission_denied",
    )
    .await?;
    a.request(Method::PUT, &format!("/v1/members/{b_id}/roles"))
        .json(&json!({"role_ids": ["moderator", "member"]}))
        .send()
        .await?
        .error_for_status()?;
    b.delete_message(&text.id, &moderated.id).await?;
    wait_event(a, text, &moderated.id, true).await?;
    rejected(
        a.request(Method::GET, &path(text, &suffix)).send().await?,
        StatusCode::NOT_FOUND,
        "message_not_found",
    )
    .await?;
    let audit: Vec<Value> = a
        .request(Method::GET, "/v1/audit")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        audit.iter().any(|row| row["action"] == "message_deleted"
            && row["actor_person_id"] == b_id
            && row["target_id"] == encode(&moderated.id)
            && row["target_person_id"].is_null()),
        "missing moderator delete audit"
    );
    println!("W07 verified two-way text and DMs, paging, edits, replays and audited moderation");
    Ok((dm, b_message))
}

pub(super) async fn after_kick(
    a: &KeyClient<'_>,
    c: &mut KeyClient<'_>,
    text: Scope,
    dm: Scope,
    record: [u8; 32],
) -> Result<()> {
    assert!(
        a.catalog()
            .await?
            .channels
            .iter()
            .all(|channel| channel.id != dm.id)
    );
    rejected(
        a.request(Method::GET, &path(dm, "")).send().await?,
        StatusCode::FORBIDDEN,
        "channel_read_denied",
    )
    .await?;
    let earlier = c.message(&text.id, &record).await?;
    assert!(c.open_message(&text.id, &earlier).await?.text == TEXT_B);
    println!("W07 kick ended DM access and retained verifiable text history");
    Ok(())
}
