//! The server's names, channels, members and permissions, decrypted for the UI.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::{Context, Result};
use eframe::egui::ColorImage;
use reqwest::{Method, StatusCode};
use rusttalk_core::{
    auth::{decode, encode},
    client::{Catalog, Channel as WireChannel, KeyClient, decode_profile_first_frame},
    identity::fingerprint,
    keys::{MemberProfile, Metadata, Part},
    presence::{Presence, Status},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use rusttalk_core::client::voice::{DEFAULT_AUDIO_KBPS, MAX_AUDIO_KBPS};

use super::{Ctx, State, Update, leave_voice, now, recent::Store};
use crate::server::{
    Body, Calendar, Category, Channel, CustomEmoji, Device, Directory, Dm, EmojiPack, Id, Member,
    Msg, Perms, Picture, Role, SystemMessages, member_name, parse_role_display,
};

pub(super) async fn get(client: &KeyClient<'_>, path: &str) -> Result<Value> {
    Ok(client
        .request(Method::GET, path)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Save a channel order, or return false if the server lacks the endpoint.
/// Everything this person can't see keeps its place.
pub(super) async fn save_order(client: &KeyClient<'_>, shown: &[(Id, Vec<Id>)]) -> Result<bool> {
    let id = |value: &Value| -> Result<Id> { Ok(decode(value.as_str().context("missing ID")?)?) };
    let categories = get(client, "/v1/categories").await?;
    let categories: Vec<Id> = (categories.as_array().into_iter().flatten())
        .map(|category| id(&category["id"]))
        .collect::<Result<_>>()?;
    let channels = get(client, "/v1/channels").await?;
    let channels: Vec<(Id, Id)> = (channels.as_array().into_iter().flatten())
        .map(|channel| Ok((id(&channel["id"])?, id(&channel["category_id"])?)))
        .collect::<Result<_>>()?;
    let order = crate::reorder::merge(shown, &categories, &channels);
    let body: Vec<Value> = (order.iter())
        .map(|(category, list)| {
            let list: Vec<String> = list.iter().map(|channel| encode(channel)).collect();
            serde_json::json!({"id": encode(category), "channels": list})
        })
        .collect();
    let reply = client
        .request(Method::PUT, "/v1/channels/order")
        .json(&serde_json::json!({ "categories": body }))
        .send()
        .await?;
    match reply.status() {
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED => return Ok(false),
        StatusCode::BAD_REQUEST => {
            // Older servers treat "order" as a channel ID and reject its encoding.
            let error = reply.json::<Value>().await?;
            if error["error"] == "invalid_encoding" {
                return Ok(false);
            }
            anyhow::bail!("channel order rejected");
        }
        _ => reply.error_for_status()?,
    };
    Ok(true)
}

pub(super) async fn rename_channel(
    client: &KeyClient<'_>,
    id: &Id,
    name: &str,
    topic: Option<&str>,
    audio_kbps: Option<u32>,
    member_limit: Option<u32>,
    slow_mode_secs: Option<u32>,
) -> Result<()> {
    // The update also replaces category and position. Read them just before saving.
    let catalog = client.catalog().await?;
    let channel = catalog
        .channels
        .iter()
        .find(|channel| &channel.id == id)
        .context("channel no longer visible")?;
    let current_topic = if topic.is_none() {
        channel
            .topic_ct
            .as_deref()
            .map(|ct| client.decrypt_metadata(Metadata::ChannelTopic, id, ct))
            .transpose()?
    } else {
        None
    };
    let topic = topic.map(str::as_bytes).or(current_topic.as_deref());
    let mut body = channel_update(channel, name, topic, |kind, text| {
        client.encrypt_metadata(kind, id, text)
    })?;
    if let Some(kbps) = audio_kbps {
        body["audio_kbps"] = kbps.into();
        body["audio_kbps_max"] = MAX_AUDIO_KBPS.into();
    }
    if let Some(limit) = member_limit {
        body["member_limit"] = limit.into();
    }
    if let Some(secs) = slow_mode_secs {
        body["slow_mode_secs"] = secs.into();
    }
    client
        .request(Method::PUT, &format!("/v1/channels/{}", encode(id)))
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

fn channel_update(
    channel: &WireChannel,
    name: &str,
    topic: Option<&[u8]>,
    encrypt: impl Fn(Metadata, &[u8]) -> Result<String>,
) -> Result<Value> {
    Ok(json!({
        "category_id": channel.category_id.as_deref().context("channel has no category")?,
        "position": channel.position,
        "name_ct": encrypt(Metadata::ChannelName, name.as_bytes())?,
        "topic_ct": topic.map(|topic| encrypt(Metadata::ChannelTopic, topic)).transpose()?,
    }))
}

pub(super) async fn rename_category(client: &KeyClient<'_>, id: &Id, name: &str) -> Result<()> {
    let categories = managed_categories(client).await?;
    let category = categories
        .iter()
        .find(|category| &category.id == id)
        .context("category no longer exists")?;
    let body = category_update(category.position, name, |kind, text| {
        client.encrypt_metadata(kind, id, text)
    })?;
    client
        .request(Method::PUT, &format!("/v1/categories/{}", encode(id)))
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

fn category_update(
    position: i64,
    name: &str,
    encrypt: impl Fn(Metadata, &[u8]) -> Result<String>,
) -> Result<Value> {
    Ok(json!({
        "position": position,
        "name_ct": encrypt(Metadata::CategoryName, name.as_bytes())?,
    }))
}

async fn managed_categories(
    client: &KeyClient<'_>,
) -> Result<Vec<rusttalk_core::client::Category>> {
    Ok(client
        .request(Method::GET, "/v1/categories")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// The server replies a directory is built from, kept so a notice can refetch only its part.
pub(super) struct Sources {
    /// With every category for channel managers, who see empty ones too.
    catalog: Catalog,
    members: Value,
    me: Value,
    roles: Value,
    space: Value,
    emoji: Value,
}

/// Fetch and decrypt names, channels, members and permissions, and report member changes.
pub(super) async fn directory(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
) -> Result<()> {
    // The parts don't depend on each other, so they cost one round trip instead of seven.
    let shared = &*client;
    let (catalog, members, me, roles, space, emoji, calendar) = tokio::join!(
        shared.catalog(),
        get(shared, "/v1/members"),
        get(shared, "/v1/me"),
        get(shared, "/v1/roles"),
        get(shared, "/v1/space"),
        emoji_list(shared),
        super::calendar::load(shared),
    );
    let mut catalog = catalog?;
    discard_unreadable(ctx, url, state, &catalog);
    let (members, me, roles) = (members?, me?, roles?);
    let (space, emoji, calendar) = (space?, emoji?, calendar?);
    if manages_channels(&me, &roles) {
        catalog.categories = managed_categories(client).await?;
    }
    let sources = Sources {
        catalog,
        members,
        me,
        roles,
        space,
        emoji,
    };
    rebuild(ctx, url, client, state, sources, calendar).await
}

/// Refetch only what a `channel_changed` notice says changed, then rebuild the directory.
pub(super) async fn reload(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    part: Part,
) -> Result<()> {
    // Taken while loading, so a failed load leaves none and the next notice loads it all.
    let Some(mut sources) = state.raw.take() else {
        return directory(ctx, url, client, state).await;
    };
    let mut calendar = state.last.as_ref().and_then(|dir| dir.calendar.clone());
    match part {
        Part::Channels => {
            sources.catalog = client.catalog().await?;
            discard_unreadable(ctx, url, state, &sources.catalog);
            if manages_channels(&sources.me, &sources.roles) {
                sources.catalog.categories = managed_categories(client).await?;
            }
        }
        Part::Space => sources.space = get(client, "/v1/space").await?,
        Part::Emoji => sources.emoji = emoji_list(client).await?,
        Part::Calendar => calendar = super::calendar::load(client).await?,
        Part::Other => return directory(ctx, url, client, state).await,
    }
    rebuild(ctx, url, client, state, sources, calendar).await
}

/// Apply one member's new profile from a `member_changed` notice, fetching at most their
/// picture.
pub(super) async fn profile(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    person: &str,
    profile: &MemberProfile,
) -> Result<()> {
    let own = encode(ctx.identity.certificate.person_id()) == person;
    let Some(mut sources) = state.raw.take() else {
        return directory(ctx, url, client, state).await;
    };
    if !patch_profile(&mut sources, person, profile, own) {
        // Not listed yet: a member this directory doesn't know, so load it all.
        return directory(ctx, url, client, state).await;
    }
    let calendar = state.last.as_ref().and_then(|dir| dir.calendar.clone());
    rebuild(ctx, url, client, state, sources, calendar).await
}

/// Write a profile into the member's row (and the own `/v1/me` reply), as a fresh
/// `/v1/members` would list it. False when the member isn't listed.
fn patch_profile(sources: &mut Sources, person: &str, profile: &MemberProfile, own: bool) -> bool {
    let Some(row) = member_row(&mut sources.members, person) else {
        return false;
    };
    row["display_name_ct"] = json!(profile.display_name_ct);
    row["avatar_hash"] = json!(profile.avatar_hash);
    row["bio_ct"] = json!(profile.bio_ct);
    row["hide_offline"] = json!(profile.hide_offline);
    row["avatar_initialized"] = json!(profile.avatar_initialized);
    row["voice_muted"] = json!(profile.voice_muted);
    if own {
        sources.me["hide_offline"] = json!(profile.hide_offline);
    }
    true
}

fn member_row<'a>(members: &'a mut Value, person: &str) -> Option<&'a mut Value> {
    (members.as_array_mut()?.iter_mut()).find(|row| row["person_id"].as_str() == Some(person))
}

/// Keep the kept member list's presence current, so a rebuild without a fresh member
/// list doesn't show an old one.
pub(super) fn presence_changed(state: &mut State, person: &str, presence: Presence) {
    if let Some(row) = (state.raw.as_mut()).and_then(|s| member_row(&mut s.members, person)) {
        row["presence"] = json!(presence);
    }
}

/// Keep the kept own status current after setting it.
pub(super) fn status_changed(state: &mut State, status: Status) {
    if let Some(sources) = &mut state.raw {
        sources.me["status"] = json!(status);
    }
}

/// Forget channels and DMs the catalog no longer lists for us. A successful catalog is
/// authoritative, also when a later request of the same load fails.
fn discard_unreadable(ctx: &Ctx, url: &Url, state: &mut State, catalog: &Catalog) {
    if let Some(before) = &state.last {
        let readable: HashMap<_, _> = catalog.channels.iter().map(|c| (c.id, c)).collect();
        let channels = before
            .categories
            .iter()
            .flat_map(|c| &c.channels)
            .filter(|c| readable.get(&c.id).is_none_or(|new| new.kind == "dm"))
            .map(|c| c.id);
        let dms = before
            .dms
            .iter()
            .filter(|dm| {
                readable.get(&dm.channel).is_none_or(|new| {
                    new.kind != "dm"
                        || new.peer.as_deref().and_then(|p| decode(p).ok()) != Some(dm.peer)
                })
            })
            .map(|dm| dm.channel);
        state.discard_channels(ctx, url, channels.chain(dms).collect());
    }
}

/// Whether this member's roles grant `manage_channels`, as the directory's permissions read.
fn manages_channels(me: &Value, roles: &Value) -> bool {
    let mine = me["role_ids"].as_array();
    (roles["roles"].as_array().into_iter().flatten()).any(|role| {
        mine.is_some_and(|mine| mine.contains(&role["id"]))
            && (role["permissions"].as_array())
                .is_some_and(|flags| flags.contains(&json!("manage_channels")))
    })
}

/// Decrypt the sources into a directory, show it and report member changes.
async fn rebuild(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    sources: Sources,
    calendar: Option<Calendar>,
) -> Result<()> {
    let Sources {
        catalog,
        members,
        me,
        roles,
        space,
        emoji,
    } = &sources;
    client.slow_mode = (catalog.channels.iter())
        .filter(|channel| channel.slow_mode_secs.is_some_and(|secs| secs > 0))
        .map(|channel| channel.id)
        .collect();
    let joined_kbps = state.voice.as_ref().and_then(|live| {
        let channel = catalog.channels.iter().find(|c| c.id == live.channel)?;
        channel.audio_kbps
    });
    // Shown with the pictures already decoded; the serve loop loads the rest when idle,
    // from the disk cache or the server.
    let (pictures, _, _) =
        known_pictures(client.space().id, members, space, emoji, &state.pictures)?;
    let mut dir = build(
        client,
        catalog,
        members,
        me,
        roles,
        space,
        emoji,
        &|id: &Id, hash: Option<&str>| pictures.get(&picture_key(id, hash?)).cloned(),
    )?;
    // A DM channel stays with the person it was first listed with. One the server
    // now lists with someone else is not shown, so nothing is sent there.
    dir.dms.retain(|dm| {
        let kept = client.bind_dm(dm.channel, dm.peer);
        if !kept && state.relabelled.insert(dm.channel) {
            tracing::warn!(
                lost = state.pins_lost,
                "not using a DM the server relabelled, or whose pin was lost"
            );
            let server = url.host_str().unwrap_or(t!("net.a_server"));
            ctx.send(Update::Failed(match state.pins_lost {
                true => t!("net.dm_pins_unreadable", server = server),
                false => t!("net.dm_skipped", server = server),
            }));
        }
        kept
    });
    dir.calendar = calendar;
    super::save_pins(ctx, url, client, state);
    state.pictures = pictures;
    state.pictures_pending = true;
    if let Some(before) = &state.last
        && let Some(channel) = dir.notice_channel()
    {
        for (person, action, detail) in changes(before, &dir) {
            let name = member_name(&dir, &person);
            let name = if dir.members.iter().any(|m| m.person == person) {
                name
            } else {
                member_name(before, &person)
            };
            state.notices += 1;
            let mut id = state.server_id;
            id[..8].copy_from_slice(&state.notices.to_be_bytes());
            let msg = Msg {
                backfilled: false,
                id,
                person,
                at: now(),
                reply_to: None,
                quoted_author: None,
                edit_of: None,
                body: Body::Notice {
                    name,
                    action,
                    detail,
                },
                files: Vec::new(),
                live: None,
                sticker: None,
                replicas: Vec::new(),
                copy_files: Vec::new(),
            };
            ctx.send(Update::Notice(url.clone(), channel, msg));
        }
    }
    // Everyone in the call follows the channel's bitrate, also after a manager changes it.
    if let Some(kbps) = joined_kbps {
        super::voice::set_audio_kbps(state, kbps).await;
    }
    let joined = state.voice.as_ref().map(|live| live.channel);
    let deleted = joined.filter(|joined| {
        !dir.categories
            .iter()
            .flat_map(|category| &category.channels)
            .any(|channel| channel.id == *joined)
    });
    state.prune_history(&dir);
    super::recent::Listing::save(state, &dir, members, space);
    state.avatar_uninitialized =
        own_avatar_uninitialized(members, ctx.identity.certificate.person_id());
    state.raw = Some(sources);
    state.last = Some(dir.clone());
    state.named = client.has_space_key();
    ctx.send(Update::Directory(url.clone(), Box::new(dir)));
    if let Some(channel) = deleted {
        leave_voice(ctx, url, client, state);
        ctx.send(Update::Call(url.clone(), channel, None));
    }
    super::cache_own_avatar(ctx, client, state);
    Ok(())
}

/// True only when the server explicitly advertises an uninitialized, empty own avatar.
/// Older servers omit `avatar_initialized`, so this returns false there.
pub(super) fn own_avatar_uninitialized(members: &Value, person: &Id) -> bool {
    let person = encode(person);
    members
        .as_array()
        .into_iter()
        .flatten()
        .find(|member| member["person_id"].as_str() == Some(person.as_str()))
        .is_some_and(|member| {
            member["avatar_initialized"] == false && member["avatar_hash"].is_null()
        })
}

/// The server's emoji and stickers, or none on servers from before them.
async fn emoji_list(client: &KeyClient<'_>) -> Result<Value> {
    let reply = client.request(Method::GET, "/v1/emoji").send().await?;
    if reply.status() == StatusCode::NOT_FOUND {
        return Ok(Value::Null);
    }
    Ok(reply.error_for_status()?.json().await?)
}

#[allow(clippy::too_many_arguments)]
fn build(
    client: &KeyClient<'_>,
    catalog: &Catalog,
    members: &Value,
    me: &Value,
    roles: &Value,
    space: &Value,
    emoji: &Value,
    picture: &impl Fn(&Id, Option<&str>) -> Option<Picture>,
) -> Result<Directory> {
    let bytes = |kind: Metadata, id: &[u8], ct: Option<&str>| {
        let plain = client.decrypt_metadata(kind, id, ct?);
        if let Err(error) = &plain {
            tracing::warn!(error = %format_args!("{error:#}"), "name could not be decrypted");
        }
        plain.ok()
    };
    let text = |kind: Metadata, id: &[u8], ct: Option<&str>| {
        Some(String::from_utf8_lossy(&bytes(kind, id, ct)?).into_owned())
    };
    let strings = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect()
    };
    let my_roles = strings(&me["role_ids"]);
    let mut perms = Perms {
        owner: my_roles.iter().any(|role| role == "owner"),
        admin: my_roles
            .iter()
            .any(|role| role == "owner" || role == "admin"),
        rank: -1,
        ..Perms::default()
    };
    let mut role_list = Vec::new();
    for role in roles["roles"].as_array().into_iter().flatten() {
        let id = role["id"].as_str().context("role without ID")?.to_owned();
        let permissions = strings(&role["permissions"]);
        let rank = role["rank"].as_i64().unwrap_or_default();
        if my_roles.contains(&id) {
            perms.rank = perms.rank.max(rank);
            for permission in &permissions {
                match permission.as_str() {
                    "manage_channels" => perms.manage_channels = true,
                    "invite" => perms.invite = true,
                    "delete_messages" => perms.delete_messages = true,
                    "kick" => perms.kick = true,
                    "ban" => perms.ban = true,
                    "manage_roles" => perms.manage_roles = true,
                    "view_audit_log" => perms.audit = true,
                    "manage_server" => perms.manage_server = true,
                    "manage_emoji" => perms.manage_emoji = true,
                    "mute_members" => perms.mute_members = true,
                    "move_members" => perms.move_members = true,
                    "suggest_events" => perms.suggest_events = true,
                    "create_events" => perms.create_events = true,
                    "manage_events" => perms.manage_events = true,
                    "start_focus" => perms.start_focus = true,
                    "bypass_slow_mode" => perms.bypass_slow_mode = true,
                    _ => {}
                }
            }
        }
        let kind = role["kind"].as_str().unwrap_or_default();
        let builtin = match kind {
            "owner" => t!("role.owner"),
            "admin" => t!("role.admin"),
            "moderator" => t!("role.moderator"),
            "member" => t!("role.member"),
            _ => t!("role.new"),
        };
        let name = text(Metadata::RoleName, id.as_bytes(), role["name_ct"].as_str());
        let (color, separate) = bytes(
            Metadata::RoleDisplay,
            id.as_bytes(),
            role["display_ct"].as_str(),
        )
        .map_or((None, false), |display| parse_role_display(&display));
        role_list.push(Role {
            name_known: name.is_some() || (kind != "custom" && role["name_ct"].is_null()),
            name: name.unwrap_or_else(|| builtin.into()),
            id,
            rank,
            permissions,
            custom: kind == "custom",
            color,
            separate,
        });
    }
    // Highest rank first, as the server lists them.
    role_list.sort_by_key(|role| -role.rank);

    let mut categories: Vec<Category> = (catalog.categories.iter())
        .map(|category| Category {
            id: Some(category.id),
            name: text(
                Metadata::CategoryName,
                &category.id,
                Some(&category.name_ct),
            )
            .unwrap_or_else(|| t!("server.default_category").into()),
            channels: Vec::new(),
        })
        .collect();
    let mut loose = Vec::new();
    let mut dms = Vec::new();
    // Text before voice only where positions tie, as on servers nobody has reordered yet.
    let mut channels: Vec<_> = catalog.channels.iter().collect();
    channels.sort_by_key(|channel| (channel.position, channel.kind == "voice", channel.id));
    for channel in channels {
        if channel.kind == "dm" {
            if let Some(peer) = channel.peer.as_deref() {
                dms.push(Dm {
                    channel: channel.id,
                    peer: decode(peer)?,
                });
            }
            continue;
        }
        let entry = Channel {
            id: channel.id,
            name: text(
                Metadata::ChannelName,
                &channel.id,
                channel.name_ct.as_deref(),
            )
            .unwrap_or_else(|| t!("server.default_channel").into()),
            topic: text(
                Metadata::ChannelTopic,
                &channel.id,
                channel.topic_ct.as_deref(),
            )
            .unwrap_or_default(),
            voice: channel.kind == "voice",
            page: channel.kind == "page",
            forum: channel.kind == "forum",
            audio_kbps: channel.audio_kbps.unwrap_or(DEFAULT_AUDIO_KBPS),
            member_limit: channel.member_limit.unwrap_or(0),
            focus_until: channel.focus_until,
            slow_mode_secs: channel.slow_mode_secs.unwrap_or(0),
            flags: (channel.flags.iter())
                .map(|entry| (entry.role_id.clone(), entry.flags))
                .collect(),
        };
        let category = categories.iter_mut().find(|category| {
            category
                .id
                .is_some_and(|id| channel.category_id.as_deref() == Some(encode(&id).as_str()))
        });
        match category {
            Some(category) => category.channels.push(entry),
            None => loose.push(entry),
        }
    }
    // Voice-only categories follow the rest only where positions tie.
    let position = |id: &Option<Id>| {
        (catalog.categories.iter())
            .find(|category| Some(category.id) == *id)
            .map_or(0, |category| category.position)
    };
    categories.sort_by_key(|c| {
        let voice = !c.channels.is_empty() && c.channels.iter().all(|ch| ch.voice);
        (position(&c.id), voice, c.id)
    });
    if !loose.is_empty() {
        categories.insert(
            0,
            Category {
                id: None,
                name: String::new(),
                channels: loose,
            },
        );
    }

    let mut list = Vec::new();
    let my_id = *client.identity.certificate.person_id();
    let mut my_name_hidden = false;
    for row in members.as_array().into_iter().flatten() {
        let person: Id = decode(row["person_id"].as_str().context("member without ID")?)?;
        let name = text(
            Metadata::DisplayName,
            &person,
            row["display_name_ct"].as_str(),
        );
        if person == my_id && name.is_none() && row["display_name_ct"].is_string() {
            my_name_hidden = true;
        }
        // My own name stays as typed, so I can see it and fix it in settings.
        let name = if person == my_id {
            name
        } else {
            crate::profile::received_name(name)
        };
        let devices = client
            .devices()
            .filter(|device| device.person_id() == &person)
            .map(|device| Device {
                signing_key: device.signing_key().to_bytes(),
                name: device.name().to_owned(),
                fingerprint: fingerprint(device.signing_key().as_bytes()),
            })
            .collect();
        list.push(Member {
            person,
            name,
            devices,
            roles: strings(&row["role_ids"]),
            avatar: picture(&person, row["avatar_hash"].as_str()),
            presence: serde_json::from_value(row["presence"].clone()).unwrap_or_default(),
            hide_offline: row["hide_offline"].as_bool().unwrap_or_default(),
            bio: received_bio(text(Metadata::Bio, &person, row["bio_ct"].as_str())),
            voice_muted: row["voice_muted"].as_bool().unwrap_or_default(),
        });
    }
    list.sort_by_cached_key(|member| {
        (
            member.name.is_none(),
            member.name.as_ref().map(|n| n.to_lowercase()),
        )
    });
    let mut emoji_items = Vec::new();
    for row in emoji["items"].as_array().into_iter().flatten() {
        let id: Id = decode(row["id"].as_str().context("emoji without ID")?)?;
        // Without a readable name it can't be typed or shown, so leave it out.
        let Some(name) = text(Metadata::EmojiName, &id, row["name_ct"].as_str()) else {
            continue;
        };
        emoji_items.push(CustomEmoji {
            id,
            name,
            sticker: row["kind"] == "sticker",
            pack: row["pack_id"]
                .as_str()
                .and_then(|pack| decode(pack).ok())
                .unwrap_or_default(),
            picture: picture(&id, row["image_hash"].as_str()),
        });
    }
    let mut packs = Vec::new();
    for row in emoji["packs"].as_array().into_iter().flatten() {
        let id: Id = decode(row["id"].as_str().context("pack without ID")?)?;
        // The Default pack has no name until someone renames it.
        packs.push(EmojiPack {
            id,
            name: text(Metadata::EmojiPackName, &id, row["name_ct"].as_str()),
        });
    }
    let limit = |key: &str| emoji[key].as_u64().unwrap_or_default() as usize;
    let server_id = client.space().id;
    let system_messages_supported = space.get("system_messages_ct").is_some();
    let system_messages = if space["system_messages_ct"].is_null() {
        SystemMessages::First
    } else {
        bytes(
            Metadata::SystemMessages,
            &server_id,
            space["system_messages_ct"].as_str(),
        )
        .and_then(|v| Id::try_from(v.as_slice()).ok())
        .map_or(SystemMessages::None, SystemMessages::Channel)
    };
    let first_text_channel = if system_messages_supported {
        space["first_text_channel"]
            .as_str()
            .map(decode)
            .transpose()?
    } else {
        categories
            .iter()
            .flat_map(|c| &c.channels)
            .find(|c| !c.page && !c.voice && !c.forum)
            .map(|c| c.id)
    };
    Ok(Directory {
        system_messages_supported,
        system_messages,
        first_text_channel,
        name: text(Metadata::ServerName, &server_id, space["name_ct"].as_str()),
        icon: picture(&server_id, space["icon_hash"].as_str()),
        status: serde_json::from_value(me["status"].clone()).ok(),
        hide_offline: me["hide_offline"].as_bool(),
        my_name_hidden,
        bios: (members.as_array().into_iter().flatten()).any(|row| row.get("bio_ct").is_some()),
        me: *client.identity.certificate.person_id(),
        perms,
        categories,
        members: list.into(),
        dms,
        roles: role_list,
        default_role: roles["default_role_id"].as_str().unwrap_or_default().into(),
        emoji: emoji_items,
        emoji_max: (limit("max_emoji"), limit("max_stickers")),
        packs,
        packs_max: limit("max_packs"),
        calendar: None,
    })
}

/// A bio as decrypted, capped like the editor caps it, so nobody can send a huge card.
fn received_bio(bio: Option<String>) -> Option<String> {
    bio.map(|bio| crate::profile::fit_bio(&bio))
}

/// The cache key of a member's avatar or the server icon: its owner and ciphertext hash.
/// The owner is part of the key so one member cannot reuse another's picture.
pub(super) fn picture_key(owner: &Id, hash: &str) -> [u8; 32] {
    Sha256::new()
        .chain_update(owner)
        .chain_update(hash.as_bytes())
        .finalize()
        .into()
}

/// Decoded pictures by [`picture_key`].
type Pictures = HashMap<[u8; 32], Picture>;

/// A picture a directory shows that isn't decoded yet.
struct Missing {
    kind: Metadata,
    id: Id,
    key: [u8; 32],
    path: String,
    field: &'static str,
}

/// The pictures these sources show: those already decoded in `last`, those to load, and
/// the keys of all of them, which the disk cache keeps.
fn known_pictures(
    space_id: Id,
    members: &Value,
    space: &Value,
    emoji: &Value,
    last: &HashMap<[u8; 32], Picture>,
) -> Result<(Pictures, Vec<Missing>, HashSet<[u8; 32]>)> {
    let mut wanted = Vec::new();
    for row in members.as_array().into_iter().flatten() {
        if let (Some(id), Some(hash)) = (row["person_id"].as_str(), row["avatar_hash"].as_str()) {
            let path = format!("/v1/members/{id}/avatar");
            wanted.push((Metadata::Avatar, decode::<32>(id)?, hash, path, "avatar_ct"));
        }
    }
    if let Some(hash) = space["icon_hash"].as_str() {
        let path = "/v1/space/icon".to_owned();
        wanted.push((Metadata::ServerIcon, space_id, hash, path, "icon_ct"));
    }
    for row in emoji["items"].as_array().into_iter().flatten() {
        if let (Some(id), Some(hash)) = (row["id"].as_str(), row["image_hash"].as_str()) {
            let path = format!("/v1/emoji/{id}/image");
            wanted.push((
                Metadata::EmojiImage,
                decode::<32>(id)?,
                hash,
                path,
                "image_ct",
            ));
        }
    }
    let mut pictures = HashMap::new();
    let mut missing = Vec::new();
    let mut keys = HashSet::new();
    for (kind, id, hash, path, field) in wanted {
        let key = picture_key(&id, hash);
        keys.insert(key);
        match last.get(&key) {
            Some(known) => {
                pictures.insert(key, known.clone());
            }
            None => missing.push(Missing {
                kind,
                id,
                key,
                path,
                field,
            }),
        }
    }
    Ok((pictures, missing, keys))
}

/// Load pictures from the encrypted disk cache, or else fetch and decode them, at most
/// [`super::PARALLEL`] at a time so a slow one doesn't hold up the rest. New ones are
/// cached, and the cache drops what isn't in `keys`. Those that fail are left out and
/// tried again next load.
async fn fetch_pictures(
    client: &KeyClient<'_>,
    mut missing: Vec<Missing>,
    store: Arc<Store>,
    keys: HashSet<[u8; 32]>,
) -> Result<Pictures> {
    // Disk reads and decoding block, so they run off the two network workers.
    let disk = store.clone();
    let (mut pictures, missing) = tokio::task::spawn_blocking(move || {
        let mut cached = HashMap::new();
        missing.retain(|m| {
            let found = disk.picture(&m.key).and_then(|bytes| picture(&bytes));
            found.map(|found| cached.insert(m.key, found)).is_none()
        });
        (cached, missing)
    })
    .await?;
    let replies = super::bounded(
        (missing.into_iter())
            .map(|picture| async move {
                let reply = get(client, &picture.path).await;
                (picture, reply)
            })
            .collect(),
    )
    .await;
    let mut fetched = Vec::new();
    for (picture, reply) in replies {
        match reply {
            Ok(reply) => {
                if let Some(ct) = reply[picture.field].as_str() {
                    fetched.push((picture, ct.to_owned()));
                }
            }
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "picture could not be fetched");
            }
        }
    }
    let mut new = Vec::new();
    // Decrypted and decoded here, off the UI.
    tokio::task::block_in_place(|| {
        for (missing, ct) in fetched {
            let bytes = match client.decrypt_metadata(missing.kind, &missing.id, &ct) {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::warn!(error = %format_args!("{error:#}"), "picture could not be decrypted");
                    continue;
                }
            };
            if let Some(picture) = picture(&bytes) {
                new.push((missing.key, picture.bytes.clone()));
                pictures.insert(missing.key, picture);
            }
        }
    });
    tokio::task::spawn_blocking(move || store.save_pictures(&new, &keys)).await?;
    Ok(pictures)
}

/// Load the pictures the last directory went without, then show it again with them. Only
/// tried once per load: what fails waits for the next one. Also runs with none missing,
/// so the disk cache drops pictures the directory no longer lists.
pub(super) async fn fill_pictures(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
) -> Result<()> {
    state.pictures_pending = false;
    let Some(sources) = &state.raw else {
        return Ok(());
    };
    let (_, missing, keys) = known_pictures(
        client.space().id,
        &sources.members,
        &sources.space,
        &sources.emoji,
        &state.pictures,
    )?;
    let store = state.cache.clone().expect("started server");
    let fetched = fetch_pictures(client, missing, store, keys).await?;
    if fetched.is_empty() {
        return Ok(());
    }
    state.pictures.extend(fetched);
    let Some(sources) = state.raw.take() else {
        return Ok(());
    };
    let calendar = state.last.as_ref().and_then(|dir| dir.calendar.clone());
    rebuild(ctx, url, client, state, sources, calendar).await?;
    state.pictures_pending = false;
    Ok(())
}

/// A decoded frame as an image and how long it shows, in ms.
pub(crate) fn frame_image(frame: image::Frame) -> (ColorImage, u32) {
    let (numer, denom) = frame.delay().numer_denom_ms();
    // Browsers also slow down GIF frames shorter than 20 ms.
    let ms = (numer / denom.max(1)).max(20);
    let image = frame.into_buffer();
    let size = [image.width() as usize, image.height() as usize];
    (ColorImage::from_rgba_unmultiplied(size, image.as_raw()), ms)
}

/// Decode a profile picture, server icon, emoji or sticker. Of a GIF only the first frame
/// is decoded here; the rest wait until it plays, as most never do.
pub(crate) fn picture(bytes: &[u8]) -> Option<Picture> {
    let key: [u8; 32] = Sha256::digest(bytes).into();
    let first = match decode_profile_first_frame(bytes) {
        Ok(first) => frame_image(first),
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "picture could not be decoded");
            return None;
        }
    };
    let bytes = Arc::new(bytes.to_vec());
    Some(match image::guess_format(&bytes) {
        Ok(image::ImageFormat::Gif) => Picture::lazy(key, first, bytes),
        _ => Picture::new(key, vec![first], bytes),
    })
}

