use super::{pin_dm_as_peer, receive_key, rejected};
use anyhow::{Context, Result, ensure};
use reqwest::{Method, StatusCode};
use rusttalk_core::{
    auth::encode,
    client::{Catalog, Event, KeyClient, MessageBody, MessageError, SlowMode, UploadRejected},
    keys::{self, KeyState, Metadata, Scope},
    messages::{
        Ballot, MessageContext, Poll, PollPart, Reaction, ReactionEmoji, reaction_vote, tally,
        tally_reactions,
    },
    presence::Update,
};
use serde_json::{Value, json};

const TEXT_A: &str = "W07 Quartz Text from A";
const TEXT_B: &str = "W07 Amethyst Text from B";
const DM_A: &str = "W07 Citrine Private Message from A";
const DM_B: &str = "W07 Opal Private Message from B";
const EDIT: &str = "W07 Turquoise Edited Message";
const FORGED_EDIT: &str = "W07 Garnet Impostor Edit";
const DELETE: &str = "W07 Pearl Author Deleted Message";
const MOD_DELETE: &str = "W07 Amber Moderator Deleted Message";
const VOICE_A: &str = "W61 Jade Voice Chat from A";
const VOICE_B: &str = "W61 Onyx Voice Chat from B";
const POLL_QUESTION: &str = "W204 Malachite Poll Question";
const POLL_FIRST: &str = "W204 Beryl Poll Answer";
const POLL_SECOND: &str = "W204 Zircon Poll Answer";
const REACTED: &str = "#511 Peridot Message With Reactions";

