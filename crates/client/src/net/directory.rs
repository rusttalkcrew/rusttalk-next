//! The server's names, channels, members and permissions, decrypted for the UI.

use anyhow::{Context, Result};
use reqwest::Method;
use rusttalk_core::{
    auth::{decode, encode},
    client::{Catalog, KeyClient},
    identity::fingerprint,
    keys::Metadata,
};
use serde_json::Value;
use url::Url;

use super::{Ctx, State, Update, leave_voice, now};
use crate::server::{
    Body, Category, Channel, Device, Directory, Dm, Id, Member, Msg, Perms, member_name,
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
    let dir = build(client, &catalog, &members, &me, &roles, &space)?;
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
) -> Result<Directory> {
    let text = |kind: Metadata, id: &[u8], ct: Option<&str>| {
        let plain = client.decrypt_metadata(kind, id, ct?);
        if let Err(error) = &plain {
            tracing::warn!(error = %format_args!("{error:#}"), "name could not be decrypted");
        }
        Some(String::from_utf8_lossy(&plain.ok()?).into_owned())
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
        ..Perms::default()
    };
    for role in roles["roles"].as_array().into_iter().flatten() {
        if role["id"]
            .as_str()
            .is_some_and(|id| my_roles.iter().any(|mine| mine == id))
        {
            for permission in strings(&role["permissions"]) {
                match permission.as_str() {
                    "manage_channels" => perms.manage_channels = true,
                    "invite" => perms.invite = true,
                    "delete_messages" => perms.delete_messages = true,
                    _ => {}
                }
            }
        }
    }

    let mut categories: Vec<_> = catalog.categories.iter().collect();
    categories.sort_by_key(|category| (category.position, category.id));
    let mut categories: Vec<Category> = categories
        .into_iter()
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
    let mut channels: Vec<_> = catalog.channels.iter().collect();
    channels.sort_by_key(|channel| (channel.position, channel.id));
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
    // Text channels first, then voice, like other chat apps.
    loose.sort_by_key(|channel| channel.voice);
    for category in &mut categories {
        category.channels.sort_by_key(|channel| channel.voice);
    }
    // Stable, so voice-only categories follow the rest in server order.
    categories.sort_by_key(|c| !c.channels.is_empty() && c.channels.iter().all(|ch| ch.voice));
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
        me: *client.identity.certificate.person_id(),
        perms,
        categories,
        members: list,
        dms,
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