/// Membership changes between two directory loads: (person, action, details).
fn changes(before: &Directory, after: &Directory) -> Vec<(Id, &'static str, String)> {
    let mut out = Vec::new();
    for member in &after.members {
        if !before.members.iter().any(|old| old.person == member.person) {
            let id = t!("home.detail_id", fingerprint = fingerprint(&member.person));
            out.push((member.person, "joined", id));
        }
    }
    for old in &before.members {
        if after
            .members
            .iter()
            .all(|member| member.person != old.person)
        {
            let id = t!("home.detail_id", fingerprint = fingerprint(&old.person));
            out.push((old.person, "left", id));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_new_pictures_are_left_to_fetch() -> Result<()> {
        let (a, b, emoji_id) = (encode(&[1; 32]), encode(&[2; 32]), encode(&[3; 32]));
        let members = json!([
            {"person_id": a, "avatar_hash": "known"},
            {"person_id": b, "avatar_hash": "new"},
            {"person_id": encode(&[4; 32]), "avatar_hash": null},
        ]);
        let space = json!({"icon_hash": "icon"});
        let emoji = json!({"items": [{"id": emoji_id, "image_hash": "smile"}]});
        let known = picture(include_bytes!("../../assets/rusttalk.png")).context("decodes")?;
        let last = HashMap::from([
            (picture_key(&[1; 32], "known"), known.clone()),
            // Not shown anymore, so dropped.
            (picture_key(&[2; 32], "old"), known),
        ]);
        let (pictures, missing, keys) = known_pictures([9; 32], &members, &space, &emoji, &last)?;
        assert_eq!(
            pictures.keys().collect::<Vec<_>>(),
            [&picture_key(&[1; 32], "known")]
        );
        let missing: Vec<_> = (missing.iter())
            .map(|m| (m.key, m.path.as_str(), m.field))
            .collect();
        assert_eq!(
            missing,
            [
                (
                    picture_key(&[2; 32], "new"),
                    format!("/v1/members/{b}/avatar").as_str(),
                    "avatar_ct"
                ),
                (picture_key(&[9; 32], "icon"), "/v1/space/icon", "icon_ct"),
                (
                    picture_key(&[3; 32], "smile"),
                    format!("/v1/emoji/{emoji_id}/image").as_str(),
                    "image_ct"
                ),
            ]
        );
        // The disk cache keeps every picture shown, decoded or not, and only those.
        let shown = [
            picture_key(&[1; 32], "known"),
            picture_key(&[2; 32], "new"),
            picture_key(&[9; 32], "icon"),
            picture_key(&[3; 32], "smile"),
        ];
        assert_eq!(keys, HashSet::from(shown));
        Ok(())
    }

    #[test]
    fn a_profile_notice_updates_only_that_member() {
        let row = |id: &str, name: &str| {
            json!({"person_id": id, "presence": "online", "display_name_ct": name,
                "avatar_hash": "h1", "avatar_initialized": true, "bio_ct": null,
                "role_ids": ["member"], "hide_offline": false})
        };
        let mut sources = Sources {
            catalog: Catalog {
                channels: Vec::new(),
                categories: Vec::new(),
            },
            members: json!([row("a", "old"), row("b", "other")]),
            me: json!({"hide_offline": false, "role_ids": ["member"]}),
            roles: Value::Null,
            space: Value::Null,
            emoji: Value::Null,
        };
        let mut profile = MemberProfile {
            display_name_ct: Some("new".into()),
            avatar_hash: Some("h1".into()),
            bio_ct: Some("bio".into()),
            hide_offline: true,
            avatar_initialized: true,
            voice_muted: true,
        };
        assert!(patch_profile(&mut sources, "a", &profile, true));
        let mut expected = row("a", "new");
        expected["bio_ct"] = json!("bio");
        expected["hide_offline"] = json!(true);
        expected["voice_muted"] = json!(true);
        assert_eq!(sources.members[0], expected);
        assert_eq!(sources.members[1], row("b", "other"));
        assert_eq!(sources.me["hide_offline"], true);
        profile.avatar_hash = None;
        assert!(patch_profile(&mut sources, "a", &profile, false));
        assert!(sources.members[0]["avatar_hash"].is_null());
        // Someone this directory doesn't list yet needs a full load.
        assert!(!patch_profile(&mut sources, "c", &profile, false));
    }

    #[test]
    fn only_a_role_with_the_flag_manages_channels() {
        let roles = json!({"roles": [
            {"id": "member", "permissions": ["invite"]},
            {"id": "mods", "permissions": ["manage_channels"]},
        ]});
        assert!(!manages_channels(&json!({"role_ids": ["member"]}), &roles));
        assert!(manages_channels(
            &json!({"role_ids": ["member", "mods"]}),
            &roles
        ));
        assert!(!manages_channels(&Value::Null, &roles));
    }

    #[test]
    fn channel_update_keeps_order_and_encrypts_name_and_topic() {
        use rusttalk_core::{auth::decode_bytes, keys::random_key};

        let server = [1; 32];
        let id = [2; 32];
        let key = random_key().unwrap();
        let category_id = encode(&[3; 32]);
        let channel = WireChannel {
            id,
            kind: "text".into(),
            category_id: Some(category_id.clone()),
            position: 7,
            name_ct: None,
            topic_ct: None,
            peer: None,
            audio_kbps: None,
            member_limit: None,
            focus_until: None,
            slow_mode_secs: None,
            flags: Vec::new(),
        };
        let encrypt =
            |kind: Metadata, text: &[u8]| Ok(encode(&kind.seal(&server, &id, 4, &key, text)?));
        let body = channel_update(&channel, "Release notes", Some(b"What's new"), encrypt).unwrap();
        assert_eq!(body["category_id"], category_id);
        assert_eq!(body["position"], 7);
        for (field, kind, expected) in [
            (
                "name_ct",
                Metadata::ChannelName,
                b"Release notes".as_slice(),
            ),
            ("topic_ct", Metadata::ChannelTopic, b"What's new".as_slice()),
        ] {
            let bytes = decode_bytes(body[field].as_str().unwrap()).unwrap();
            assert_eq!(kind.epoch(&bytes).unwrap(), 4);
            assert_eq!(kind.open(&server, &id, &key, &bytes).unwrap(), expected);
        }
        assert!(
            channel_update(&channel, "Release notes", None, encrypt).unwrap()["topic_ct"].is_null()
        );
    }

    #[test]
    fn category_update_keeps_position_and_encrypts_name() {
        use rusttalk_core::{auth::decode_bytes, keys::random_key};

        let server = [1; 32];
        let id = [3; 32];
        let key = random_key().unwrap();
        let body = category_update(11, "Projects", |kind, text| {
            Ok(encode(&kind.seal(&server, &id, 4, &key, text)?))
        })
        .unwrap();
        assert_eq!(body["position"], 11);
        let bytes = decode_bytes(body["name_ct"].as_str().unwrap()).unwrap();
        assert_eq!(
            Metadata::CategoryName
                .open(&server, &id, &key, &bytes)
                .unwrap(),
            b"Projects",
        );
    }

    fn member(person: u8, devices: &[&str]) -> Member {
        Member {
            person: [person; 32],
            name: Some(format!("P{person}")),
            devices: devices
                .iter()
                .map(|fp| Device {
                    signing_key: [0; 32],
                    name: "Desktop".into(),
                    fingerprint: (*fp).into(),
                })
                .collect(),
            roles: Vec::new(),
            avatar: None,
            presence: Default::default(),
            hide_offline: false,
            bio: None,
            voice_muted: false,
        }
    }

    #[test]
    fn a_received_bio_is_capped_like_a_typed_one() {
        let hostile = format!("{}{}", "x\n".repeat(1000), "y".repeat(5000));
        let bio = super::received_bio(Some(hostile)).unwrap();
        assert_eq!(bio.lines().count(), crate::profile::BIO_LINES);
        assert!(bio.chars().count() <= crate::profile::BIO_CHARS);
        assert_eq!(super::received_bio(None), None);
    }

    #[test]
    fn reports_joins_and_leaves_but_skips_device_changes() {
        let dir = |members: Vec<Member>| Directory {
            members: members.into(),
            ..Directory::default()
        };
        let before = dir(vec![member(1, &["a"]), member(2, &[]), member(3, &["c"])]);
        let after = dir(vec![
            member(1, &["a", "b"]),
            member(2, &["d"]),
            member(4, &[]),
        ]);
        let lines: Vec<_> = changes(&before, &after)
            .into_iter()
            .map(|(person, action, _)| (person[0], action))
            .collect();
        assert_eq!(lines, [(4, "joined"), (3, "left")]);
    }

    #[test]
    fn own_avatar_uninitialized_matches_only_unset_new_server_rows() {
        let me = [1; 32];
        let other = [2; 32];
        let me_id = encode(&me);
        let other_id = encode(&other);

        // New server, never set or cleared: eligible for default-avatar repair.
        let unset = json!([
            { "person_id": me_id, "avatar_initialized": false, "avatar_hash": null }
        ]);
        assert!(own_avatar_uninitialized(&unset, &me));

        // Explicitly cleared on a new server: never re-upload.
        let cleared = json!([
            { "person_id": me_id, "avatar_initialized": true, "avatar_hash": null }
        ]);
        assert!(!own_avatar_uninitialized(&cleared, &me));

        // Already has a picture: never re-upload.
        let present = json!([
            { "person_id": me_id, "avatar_initialized": true, "avatar_hash": "hash" }
        ]);
        assert!(!own_avatar_uninitialized(&present, &me));

        // Older server omits avatar_initialized: never send conditional writes or poll.
        let legacy = json!([
            { "person_id": me_id, "avatar_hash": null }
        ]);
        assert!(!own_avatar_uninitialized(&legacy, &me));

        // Only this account's own row counts.
        let other_unset = json!([
            { "person_id": other_id, "avatar_initialized": false, "avatar_hash": null },
            { "person_id": me_id, "avatar_initialized": true, "avatar_hash": null }
        ]);
        assert!(!own_avatar_uninitialized(&other_unset, &me));
    }

    fn gif(frames: u8) -> Vec<u8> {
        use image::{Delay, Frame, Rgba, RgbaImage, codecs::gif::GifEncoder};
        let mut bytes = Vec::new();
        let mut encoder = GifEncoder::new(&mut bytes);
        (encoder.encode_frames((0..frames).map(|index| {
            let image = RgbaImage::from_pixel(4, 4, Rgba([index * 40, 30, 60, 255]));
            Frame::from_parts(image, 0, 0, Delay::from_numer_denom_ms(50, 1))
        })))
        .unwrap();
        drop(encoder);
        bytes
    }

    #[test]
    fn animated_picture_loads_one_frame_and_decodes_the_rest_on_play() {
        let loaded = picture(&gif(3)).unwrap();
        let one_frame = 4 * 4 * 4;
        assert_eq!(loaded.held_bytes(), one_frame);
        let (tx, rx) = std::sync::mpsc::channel();
        let again = tx.clone();
        assert!(loaded.rest(move || tx.send(()).unwrap()).is_none());
        rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        // Every clone shares the decode; the frames are handed over, not kept.
        let copy = loaded.clone();
        let rest = copy.rest(|| panic!("decoded again")).unwrap();
        assert_eq!(rest.len(), 2);
        assert_eq!(
            rest[1].0.pixels[0],
            eframe::egui::Color32::from_rgb(80, 30, 60)
        );
        assert_eq!(loaded.held_bytes(), one_frame);
        // Asked again (textures dropped), they are decoded again from the file.
        assert!(loaded.rest(move || again.send(()).unwrap()).is_none());
        rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(loaded.rest(|| panic!("decoded again")).unwrap().len(), 2);
    }

    #[test]
    fn many_animated_pictures_share_a_few_decoding_threads() {
        let file = gif(3);
        let pictures: Vec<_> = (0..300).map(|_| picture(&file).unwrap()).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut threads = std::collections::HashSet::new();
        let mut waiting: Vec<_> = pictures.iter().collect();
        // A full queue leaves a picture for a later repaint; every decode that ends is one.
        loop {
            waiting.retain(|shown| {
                let tx = tx.clone();
                let done = move || tx.send(std::thread::current().id()).unwrap();
                match shown.rest(done) {
                    Some(rest) => {
                        assert_eq!(rest.len(), 2);
                        false
                    }
                    None => true,
                }
            });
            if waiting.is_empty() {
                break;
            }
            threads.insert(rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap());
        }
        threads.extend(rx.try_iter());
        assert!(
            threads.len() <= crate::server::PICTURE_WORKERS,
            "{threads:?}"
        );
    }

    #[test]
    fn still_picture_has_no_frames_to_decode() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let still = picture(png.get_ref()).unwrap();
        assert_eq!(still.first.0.size, [4, 4]);
        assert!(
            still
                .rest(|| panic!("nothing to decode"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(still.held_bytes(), 4 * 4 * 4);
    }
}