const PAGE: &str = "#210 Malachite Page by a moderator";
const POST: &str = "# #211 Topaz Forum Post\nby a member";
const REPLY: &str = "#211 Beryl Forum Reply";
const SLOW_FIRST: &str = "#660 Jasper First Slow Message";
const SLOW_EDIT: &str = "#660 Agate Edited Slow Message";
pub(super) const TEXTS: [&str; 19] = [
    TEXT_A,
    TEXT_B,
    DM_A,
    DM_B,
    EDIT,
    FORGED_EDIT,
    DELETE,
    MOD_DELETE,
    VOICE_A,
    VOICE_B,
    POLL_QUESTION,
    POLL_FIRST,
    POLL_SECOND,
    REACTED,
    PAGE,
    POST,
    REPLY,
    SLOW_FIRST,
    SLOW_EDIT,
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

pub(super) async fn wait_event(
    client: &mut KeyClient<'_>,
    scope: Scope,
    id: &[u8; 32],
    deleted: bool,
) -> Result<()> {
    let channel = encode(&scope.id);
    let id = encode(id);
    let record = path(scope, &format!("/{id}"));
    super::wait::until(
        client,
        "message event",
        // The record is stored before its notice goes out, so it settles the wait as well.
        async |client| {
            let status = client.request(Method::GET, &record).send().await?.status();
            ensure!(
                status.is_success() || status == StatusCode::NOT_FOUND,
                "reading the message got {status}"
            );
            Ok(status.is_success() != deleted)
        },
        |update| match update {
            Update::Event(Event::MessageCreated {
                channel: c, id: m, ..
            }) => !deleted && *c == channel && *m == id,
            Update::Event(Event::MessageDeleted { channel: c, id: m }) => {
                deleted && *c == channel && *m == id
            }
            _ => false,
        },
    )
    .await
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
    reply.quoted_author = Some(first.person);
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
    let category = a.create_category(b"W61 Voice chat").await?;
    let voice = a
        .create_channel(&category, "voice", b"W61 Lounge", b"")
        .await?;
    let ask = rusttalk_core::messages::VoiceRequest::Ask(
        voice.id,
        rusttalk_core::index::now_ms().context("timestamp")? / 1000,
    );
    let signal = |request| MessageBody {
        voice_request: Some(request),
        ..Default::default()
    };
    let first = a.send_message(&dm.id, &signal(ask)).await?;
    receive_key(b, dm, 1).await?;
    let delivered = b.message(&dm.id, &first.id).await?;
    assert_eq!(
        b.open_message(&dm.id, &delivered).await?.voice_request,
        Some(ask)
    );
    // B asking for the same DM gets A's, pins it and re-keys it for the two of them.
    assert_eq!(b.create_dm(&a_person).await?, dm);
    pin_dm_as_peer(b, a, dm).await?;
    let answer = rusttalk_core::messages::VoiceRequest::Answer(
        first.id,
        rusttalk_core::messages::VoiceAnswer::Later,
    );
    let reply = b.send_message(&dm.id, &signal(answer)).await?;
    let delivered = a.message(&dm.id, &reply.id).await?;
    assert_eq!(
        a.open_message(&dm.id, &delivered).await?.voice_request,
        Some(answer)
    );
    println!("W294 first-ever DM delivered the encrypted invitation and its matching answer");
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
    // About the notices themselves: C's socket is older than the message, so it can't miss them.
    super::wait::within("a public-channel event on outsider socket", async {
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
    .await?;
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
    // #525: DMs are private to their two people, so `delete_messages` doesn't reach into them.
    // Neither B, now a moderator, nor A, the owner, can delete the other's DM message or file.
    for (client, message) in [(&*b, first.id), (&*a, reply.id)] {
        let suffix = format!("/{}", encode(&message));
        rejected(
            client
                .request(Method::DELETE, &path(dm, &suffix))
                .send()
                .await?,
            StatusCode::FORBIDDEN,
            "permission_denied",
        )
        .await?;
        let kept = b.message(&dm.id, &message).await?;
        assert_eq!(kept.id, message);
    }
    let attachment = a
        .upload_file(
            &dm.id,
            "w07-dm.bin",
            "application/octet-stream",
            &[7; 64],
            None,
        )
        .await?;
    let file = encode(&attachment.file.id);
    rejected(
        b.request(Method::DELETE, &format!("/v1/files/{file}"))
            .query(&[("uploader", encode(&a_person))])
            .send()
            .await?,
        StatusCode::FORBIDDEN,
        "permission_denied",
    )
    .await?;
    b.download_file(&attachment.file, None).await?;
    a.delete_message_file(&attachment.file.id, &a_person)
        .await?;
    rejected(
        b.request(Method::GET, &format!("/v1/files/{file}"))
            .send()
            .await?,
        StatusCode::NOT_FOUND,
        "file_not_found",
    )
    .await?;
    println!("#525 verified only the author deletes their DM messages and files");
    println!("W07 verified two-way text and DMs, paging, edits, replays and audited moderation");

    // A voice channel has the same chat as a text channel, under its own channel key.
    a.sync().await?;
    receive_key(b, voice, 1).await?;
    exchange(a, b, voice, [VOICE_A, VOICE_B]).await?;
    // Later key checks expect only the first text channel, so remove this one.
    a.request(
        Method::DELETE,
        &format!("/v1/channels/{}", encode(&voice.id)),
    )
    .send()
    .await?
    .error_for_status()?;
    println!("W61 verified two-way chat, replies and paging in a voice channel");
    poll(a, b, c, text).await?;
    reactions(a, b, c, text).await?;
    page(a, b, c, &category).await?;
    forum(a, b, c, &category).await?;
    slow_mode(a, b, c, text).await?;
    nsfw(a, b, c, text).await?;
    new_member_rules(a, b, c, text).await?;
    timeouts(a, b, c, text, &category).await?;
    Ok((dm, b_message))
}

/// #664: only a server manager turns on new member rules, which is audited. C, who just
/// joined with only the default role, then may not upload files or start a DM; B, a
/// moderator, may. Turned off, C uploads again.
async fn new_member_rules(
    a: &KeyClient<'_>,
    b: &KeyClient<'_>,
    c: &KeyClient<'_>,
    text: Scope,
) -> Result<()> {
    const PATH: &str = "/v1/settings/new-member-rules";
    let set = |client: &KeyClient<'_>, secs: i64| {
        (client.request(Method::PUT, PATH))
            .json(&json!({ "secs": secs }))
            .send()
    };
    rejected(
        set(b, 3600).await?,
        StatusCode::FORBIDDEN,
        "permission_denied",
    )
    .await?;
    set(a, 3600).await?.error_for_status()?;
    ensure!(
        me(c).await?["new_member_until"].is_i64(),
        "C is not a new member"
    );
    ensure!(
        me(b).await?.get("new_member_until").is_none(),
        "B is a new member"
    );

    async fn upload(client: &KeyClient<'_>, channel: &[u8; 32]) -> Result<()> {
        (client.upload_file(
            channel,
            "664.bin",
            "application/octet-stream",
            &[6; 64],
            None,
        ))
        .await
        .map(drop)
    }
    let refused = upload(c, &text.id)
        .await
        .err()
        .context("a new member uploaded a file")?;
    let code = refused
        .downcast_ref::<UploadRejected>()
        .map(|r| r.code.as_str());
    assert_eq!(code, Some("new_member_rules"));
    upload(b, &text.id).await?;
    let dm = c
        .request(Method::POST, "/v1/dms")
        .json(&json!({
            "id": encode(&keys::random_key()?[..]),
            "peer": encode(a.identity.certificate.person_id()),
            "commitment": encode(&[0; 32]),
            "envelope": "",
        }))
        .send()
        .await?;
    rejected(dm, StatusCode::FORBIDDEN, "new_member_rules").await?;

    let audit: Vec<Value> = (a.request(Method::GET, "/v1/audit").send().await?)
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        audit
            .iter()
            .any(|row| row["action"] == "new_member_rules_changed"
                && row["details"]["after"] == 3600),
        "missing new member rules audit"
    );
    set(a, 0).await?.error_for_status()?;
    upload(c, &text.id).await?;
    println!(
        "#664 verified new member rules: managed, audited, enforced for new default members only"
    );
    Ok(())
}

async fn me(client: &KeyClient<'_>) -> Result<Value> {
    Ok((client.request(Method::GET, "/v1/me").send().await?)
        .error_for_status()?
        .json()
        .await?)
}

/// #663: moderator B times out member C, who may only read: no message, reaction or voice
/// join. C's app hears at once; A ends it early; both are audited.
async fn timeouts(
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &mut KeyClient<'_>,
    text: Scope,
    category: &[u8; 32],
) -> Result<()> {
    let b_person = *b.identity.certificate.person_id();
    let c_person = encode(c.identity.certificate.person_id());
    let path = format!("/v1/members/{c_person}/timeout");
    let reason = c.encrypt_metadata(
        keys::Metadata::TimeoutReason,
        c.identity.certificate.person_id(),
        b"#663 Obsidian reason",
    )?;
    rejected(
        c.request(
            Method::PUT,
            &format!("/v1/members/{}/timeout", encode(&b_person)),
        )
        .json(&json!({ "seconds": 60 }))
        .send()
        .await?,
        StatusCode::FORBIDDEN,
        "permission_denied",
    )
    .await?;
    rejected(
        b.request(Method::PUT, &path)
            .json(&json!({ "seconds": 61 }))
            .send()
            .await?,
        StatusCode::BAD_REQUEST,
        "invalid_timeout",
    )
    .await?;
    b.request(Method::PUT, &path)
        .json(&json!({ "seconds": 600, "reason_ct": reason }))
        .send()
        .await?
        .error_for_status()?;

    // C's own app learns it from the live event, then reads the reason from /v1/me.
    let until = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(Event::MemberChanged {
                person,
                profile: Some(profile),
                ..
            }) = c.next_event().await?
                && person == c_person
                && let Some(until) = profile.timeout_until
            {
                return Ok::<_, anyhow::Error>(until);
            }
        }
    })
    .await
    .context("C's app heard nothing of the timeout")??;
    let me: Value = c
        .request(Method::GET, "/v1/me")
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(me["timeout_until"], until);
    let shown = c.decrypt_metadata(
        keys::Metadata::TimeoutReason,
        c.identity.certificate.person_id(),
        me["timeout_reason_ct"].as_str().context("reason missing")?,
    )?;
    assert_eq!(shown, b"#663 Obsidian reason");

    let refused = |result: Result<_>| {
        result
            .err()
            .and_then(|error| error.downcast::<reqwest::Error>().ok())
            .and_then(|error| error.status())
    };
    let posted = a.send_message(&text.id, &body(TEXT_A)).await?;
    let reaction = MessageBody {
        poll: Some(reaction_vote(
            &posted.id,
            &ReactionEmoji::Unicode("👍".into()).key(),
            1,
            true,
        )),
        ..Default::default()
    };
    for message in [body(TEXT_B), reaction] {
        let result = c.send_message(&text.id, &message).await.map(|_| ());
        assert_eq!(refused(result), Some(StatusCode::FORBIDDEN));
    }
    // C may see a lounge, so only the timeout refuses the join.
    let voice = a
        .create_channel(category, "voice", b"#663 Lounge", b"")
        .await?;
    let lounge = |flags: u8| {
        a.request(
            Method::PUT,
            &format!("/v1/channels/{}/permissions/member", encode(&voice.id)),
        )
        .json(&json!({ "flags": flags }))
        .send()
    };
    lounge(1).await?.error_for_status()?;
    rejected(
        c.request(
            Method::POST,
            &format!("/v1/voice/{}/token", encode(&voice.id)),
        )
        .send()
        .await?,
        StatusCode::FORBIDDEN,
        "timed_out",
    )
    .await?;
    // Later steps expect C to hold no other channel keys.
    a.request(
        Method::DELETE,
        &format!("/v1/channels/{}", encode(&voice.id)),
    )
    .send()
    .await?
    .error_for_status()?;

    a.request(Method::DELETE, &path)
        .send()
        .await?
        .error_for_status()?;
    rejected(
        a.request(Method::DELETE, &path).send().await?,
        StatusCode::NOT_FOUND,
        "timeout_not_found",
    )
    .await?;
    c.send_message(&text.id, &body(TEXT_B)).await?;
    let audit: Vec<Value> = a
        .request(Method::GET, "/v1/audit")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for action in ["member_timed_out", "member_timeout_ended"] {
        ensure!(
            audit
                .iter()
                .any(|row| row["action"] == action && row["target_person_id"] == c_person),
            "missing {action} audit"
        );
    }
    println!(
        "#663 verified timeouts: ranked, live, enforced on messages, reactions and voice, ended early, audited"
    );
    Ok(())
}

