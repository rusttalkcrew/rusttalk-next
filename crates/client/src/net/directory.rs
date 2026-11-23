//! The server's names, channels, members and permissions, decrypted for the UI.

use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result};
use eframe::egui::ColorImage;
use futures_util::future::join_all;
use reqwest::{Method, StatusCode};
use rusttalk_core::{
    auth::{decode, encode},
    client::{Catalog, Channel as WireChannel, KeyClient, decode_profile_image},
    identity::fingerprint,
    keys::Metadata,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use rusttalk_core::client::voice::DEFAULT_AUDIO_KBPS;

use super::{Ctx, State, Update, leave_voice, now};
use crate::server::{
    Body, Category, Channel, CustomEmoji, Device, Directory, Dm, EmojiPack, Id, Member, Msg, Perms,
    Picture, Role, SystemMessages, member_name, parse_role_display,
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

/// Fetch and decrypt names, channels, members and permissions, and report member changes.
pub(super) async fn directory(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
) -> Result<()> {
    let catalog = client.catalog().await?;
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
    let joined_kbps = state.voice.as_ref().and_then(|live| {
        let channel = catalog.channels.iter().find(|c| c.id == live.channel)?;
        channel.audio_kbps
    });
    let members = get(client, "/v1/members").await?;
    let me = get(client, "/v1/me").await?;
    let roles = get(client, "/v1/roles").await?;
    let space = get(client, "/v1/space").await?;
    let emoji = emoji_list(client).await?;
    let pictures = pictures(client, &members, &space, &emoji, &state.pictures).await?;
    let mut dir = build(
        client,
        catalog,
        &members,
        &me,
        &roles,
        &space,
        &emoji,
        &|id: &Id, hash: Option<&str>| pictures.get(&picture_key(id, hash?)).cloned(),
    )
    .await?;
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
    dir.calendar = super::calendar::load(client).await?;
    super::save_pins(ctx, url, client, state);
    state.pictures = pictures;
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
    super::recent::Listing::save(state, &dir, &members, &space);
    state.last = Some(dir.clone());
    state.named = client.has_space_key();
    ctx.send(Update::Directory(url.clone(), Box::new(dir)));
    if let Some(channel) = deleted {
        leave_voice(ctx, url, client, state);
        ctx.send(Update::Call(url.clone(), channel, None));
    }
    Ok(())
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
async fn build(
    client: &KeyClient<'_>,
    mut catalog: Catalog,
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

    // The readable catalog omits categories with no readable channels, including empty ones.
    if perms.manage_channels {
        catalog.categories = managed_categories(client).await?;
    }
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
            audio_kbps: channel.audio_kbps.unwrap_or(DEFAULT_AUDIO_KBPS),
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
            .find(|c| !c.page && !c.voice)
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
        members: list,
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

/// The pictures in this directory load. Only pictures whose hash is new are fetched and
/// decoded, the rest come from the last load. Pictures that fail are tried again next load.
async fn pictures(
    client: &KeyClient<'_>,
    members: &Value,
    space: &Value,
    emoji: &Value,
    last: &HashMap<[u8; 32], Picture>,
) -> Result<HashMap<[u8; 32], Picture>> {
    let mut wanted = Vec::new();
    for row in members.as_array().into_iter().flatten() {
        if let (Some(id), Some(hash)) = (row["person_id"].as_str(), row["avatar_hash"].as_str()) {
            let path = format!("/v1/members/{id}/avatar");
            wanted.push((Metadata::Avatar, decode::<32>(id)?, hash, path, "avatar_ct"));
        }
    }
    if let Some(hash) = space["icon_hash"].as_str() {
        let path = "/v1/space/icon".to_owned();
        wanted.push((
            Metadata::ServerIcon,
            client.space().id,
            hash,
            path,
            "icon_ct",
        ));
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
    for (kind, id, hash, path, field) in wanted {
        let key = picture_key(&id, hash);
        match last.get(&key) {
            Some(known) => {
                pictures.insert(key, known.clone());
            }
            None => missing.push((kind, id, key, path, field)),
        }
    }
    let mut fetched = Vec::new();
    for batch in missing.chunks(8) {
        let replies = join_all(batch.iter().map(|(_, _, _, path, _)| get(client, path))).await;
        for ((kind, id, key, _, field), reply) in batch.iter().zip(replies) {
            match reply {
                Ok(reply) => {
                    if let Some(ct) = reply[*field].as_str() {
                        fetched.push((*kind, *id, *key, ct.to_owned()));
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %format_args!("{error:#}"), "picture could not be fetched");
                }
            }
        }
    }
    // Decrypted and decoded here, off the UI.
    tokio::task::block_in_place(|| {
        for (kind, id, key, ct) in fetched {
            let bytes = match client.decrypt_metadata(kind, &id, &ct) {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::warn!(error = %format_args!("{error:#}"), "picture could not be decrypted");
                    continue;
                }
            };
            if let Some(picture) = picture(&bytes) {
                pictures.insert(key, picture);
            }
        }
    });
    Ok(pictures)
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

/// Decode a profile picture, server icon, emoji or sticker.
pub(super) fn picture(bytes: &[u8]) -> Option<Picture> {
    let key: [u8; 32] = Sha256::digest(bytes).into();
    let frames = match decode_profile_image(bytes) {
        Ok(frames) => frames,
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "picture could not be decoded");
            return None;
        }
    };
    let frames: Vec<_> = frames.into_iter().map(frame_image).collect();
    Some(Picture {
        key,
        frames: frames.into(),
        bytes: Arc::new(bytes.to_vec()),
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
        let dir = |members| Directory {
            members,
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
}
