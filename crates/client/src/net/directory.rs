//! The server's names, channels, members and permissions, decrypted for the UI.

use std::collections::HashMap;

use anyhow::{Context, Result};
use eframe::egui::ColorImage;
use futures_util::future::join_all;
use reqwest::Method;
use rusttalk_core::{
    auth::{decode, encode},
    client::{Catalog, KeyClient, decode_profile_image},
    identity::fingerprint,
    keys::Metadata,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use super::{Ctx, State, Update, leave_voice, now};
use crate::server::{
    Body, Category, Channel, Device, Directory, Dm, Id, Member, Msg, Perms, Picture, Role,
    member_name,
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

/// Save a channel order. Everything this person can't see keeps its place.
pub(super) async fn save_order(client: &KeyClient<'_>, shown: &[(Id, Vec<Id>)]) -> Result<()> {
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
    client
        .request(Method::PUT, "/v1/channels/order")
        .json(&serde_json::json!({ "categories": body }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Fetch and decrypt names, channels, members and permissions, and report member changes.
pub(super) async fn directory(
    ctx: &Ctx,
    url: &Url,
    client: &KeyClient<'_>,
    state: &mut State,
) -> Result<()> {
    let catalog = client.catalog().await?;
    let members = get(client, "/v1/members").await?;
    let me = get(client, "/v1/me").await?;
    let roles = get(client, "/v1/roles").await?;
    let space = get(client, "/v1/space").await?;
    let pictures = pictures(client, &members, &space, &state.pictures).await?;
    let dir = build(
        client,
        &catalog,
        &members,
        &me,
        &roles,
        &space,
        &|id: &Id, hash: Option<&str>| pictures.get(&picture_key(id, hash?)).cloned(),
    )?;
    state.pictures = pictures;
    if let Some(before) = &state.last {
        for (person, action, detail) in changes(before, &dir) {
            let name = member_name(&dir, &person);
            let name = if dir.members.iter().any(|m| m.person == person) {
                name
            } else {
                member_name(before, &person)
            };
            state.notices += 1;
            let mut id = [0xff; 32];
            id[..8].copy_from_slice(&state.notices.to_be_bytes());
            let msg = Msg {
                id,
                person,
                at: now(),
                reply_to: None,
                edit_of: None,
                body: Body::Notice {
                    name,
                    action,
                    detail,
                },
                files: Vec::new(),
                live: None,
            };
            ctx.send(Update::Notice(url.clone(), person, msg));
        }
    }
    let joined = state.voice.as_ref().map(|live| live.channel);
    let deleted = joined.filter(|joined| {
        !dir.categories
            .iter()
            .flat_map(|category| &category.channels)
            .any(|channel| channel.id == *joined)
    });
    state.last = Some(dir.clone());
    state.named = client.has_space_key();
    ctx.send(Update::Directory(url.clone(), Box::new(dir)));
    if let Some(channel) = deleted {
        leave_voice(ctx, url, client, state);
        ctx.send(Update::Call(url.clone(), channel, None));
    }
    Ok(())
}

fn build(
    client: &KeyClient<'_>,
    catalog: &Catalog,
    members: &Value,
    me: &Value,
    roles: &Value,
    space: &Value,
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
                    _ => {}
                }
            }
        }
        let kind = role["kind"].as_str().unwrap_or_default();
        let builtin = match kind {
            "owner" => "Owner",
            "admin" => "Admin",
            "moderator" => "Moderator",
            "member" => "Member",
            _ => "New role",
        };
        let name = text(Metadata::RoleName, id.as_bytes(), role["name_ct"].as_str());
        role_list.push(Role {
            name: name.unwrap_or_else(|| builtin.into()),
            id,
            rank,
            permissions,
            custom: kind == "custom",
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
            .unwrap_or_else(|| "Channels".into()),
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
            .unwrap_or_else(|| "channel".into()),
            voice: channel.kind == "voice",
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
    for row in members.as_array().into_iter().flatten() {
        let person: Id = decode(row["person_id"].as_str().context("member without ID")?)?;
        let devices = client
            .devices()
            .filter(|device| device.person_id() == &person)
            .map(|device| Device {
                name: device.name().to_owned(),
                fingerprint: fingerprint(device.signing_key().as_bytes()),
            })
            .collect();
        list.push(Member {
            person,
            name: text(
                Metadata::DisplayName,
                &person,
                row["display_name_ct"].as_str(),
            ),
            devices,
            roles: strings(&row["role_ids"]),
            avatar: picture(&person, row["avatar_hash"].as_str()),
            presence: serde_json::from_value(row["presence"].clone()).unwrap_or_default(),
        });
    }
    list.sort_by_cached_key(|member| {
        (
            member.name.is_none(),
            member.name.as_ref().map(|n| n.to_lowercase()),
        )
    });
    let server_id = client.space().id;
    Ok(Directory {
        name: text(Metadata::ServerName, &server_id, space["name_ct"].as_str()),
        icon: picture(&server_id, space["icon_hash"].as_str()),
        status: serde_json::from_value(me["status"].clone()).ok(),
        me: *client.identity.certificate.person_id(),
        perms,
        categories,
        members: list,
        dms,
        roles: role_list,
        default_role: roles["default_role_id"].as_str().unwrap_or_default().into(),
    })
}

/// The cache key of a member's avatar or the server icon: its owner and ciphertext hash.
/// The owner is part of the key so one member cannot reuse another's picture.
fn picture_key(owner: &Id, hash: &str) -> [u8; 32] {
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

/// Decode a profile picture or server icon.
fn picture(bytes: &[u8]) -> Option<Picture> {
    let key: [u8; 32] = Sha256::digest(bytes).into();
    let frames = match decode_profile_image(bytes) {
        Ok(frames) => frames,
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "picture could not be decoded");
            return None;
        }
    };
    let frames: Vec<_> = frames
        .into_iter()
        .map(|frame| {
            let (numer, denom) = frame.delay().numer_denom_ms();
            // Browsers also slow down GIF frames shorter than 20 ms.
            let ms = (numer / denom.max(1)).max(20);
            let image = frame.into_buffer();
            let size = [image.width() as usize, image.height() as usize];
            (ColorImage::from_rgba_unmultiplied(size, image.as_raw()), ms)
        })
        .collect();
    Some(Picture {
        key,
        frames: frames.into(),
    })
}

/// Member and device changes between two directory loads: (person, action, details).
fn changes(before: &Directory, after: &Directory) -> Vec<(Id, &'static str, String)> {
    let mut out = Vec::new();
    for member in &after.members {
        let id = format!("ID {}", fingerprint(&member.person));
        let Some(old) = before
            .members
            .iter()
            .find(|old| old.person == member.person)
        else {
            out.push((member.person, "joined", id));
            continue;
        };
        // A new member's first device arrives just after the join; that is not news.
        if old.devices.is_empty() {
            continue;
        }
        for device in &member.devices {
            if old
                .devices
                .iter()
                .all(|d| d.fingerprint != device.fingerprint)
            {
                let detail = format!("{id}\nNew device {} ({})", device.fingerprint, device.name);
                out.push((member.person, "added a new device", detail));
            }
        }
        for device in &old.devices {
            if member
                .devices
                .iter()
                .all(|d| d.fingerprint != device.fingerprint)
            {
                let detail = format!(
                    "{id}\nRemoved device {} ({})",
                    device.fingerprint, device.name
                );
                out.push((member.person, "removed a device", detail));
            }
        }
    }
    for old in &before.members {
        if after
            .members
            .iter()
            .all(|member| member.person != old.person)
        {
            let id = format!("ID {}", fingerprint(&old.person));
            out.push((old.person, "left", id));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(person: u8, devices: &[&str]) -> Member {
        Member {
            person: [person; 32],
            name: Some(format!("P{person}")),
            devices: devices
                .iter()
                .map(|fp| Device {
                    name: "Desktop".into(),
                    fingerprint: (*fp).into(),
                })
                .collect(),
            roles: Vec::new(),
            avatar: None,
            presence: Default::default(),
        }
    }

    #[test]
    fn reports_joins_leaves_and_device_changes_but_not_a_first_device() {
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
        assert_eq!(
            lines,
            [(1, "added a new device"), (4, "joined"), (3, "left")]
        );
    }
}