/// A posts a poll, B and C vote (B changes their mind), and A counts the votes it reads.
async fn poll(
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &mut KeyClient<'_>,
    text: Scope,
) -> Result<()> {
    let open = MessageBody {
        text: POLL_QUESTION.into(),
        poll: Some(PollPart::Open(Poll {
            options: vec![POLL_FIRST.into(), POLL_SECOND.into()],
            multiple: false,
            ends_at: 0,
        })),
        ..Default::default()
    };
    let posted = a.send_message(&text.id, &open).await?;
    wait_event(b, text, &posted.id, false).await?;
    let received = b.message(&text.id, &posted.id).await?;
    assert!(b.open_message(&text.id, &received).await? == open);
    let vote = |sent, choices: &[u8]| MessageBody {
        poll: Some(PollPart::Vote {
            poll: posted.id,
            sent,
            choices: choices.to_vec(),
        }),
        ..Default::default()
    };
    b.send_message(&text.id, &vote(1, &[0])).await?;
    b.send_message(&text.id, &vote(2, &[1])).await?;
    let last = c.send_message(&text.id, &vote(1, &[1])).await?;
    wait_event(a, text, &last.id, false).await?;
    let end = MessageBody {
        poll: Some(PollPart::End { poll: posted.id }),
        ..Default::default()
    };
    a.send_message(&text.id, &end).await?;

    // A counts from what it reads back, as the app does.
    let (mut ballots, mut ends) = (Vec::new(), Vec::new());
    let mut question = None;
    // Just the poll's five records: an older page reaches W07's forged edit, whose original
    // is deleted, so opening it fails.
    for record in a.message_page(&text.id, None, 5).await? {
        match a.open_message(&text.id, &record).await?.poll {
            Some(PollPart::Open(poll)) if record.id == posted.id => question = Some(poll),
            Some(PollPart::Vote {
                poll,
                sent,
                choices,
            }) if poll == posted.id => ballots.push(Ballot {
                person: record.person,
                at: record.at,
                sent,
                id: record.id,
                choices,
            }),
            Some(PollPart::End { poll }) if poll == posted.id => {
                ends.push((record.person, record.at))
            }
            _ => (),
        }
    }
    let poll = question.context("the poll was not in the page")?;
    let result = tally(&poll, a.identity.certificate.person_id(), &ballots, &ends);
    let counts: Vec<usize> = result.voters.iter().map(Vec::len).collect();
    ensure!(
        counts == [0, 2] && result.people == 2,
        "unexpected counts {counts:?}"
    );
    ensure!(result.ended, "the creator's end did not count");
    println!("W204 verified an encrypted poll: two votes, a changed vote and the creator's end");
    Ok(())
}

/// #511: B and C react to A's message (C takes one back), and A tallies what it reads.
async fn reactions(
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &mut KeyClient<'_>,
    text: Scope,
) -> Result<()> {
    let posted = a.send_message(&text.id, &body(REACTED)).await?;
    wait_event(b, text, &posted.id, false).await?;
    wait_event(c, text, &posted.id, false).await?;
    let heart = ReactionEmoji::Unicode("❤️".into()).key();
    let thumbs = ReactionEmoji::Unicode("👍".into()).key();
    let react = |emoji, sent, on| MessageBody {
        poll: Some(reaction_vote(&posted.id, &emoji, sent, on)),
        ..Default::default()
    };
    b.send_message(&text.id, &react(heart, 1, true)).await?;
    c.send_message(&text.id, &react(heart, 1, true)).await?;
    c.send_message(&text.id, &react(heart, 2, false)).await?;
    let last = c.send_message(&text.id, &react(thumbs, 3, true)).await?;
    wait_event(a, text, &last.id, false).await?;

    // A finds them by the message ID's first half, as the app does.
    let mut found = Vec::new();
    for record in a.message_page(&text.id, None, 5).await? {
        let Some(part) = a.open_message(&text.id, &record).await?.poll else {
            continue;
        };
        if let Some((target, reaction)) =
            Reaction::from_vote(&part, record.person, record.at, record.id)
            && target == posted.id[..16]
        {
            found.push(reaction);
        }
    }
    // Records from the same second are ordered by their random IDs, so sort.
    let mut result = tally_reactions(&found, |emoji| [heart, thumbs].contains(emoji));
    result.sort();
    let (b_id, c_id) = (
        *b.identity.certificate.person_id(),
        *c.identity.certificate.person_id(),
    );
    let mut expected = [(heart, vec![b_id]), (thumbs, vec![c_id])];
    expected.sort();
    ensure!(result == expected, "unexpected reactions {result:?}");
    println!("#511 verified encrypted reactions: two people, one taken back");
    Ok(())
}

/// #660: only a channel manager turns on slow mode, which is audited. Then C, a member,
/// waits between messages (but not for an edit), while B, a moderator, doesn't.
async fn slow_mode(
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &mut KeyClient<'_>,
    text: Scope,
) -> Result<()> {
    let catalog = a.catalog().await?;
    let channel = (catalog.channels.iter())
        .find(|channel| channel.id == text.id)
        .context("text channel missing")?;
    let name = a.decrypt_metadata(
        Metadata::ChannelName,
        &text.id,
        channel
            .name_ct
            .as_deref()
            .context("channel without a name")?,
    )?;
    let update = json!({
        "category_id": channel.category_id,
        "position": channel.position,
        "name_ct": a.encrypt_metadata(Metadata::ChannelName, &text.id, &name)?,
    });
    let set = |client: &KeyClient<'_>, secs: u32| {
        let mut update = update.clone();
        update["slow_mode_secs"] = secs.into();
        client
            .request(Method::PUT, &format!("/v1/channels/{}", encode(&text.id)))
            .json(&update)
            .send()
    };
    rejected(
        set(b, 60).await?,
        StatusCode::FORBIDDEN,
        "permission_denied",
    )
    .await?;
    rejected(
        set(a, 7).await?,
        StatusCode::BAD_REQUEST,
        "invalid_slow_mode",
    )
    .await?;
    set(a, 60).await?.error_for_status()?;
    let listed = a
        .catalog()
        .await?
        .channels
        .into_iter()
        .find(|c| c.id == text.id);
    assert_eq!(listed.and_then(|c| c.slow_mode_secs), Some(60));
    let audit: Vec<Value> = a
        .request(Method::GET, "/v1/audit")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        audit.iter().any(|row| row["action"] == "slow_mode_changed"
            && row["target_id"] == encode(&text.id)
            && row["details"]["after"] == 60),
        "missing slow mode audit"
    );

    // The app marks follow-ups such as edits in a slow mode channel.
    c.slow_mode.insert(text.id);
    let first = c.send_message(&text.id, &body(SLOW_FIRST)).await?;
    let refused = c.send_message(&text.id, &body(SLOW_FIRST)).await;
    let wait = refused
        .err()
        .and_then(|error| error.downcast::<SlowMode>().ok())
        .context("a second message went through slow mode")?;
    ensure!((1..=60).contains(&wait.0), "unexpected wait {}", wait.0);
    let mut edit = body(SLOW_EDIT);
    edit.edit_of = Some(first.id);
    c.send_message(&text.id, &edit).await?;
    // A moderator bypasses it.
    for _ in 0..2 {
        b.send_message(&text.id, &body(SLOW_FIRST)).await?;
    }
    set(a, 0).await?.error_for_status()?;
    c.send_message(&text.id, &body(SLOW_FIRST)).await?;
    c.slow_mode.clear();
    println!(
        "#660 verified slow mode: managed, audited, enforced per member, bypassed by moderators"
    );
    Ok(())
}

/// #681: only a channel manager marks a channel NSFW, which is audited; every reader's
/// catalog says so, and unmarking takes it away again.
async fn nsfw(
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &mut KeyClient<'_>,
    text: Scope,
) -> Result<()> {
    let catalog = a.catalog().await?;
    let channel = (catalog.channels.iter())
        .find(|channel| channel.id == text.id)
        .context("text channel missing")?;
    ensure!(!channel.nsfw, "a new channel starts unmarked");
    let name = a.decrypt_metadata(
        Metadata::ChannelName,
        &text.id,
        channel
            .name_ct
            .as_deref()
            .context("channel without a name")?,
    )?;
    let update = json!({
        "category_id": channel.category_id,
        "position": channel.position,
        "name_ct": a.encrypt_metadata(Metadata::ChannelName, &text.id, &name)?,
    });
    let set = |client: &KeyClient<'_>, nsfw: bool| {
        let mut update = update.clone();
        update["nsfw"] = nsfw.into();
        client
            .request(Method::PUT, &format!("/v1/channels/{}", encode(&text.id)))
            .json(&update)
            .send()
    };
    rejected(
        set(b, true).await?,
        StatusCode::FORBIDDEN,
        "permission_denied",
    )
    .await?;
    set(a, true).await?.error_for_status()?;
    let marked = |catalog: Catalog| {
        (catalog.channels.into_iter()).any(|channel| channel.id == text.id && channel.nsfw)
    };
    ensure!(
        marked(c.catalog().await?),
        "a member doesn't see the NSFW mark"
    );
    let audit: Vec<Value> = a
        .request(Method::GET, "/v1/audit")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        audit.iter().any(|row| row["action"] == "nsfw_changed"
            && row["target_id"] == encode(&text.id)
            && row["details"]["after"] == true),
        "missing NSFW audit"
    );
    set(a, false).await?.error_for_status()?;
    ensure!(!marked(c.catalog().await?), "the NSFW mark stayed");
    println!("#681 verified NSFW channels: managed, audited, shown to every reader");
    Ok(())
}

/// #211: a forum is a text channel; a reply carries its post in the encrypted `reply_to`.
async fn forum(
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &mut KeyClient<'_>,
    category: &[u8; 32],
) -> Result<()> {
    let forum = a.create_channel(category, "forum", b"Board", b"").await?;
    a.sync().await?;
    receive_key(b, forum, 1).await?;
    receive_key(c, forum, 1).await?;
    let listed = c.catalog().await?.channels;
    let listed = listed.iter().find(|channel| channel.id == forum.id);
    ensure!(
        listed.is_some_and(|channel| channel.kind == "forum"),
        "forum not listed as a forum"
    );
    let post = c.send_message(&forum.id, &body(POST)).await?;
    let mut reply = body(REPLY);
    reply.reply_to = Some(post.id);
    let answer = b.send_message(&forum.id, &reply).await?;
    wait_event(c, forum, &answer.id, false).await?;
    let received = c.message(&forum.id, &answer.id).await?;
    ensure!(c.open_message(&forum.id, &received).await? == reply);
    a.request(
        Method::DELETE,
        &format!("/v1/channels/{}", encode(&forum.id)),
    )
    .send()
    .await?
    .error_for_status()?;
    println!("#211 verified a forum: members post, replies point at their post");
    Ok(())
}

/// #210: a page channel is a text channel only staff may write to; everyone reads it.
async fn page(
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &mut KeyClient<'_>,
    category: &[u8; 32],
) -> Result<()> {
    let page = a.create_channel(category, "page", b"Rules", b"").await?;
    a.sync().await?;
    receive_key(b, page, 1).await?;
    receive_key(c, page, 1).await?;
    let listed = c.catalog().await?.channels;
    let listed = listed.iter().find(|channel| channel.id == page.id);
    ensure!(
        listed.is_some_and(|channel| channel.kind == "page"),
        "page not listed as a page"
    );
    // C is a plain member: read only. B is a moderator: may change the page.
    let Err(refused) = c.send_message(&page.id, &body(PAGE)).await else {
        anyhow::bail!("a member changed a page");
    };
    ensure!(
        format!("{refused:#}").contains("403"),
        "unexpected refusal: {refused:#}"
    );
    let saved = b.send_message(&page.id, &body(PAGE)).await?;
    wait_event(c, page, &saved.id, false).await?;
    let [newest] = c
        .message_page(&page.id, None, 1)
        .await?
        .try_into()
        .ok()
        .context("no page")?;
    ensure!(c.open_message(&page.id, &newest).await?.text == PAGE);
    // #226: a page's picture is an encrypted file. A uploads it, B saves the page again
    // with the same reference, and C still opens it from the newest version.
    let picture = b"not really a png, just page picture bytes";
    let mut first = body(PAGE);
    first.attachments = vec![
        a.upload_file(&page.id, "map.png", "image/png", picture, None)
            .await?,
    ];
    let first = a.send_message(&page.id, &first).await?;
    let mut again = body("Updated rules");
    // B reads the page's picture reference, as the app does before saving.
    again.attachments = b
        .open_message(&page.id, &b.message(&page.id, &first.id).await?)
        .await?
        .attachments;
    let saved = b.send_message(&page.id, &again).await?;
    wait_event(c, page, &saved.id, false).await?;
    let newest_picture = async |c: &mut KeyClient<'_>| -> Result<Vec<u8>> {
        let [newest] = c
            .message_page(&page.id, None, 1)
            .await?
            .try_into()
            .ok()
            .context("no page")?;
        let opened = c.open_message(&page.id, &newest).await?;
        let kept = opened
            .attachments
            .first()
            .context("page lost its picture")?;
        c.download_file(&kept.file, None).await
    };
    ensure!(newest_picture(c).await? == picture);
    // B, a moderator, deletes their version and asks to remove its files the way an
    // older app does: the picture is A's, so it stays for the version before.
    b.delete_message(&page.id, &saved.id).await?;
    let file = again.attachments.first().context("no picture")?;
    ensure!(
        b.delete_message_file(&file.file.id, &saved.person)
            .await
            .is_err(),
        "a moderator removed another member's page picture"
    );
    wait_event(c, page, &saved.id, true).await?;
    ensure!(newest_picture(c).await? == picture);
    // #269: a picture the newest page version keeps outlives file expiry (12 s in e2e)
    // and expires once a newer version drops it. Only editors may save a version.
    let mut kept = body("Pinned rules");
    kept.attachments = vec![
        a.upload_file(&page.id, "kept.png", "image/png", picture, None)
            .await?,
    ];
    let file = kept.attachments[0].file.clone();
    let id = *keys::random_key()?;
    ensure!(
        c.post_page(&page.id, id, &kept).await.is_err(),
        "a member saved a page version"
    );
    let pinned = a.post_page(&page.id, id, &kept).await?;
    wait_event(c, page, &pinned.id, false).await?;
    tokio::time::sleep(std::time::Duration::from_secs(18)).await;
    ensure!(
        c.download_file(&file, None).await? == picture,
        "the page lost its kept picture"
    );
    let dropped = a
        .post_page(
            &page.id,
            *keys::random_key()?,
            &body("Rules without pictures"),
        )
        .await?;
    wait_event(c, page, &dropped.id, false).await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while c.download_file(&file, None).await.is_ok() {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    })
    .await
    .context("a dropped page picture did not expire")?;
    // Later key checks expect only the first text channel, so remove this one.
    a.request(
        Method::DELETE,
        &format!("/v1/channels/{}", encode(&page.id)),
    )
    .send()
    .await?
    .error_for_status()?;
    println!(
        "#210/#226/#269 verified a page channel: members read it and its pictures, only moderators and up change it, kept pictures outlive file expiry"
    );
    Ok(())
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
