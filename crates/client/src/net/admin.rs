//! Server settings: members, roles, bans, the audit log, joining and invites.
//! Every refusal comes back to the settings window in plain words.

use reqwest::Method;
use rusttalk_core::{
    auth::{FileLimits, FileSettings, ServerUpdates, decode, encode},
    client::KeyClient,
    keys::{Metadata, random_key},
};
use serde_json::{Value, json};
use url::Url;

use super::{Ctx, State, Update, directory, get, now};
use crate::server::{Id, role_display};
use eframe::egui::Color32;

pub enum Admin {
    /// What the window shows, as far as this person may see it.
    Load {
        joining: bool,
        audit: bool,
        bans: bool,
    },
    SystemMessages(crate::server::SystemMessages),
    Kick(Id),
    /// Mute (`true`) or unmute someone in every voice channel.
    VoiceMute(Id, bool),
    /// Disconnect someone from a voice channel, or (`Some`) move them to another.
    VoiceMove {
        person: Id,
        from: Id,
        to: Option<Id>,
    },
    /// Ban a person, and (`true`) the networks they use.
    Ban(Id, bool),
    Unban(Id),
    /// Replace a member's roles.
    SetRoles(Id, Vec<String>),
    CreateRole {
        name: String,
        rank: i64,
    },
    /// Any role's colour and grouping, and its new name if `Some`.
    StyleRole {
        id: String,
        name: Option<String>,
        color: Option<Color32>,
        separate: bool,
    },
    /// A custom role's permissions, and its new rank if `Some`.
    EditRole {
        id: String,
        permissions: Vec<String>,
        rank: Option<i64>,
    },
    DeleteRole(String),
    DefaultRole(String),
    /// Channel, role and its read/write/speak/video flags there.
    ChannelFlags(Id, String, u8),
    /// The whole audit log, or only one action.
    Audit(Option<String>),
    /// Open joining on or off, with a member limit.
    Joining(bool, u32),
    LoadFiles,
    SaveFiles(FileLimits),
    /// Active invites, as far as this person may see them.
    Invites,
    RevokeInvite(String),
    /// Replace the server icon (PNG, JPEG or GIF bytes).
    SetIcon(Vec<u8>),
    /// Events on or off.
    Events(bool),
    /// Add a server emoji or (`sticker`) a sticker from PNG or GIF bytes.
    AddEmoji {
        name: String,
        sticker: bool,
        bytes: Vec<u8>,
        /// The pack to put it in; the server's first when `None`.
        pack: Option<Id>,
    },
    RenameEmoji(Id, String),
    DeleteEmoji(Id),
    /// A new emoji and sticker pack with this name.
    AddPack(String),
    RenamePack(Id, String),
    /// Delete a pack and everything in it.
    DeletePack(Id),
    /// Every pack, in the new order.
    OrderPacks(Vec<Id>),
    /// Move an emoji or sticker to another pack.
    MoveEmoji(Id, Id),
    /// Make a new pack and add these pictures to it, each fitted like a normal upload.
    ImportPack {
        name: String,
        items: Vec<ImportItem>,
    },
    /// The owner's public listing setting.
    LoadListing,
    LoadUpdates,
    /// List the server publicly with these details, or stop (`None`).
    SaveListing(Option<PublicListing>),
}

/// One picture of an imported pack, as read from the file.
pub struct ImportItem {
    pub name: String,
    pub sticker: bool,
    pub bytes: Vec<u8>,
}

/// What the owner publishes in a server directory. Unlike everything else, in plain text.
#[derive(Clone, Default)]
pub struct PublicListing {
    pub name: String,
    pub description: String,
    /// This server's HTTPS address, as people should reach it.
    pub address: String,
    pub index_url: String,
    /// The directory's certificate pin as base64url, or empty to use normal trust.
    pub index_fingerprint: String,
}

pub struct Ban {
    pub person: Id,
    pub at: i64,
    /// Their networks are blocked too.
    pub network: bool,
}

pub struct Invite {
    pub id: String,
    /// Unknown for invites made before the server recorded it.
    pub creator: Option<Id>,
    pub uses: i64,
    pub expires_at: i64,
}

pub struct AuditEntry {
    pub target_id: Option<Id>,
    pub actor: Id,
    pub action: String,
    pub target: Option<Id>,
    pub at: i64,
}

pub enum AdminUpdate {
    Bans(Vec<Ban>),
    /// Newest first.
    Audit(Vec<AuditEntry>),
    /// Soonest to expire first.
    Invites(Vec<Invite>),
    Joining(bool, u32),
    Files(FileSettings),
    /// Whether the server is listed, and the saved details.
    Listing(bool, Option<PublicListing>),
    ServerUpdates(ServerUpdates),
    /// An import is under way: pictures added so far, and in all.
    Progress(usize, usize),
    /// A change went through; the text says what happened.
    Done(&'static str),
    Failed(String),
}

pub(super) async fn run(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    request: Admin,
) {
    let send = |update| ctx.send(Update::Admin(url.clone(), update));
    let bans_changed = matches!(request, Admin::Ban(..) | Admin::Unban(_));
    let invites_changed = matches!(request, Admin::RevokeInvite(_));
    let result = match request {
        Admin::Load {
            joining,
            audit,
            bans,
        } => {
            match load(client, joining, audit, bans).await {
                Ok(updates) => updates.into_iter().for_each(send),
                Err(text) => send(AdminUpdate::Failed(text)),
            }
            return;
        }
        Admin::LoadListing => {
            match get(client, "/v1/settings/public-listing").await {
                Ok(value) => send(listing(&value)),
                Err(error) => send(AdminUpdate::Failed(failed(&error))),
            }
            return;
        }
        Admin::LoadFiles => {
            match file_settings(client, Method::GET, None).await {
                Ok(settings) => send(AdminUpdate::Files(settings)),
                Err(text) => send(AdminUpdate::Failed(text)),
            }
            return;
        }
        Admin::SaveFiles(limits) => file_settings(client, Method::PUT, Some(json!(limits)))
            .await
            .map(|settings| {
                send(AdminUpdate::Files(settings));
                t!("admin.done_file_limits")
            }),
        Admin::LoadUpdates => {
            match get(client, "/v1/settings/updates").await {
                Ok(value) => match serde_json::from_value(value) {
                    Ok(status) => send(AdminUpdate::ServerUpdates(status)),
                    Err(_) => send(AdminUpdate::Failed(t!("admin.update_unavailable").into())),
                },
                Err(error)
                    if error
                        .downcast_ref::<reqwest::Error>()
                        .and_then(reqwest::Error::status)
                        == Some(reqwest::StatusCode::NOT_FOUND) => {}
                Err(error) => send(AdminUpdate::Failed(failed(&error))),
            }
            return;
        }
        Admin::Invites => {
            match invites(client).await {
                Ok(list) => send(AdminUpdate::Invites(list)),
                Err(text) => send(AdminUpdate::Failed(text)),
            }
            return;
        }
        Admin::Audit(action) => {
            match audit_log(client, action.as_deref()).await {
                Ok(entries) => send(AdminUpdate::Audit(entries)),
                Err(text) => send(AdminUpdate::Failed(text)),
            }
            return;
        }
        Admin::SystemMessages(choice) => save_system_messages(client, choice)
            .await
            .map(|()| t!("admin.system_saved")),
        Admin::Kick(person) => call(
            client,
            Method::DELETE,
            &format!("/v1/members/{}", encode(&person)),
            None,
        )
        .await
        .map(|()| t!("admin.done_kicked")),
        Admin::VoiceMute(person, on) => {
            let method = if on { Method::PUT } else { Method::DELETE };
            let path = format!("/v1/members/{}/voice-mute", encode(&person));
            call(client, method, &path, None)
                .await
                .map(|()| t!("admin.done_voice"))
        }
        Admin::VoiceMove { person, from, to } => {
            let path = format!(
                "/v1/voice/{}/participants/{}",
                encode(&from),
                encode(&person)
            );
            match to {
                None => call(client, Method::DELETE, &path, None).await,
                Some(to) => {
                    let body = json!({"to": encode(&to)});
                    call(client, Method::POST, &format!("{path}/move"), Some(body)).await
                }
            }
            .map(|()| t!("admin.done_voice"))
        }
        Admin::Ban(person, network) => {
            let path = format!("/v1/bans/{}", encode(&person));
            call(client, Method::PUT, &path, Some(json!({"ban_ip": network})))
                .await
                .map(|()| t!("admin.done_banned"))
                .map_err(|text| match text == problem("role_hierarchy") && network {
                    // Also refused when you or someone ranked as high as you uses that network.
                    true => t!("admin.ban_network_conflict").into(),
                    false => text,
                })
        }
        Admin::Unban(person) => call(
            client,
            Method::DELETE,
            &format!("/v1/bans/{}", encode(&person)),
            None,
        )
        .await
        .map(|()| t!("admin.done_unbanned")),
        Admin::SetRoles(person, roles) => {
            let path = format!("/v1/members/{}/roles", encode(&person));
            call(client, Method::PUT, &path, Some(json!({"role_ids": roles})))
                .await
                .map(|()| t!("admin.done_roles"))
        }
        Admin::CreateRole { name, rank } => create_role(client, &name, rank).await,
        Admin::StyleRole {
            id,
            name,
            color,
            separate,
        } => style_role(client, &id, name.as_deref(), color, separate).await,
        Admin::EditRole {
            id,
            permissions,
            rank,
        } => {
            let mut body = json!({"permissions": permissions});
            if let Some(rank) = rank {
                body["rank"] = rank.into();
            }
            call(client, Method::PUT, &format!("/v1/roles/{id}"), Some(body))
                .await
                .map(|()| t!("admin.done_role_saved"))
        }
        Admin::DeleteRole(id) => call(client, Method::DELETE, &format!("/v1/roles/{id}"), None)
            .await
            .map(|()| t!("admin.done_role_deleted")),
        Admin::DefaultRole(id) => call(
            client,
            Method::PUT,
            "/v1/roles/default",
            Some(json!({"role_id": id})),
        )
        .await
        .map(|()| t!("admin.done_default_role")),
        Admin::ChannelFlags(channel, role, flags) => {
            let path = format!("/v1/channels/{}/permissions/{role}", encode(&channel));
            call(client, Method::PUT, &path, Some(json!({"flags": flags})))
                .await
                .map(|()| t!("admin.done_channel_permissions"))
        }
        Admin::Joining(open, cap) => {
            let body = json!({"open_joining": open, "member_cap": cap});
            call(client, Method::PUT, "/v1/settings/open-joining", Some(body))
                .await
                .map(|()| t!("admin.done_joining"))
        }
        Admin::SaveListing(listing) => {
            let body = match listing {
                None => json!({"enabled": false}),
                Some(listing) => json!({
                    "enabled": true,
                    "listing": {
                        "name": listing.name,
                        "description": listing.description,
                        "address": listing.address,
                        "index_url": listing.index_url,
                        "index_tls_fingerprint":
                            Some(listing.index_fingerprint).filter(|pin| !pin.is_empty()),
                    }
                }),
            };
            call(
                client,
                Method::PUT,
                "/v1/settings/public-listing",
                Some(body),
            )
            .await
            .map(|()| t!("admin.done_listing"))
        }
        Admin::Events(on) => {
            let body = json!({ "enabled": on });
            call(client, Method::PUT, "/v1/settings/calendar", Some(body))
                .await
                .map(|()| t!("admin.done_events"))
        }
        Admin::SetIcon(bytes) => match client.set_server_icon(&bytes).await {
            Ok(()) => Ok(t!("admin.done_icon")),
            Err(error) => Err(failed(&error)),
        },
        Admin::AddEmoji {
            name,
            sticker,
            bytes,
            pack,
        } => add_emoji(client, &name, sticker, &bytes, pack.as_ref()).await,
        Admin::AddPack(name) => add_pack(client, &name)
            .await
            .map(|_| t!("admin.done_pack_added")),
        Admin::RenamePack(id, name) => {
            match emoji_ct(client, &id, Metadata::EmojiPackName, name.as_bytes()) {
                Ok(name_ct) => {
                    let path = format!("/v1/emoji/packs/{}/name", encode(&id));
                    call(
                        client,
                        Method::PUT,
                        &path,
                        Some(json!({"name_ct": name_ct})),
                    )
                    .await
                    .map(|()| t!("admin.done_renamed"))
                }
                Err(text) => Err(text),
            }
        }
        Admin::DeletePack(id) => call(
            client,
            Method::DELETE,
            &format!("/v1/emoji/packs/{}", encode(&id)),
            None,
        )
        .await
        .map(|()| t!("admin.done_deleted")),
        Admin::OrderPacks(ids) => {
            let ids: Vec<String> = ids.iter().map(|id| encode(id)).collect();
            call(
                client,
                Method::PUT,
                "/v1/emoji/packs/order",
                Some(json!({ "ids": ids })),
            )
            .await
            .map(|()| t!("admin.done_pack_order"))
        }
        Admin::MoveEmoji(id, pack) => call(
            client,
            Method::PUT,
            &format!("/v1/emoji/{}/pack", encode(&id)),
            Some(json!({"pack_id": encode(&pack)})),
        )
        .await
        .map(|()| t!("admin.done_moved")),
        Admin::ImportPack { name, items } => {
            import_pack(client, &name, items, |done, all| {
                send(AdminUpdate::Progress(done, all))
            })
            .await
        }
        Admin::RenameEmoji(id, name) => {
            match emoji_ct(client, &id, Metadata::EmojiName, name.as_bytes()) {
                Ok(name_ct) => {
                    let path = format!("/v1/emoji/{}/name", encode(&id));
                    call(
                        client,
                        Method::PUT,
                        &path,
                        Some(json!({"name_ct": name_ct})),
                    )
                    .await
                    .map(|()| t!("admin.done_renamed"))
                }
                Err(text) => Err(text),
            }
        }
        Admin::DeleteEmoji(id) => call(
            client,
            Method::DELETE,
            &format!("/v1/emoji/{}", encode(&id)),
            None,
        )
        .await
        .map(|()| t!("admin.done_deleted")),
        Admin::RevokeInvite(id) => call(client, Method::DELETE, &format!("/v1/invites/{id}"), None)
            .await
            .map(|()| t!("admin.done_revoked")),
    };
    match result {
        Ok(done) => {
            send(AdminUpdate::Done(done));
            if let Err(error) = directory(ctx, url, client, state).await {
                tracing::warn!(error = %format_args!("{error:#}"), "reloading after a settings change failed");
            }
            if bans_changed {
                match bans(client).await {
                    Ok(list) => send(AdminUpdate::Bans(list)),
                    Err(text) => send(AdminUpdate::Failed(text)),
                }
            }
            if invites_changed {
                match invites(client).await {
                    Ok(list) => send(AdminUpdate::Invites(list)),
                    Err(text) => send(AdminUpdate::Failed(text)),
                }
            }
        }
        Err(text) => send(AdminUpdate::Failed(text)),
    }
}

fn listing(value: &Value) -> AdminUpdate {
    let text = |key: &str| {
        value["listing"][key]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    let saved = value["listing"].is_object().then(|| PublicListing {
        name: text("name"),
        description: text("description"),
        address: text("address"),
        index_url: text("index_url"),
        index_fingerprint: text("index_tls_fingerprint"),
    });
    AdminUpdate::Listing(value["enabled"].as_bool().unwrap_or_default(), saved)
}

async fn load(
    client: &KeyClient<'_>,
    joining: bool,
    audit: bool,
    banned: bool,
) -> Result<Vec<AdminUpdate>, String> {
    let mut out = Vec::new();
    if joining {
        let settings = get(client, "/v1/settings/open-joining")
            .await
            .map_err(|error| failed(&error))?;
        out.push(AdminUpdate::Joining(
            settings["open_joining"].as_bool().unwrap_or_default(),
            settings["member_cap"].as_u64().unwrap_or(100) as u32,
        ));
    }
    if audit {
        out.push(AdminUpdate::Audit(audit_log(client, None).await?));
    }
    if banned {
        out.push(AdminUpdate::Bans(bans(client).await?));
    }
    Ok(out)
}

async fn bans(client: &KeyClient<'_>) -> Result<Vec<Ban>, String> {
    let rows = get(client, "/v1/bans")
        .await
        .map_err(|error| failed(&error))?;
    Ok(rows
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some(Ban {
                person: decode(row["person_id"].as_str()?).ok()?,
                at: row["banned_at"].as_i64()?,
                network: row["ban_ip"].as_bool().unwrap_or_default(),
            })
        })
        .collect())
}

async fn invites(client: &KeyClient<'_>) -> Result<Vec<Invite>, String> {
    let rows = get(client, "/v1/invites")
        .await
        .map_err(|error| failed(&error))?;
    Ok(rows
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some(Invite {
                id: row["id"].as_str()?.to_owned(),
                creator: row["created_by"].as_str().and_then(|id| decode(id).ok()),
                uses: row["remaining_uses"].as_i64()?,
                expires_at: row["expires_at"].as_i64()?,
            })
        })
        .collect())
}

/// The API pages oldest first; read it all and return it newest first.
// shortcut: loads the whole log each time, fine for small communities; ask the API for a
// newest-first cursor if logs grow to many thousands of entries.
async fn audit_log(
    client: &KeyClient<'_>,
    action: Option<&str>,
) -> Result<Vec<AuditEntry>, String> {
    let mut entries = Vec::new();
    let mut after = 0;
    loop {
        let mut path = format!("/v1/audit?after={after}&limit=100");
        if let Some(action) = action {
            path.push_str(&format!("&action={action}"));
        }
        let page = get(client, &path).await.map_err(|error| failed(&error))?;
        let rows = page.as_array().cloned().unwrap_or_default();
        // A row without a growing ID would page forever; stop after this page.
        let mut stuck = false;
        for row in &rows {
            match row["id"].as_i64() {
                Some(id) if id > after => after = id,
                _ => stuck = true,
            }
            let person = |key: &str| row[key].as_str().and_then(|id| decode(id).ok());
            let Some(actor) = person("actor_person_id") else {
                continue;
            };
            entries.push(AuditEntry {
                actor,
                action: row["action"].as_str().unwrap_or_default().to_owned(),
                target: person("target_person_id"),
                target_id: person("target_id"),
                at: row["at"].as_i64().unwrap_or_else(now),
            });
        }
        if stuck || rows.len() < 100 {
            break;
        }
    }
    entries.reverse();
    Ok(entries)
}

async fn create_role(
    client: &KeyClient<'_>,
    name: &str,
    rank: i64,
) -> Result<&'static str, String> {
    let id = random_key().map_err(|error| {
        tracing::error!(%error, "making a role ID failed");
        t!("admin.failed").to_owned()
    })?;
    // The API takes 16-byte role IDs.
    let id = encode(&id[..16]);
    let body = json!({
        "id": id,
        "rank": rank,
        "permissions": [],
        "name_ct": name_ct(client, &id, name)?,
    });
    call(client, Method::POST, "/v1/roles", Some(body))
        .await
        .map(|()| t!("admin.done_role_created"))
}

async fn style_role(
    client: &KeyClient<'_>,
    id: &str,
    name: Option<&str>,
    color: Option<Color32>,
    separate: bool,
) -> Result<&'static str, String> {
    let display = client
        .encrypt_metadata(
            Metadata::RoleDisplay,
            id.as_bytes(),
            &role_display(color, separate),
        )
        .map_err(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "encrypting a role colour failed");
            t!("admin.key_missing").to_owned()
        })?;
    let mut body = json!({"display_ct": display});
    if let Some(name) = name {
        body["name_ct"] = name_ct(client, id, name)?.into();
    }
    call(
        client,
        Method::PUT,
        &format!("/v1/roles/{id}/name"),
        Some(body),
    )
    .await
    .map(|()| t!("admin.done_role_saved"))
}

/// Make a pack; its ID.
async fn add_pack(client: &KeyClient<'_>, name: &str) -> Result<Id, String> {
    let id = random_key().map_err(|error| {
        tracing::error!(%error, "making a pack ID failed");
        t!("admin.failed").to_owned()
    })?;
    let body = json!({
        "id": encode(id.as_ref()),
        "name_ct": emoji_ct(client, &id[..], Metadata::EmojiPackName, name.as_bytes())?,
    });
    call(client, Method::POST, "/v1/emoji/packs", Some(body)).await?;
    Ok(*id)
}

/// Make a pack and add the pictures to it one by one, fitting each like a normal upload.
/// A busy server (429) is waited for; a picture that fails is skipped and counted.
// shortcut: runs inside this server's loop like other uploads, so its events wait.
async fn import_pack(
    client: &KeyClient<'_>,
    name: &str,
    items: Vec<ImportItem>,
    progress: impl Fn(usize, usize),
) -> Result<&'static str, String> {
    let pack = add_pack(client, name).await?;
    let total = items.len();
    let mut failed = 0;
    progress(0, total);
    for (done, item) in items.into_iter().enumerate() {
        let sticker = item.sticker;
        let fitted =
            tokio::task::spawn_blocking(move || crate::crop::fit(&item.bytes, sticker)).await;
        let added = match fitted {
            Ok(Ok(bytes)) => add_emoji_retrying(client, &item.name, sticker, &bytes, &pack).await,
            _ => Err(t!("admin.failed").to_owned()),
        };
        if added.is_err() {
            failed += 1;
        }
        progress(done + 1, total);
        // Easy on the server: a pause between pictures, even when it never says it is busy.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    match failed {
        0 => Ok(t!("admin.done_import")),
        count if count == total => Err(t!("admin.import_all_failed").to_owned()),
        count => Err(t!("admin.import_some_failed", count = count)),
    }
}

/// `add_emoji`, waiting and trying again while the server says it is busy.
async fn add_emoji_retrying(
    client: &KeyClient<'_>,
    name: &str,
    sticker: bool,
    bytes: &[u8],
    pack: &Id,
) -> Result<&'static str, String> {
    for attempt in 0..4 {
        match add_emoji(client, name, sticker, bytes, Some(pack)).await {
            Err(text) if text == t!("admin.busy") && attempt < 3 => {
                tokio::time::sleep(std::time::Duration::from_secs(2 << attempt)).await;
            }
            done => return done,
        }
    }
    Err(t!("admin.busy").to_owned())
}

async fn add_emoji(
    client: &KeyClient<'_>,
    name: &str,
    sticker: bool,
    bytes: &[u8],
    pack: Option<&Id>,
) -> Result<&'static str, String> {
    let id = random_key().map_err(|error| {
        tracing::error!(%error, "making an emoji ID failed");
        t!("admin.failed").to_owned()
    })?;
    let mut body = json!({
        "id": encode(id.as_ref()),
        "kind": if sticker { "sticker" } else { "emoji" },
        "name_ct": emoji_ct(client, &id[..], Metadata::EmojiName, name.as_bytes())?,
        "image_ct": emoji_ct(client, &id[..], Metadata::EmojiImage, bytes)?,
    });
    if let Some(pack) = pack {
        body["pack_id"] = encode(pack).into();
    }
    call(client, Method::POST, "/v1/emoji", Some(body))
        .await
        .map(|()| {
            if sticker {
                t!("admin.done_sticker")
            } else {
                t!("admin.done_emoji")
            }
        })
}

pub(super) async fn save_system_messages(
    client: &KeyClient<'_>,
    choice: crate::server::SystemMessages,
) -> Result<(), String> {
    use crate::server::SystemMessages;
    let clear = match &choice {
        SystemMessages::First => None,
        SystemMessages::None => Some([].as_slice()),
        SystemMessages::Channel(id) => Some(id.as_slice()),
    };
    let ct = clear
        .map(|bytes| client.encrypt_metadata(Metadata::SystemMessages, &client.space().id, bytes))
        .transpose()
        .map_err(|e| failed(&e))?;
    call(
        client,
        Method::PUT,
        "/v1/space",
        Some(json!({"system_messages_ct": ct})),
    )
    .await
}

fn emoji_ct(
    client: &KeyClient<'_>,
    id: &[u8],
    kind: Metadata,
    clear: &[u8],
) -> Result<String, String> {
    client.encrypt_metadata(kind, id, clear).map_err(|error| {
        tracing::warn!(error = %format_args!("{error:#}"), "encrypting an emoji failed");
        t!("admin.key_missing").to_owned()
    })
}

fn name_ct(client: &KeyClient<'_>, id: &str, name: &str) -> Result<String, String> {
    client
        .encrypt_metadata(Metadata::RoleName, id.as_bytes(), name.as_bytes())
        .map_err(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "encrypting a role name failed");
            t!("admin.key_missing").to_owned()
        })
}

fn failed(error: &anyhow::Error) -> String {
    tracing::warn!(error = %format_args!("{error:#}"), "server settings request failed");
    match error
        .downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
    {
        Some(reqwest::StatusCode::FORBIDDEN) => t!("admin.no_permission").into(),
        _ => t!("admin.failed").into(),
    }
}

/// Send a request and turn a refusal into plain words.
async fn call(
    client: &KeyClient<'_>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<(), String> {
    request(client, method, path, body).await.map(|_| ())
}

async fn file_settings(
    client: &KeyClient<'_>,
    method: Method,
    body: Option<Value>,
) -> Result<FileSettings, String> {
    request(client, method, "/v1/settings/files", body)
        .await?
        .json()
        .await
        .map_err(|error| failed(&error.into()))
}

async fn request(
    client: &KeyClient<'_>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<reqwest::Response, String> {
    let mut request = client.request(method, path);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let reply = request.send().await.map_err(|error| {
        tracing::warn!(error = %error.without_url(), "server settings request failed");
        t!("admin.offline").to_owned()
    })?;
    let status = reply.status();
    if status.is_success() {
        return Ok(reply);
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(t!("admin.busy").into());
    }
    let code = reply
        .json::<Value>()
        .await
        .ok()
        .and_then(|body| body["error"].as_str().map(str::to_owned))
        .unwrap_or_default();
    tracing::warn!(%status, code, "server settings change refused");
    Err(problem(&code).into())
}

fn problem(code: &str) -> &'static str {
    match code {
        "role_hierarchy" => t!("admin.role_hierarchy"),
        "cannot_grant_permission" => t!("admin.cannot_grant"),
        "permission_denied" => t!("admin.no_permission"),
        "owner_required" => t!("admin.owner_required"),
        "built_in_role" => t!("admin.built_in_role"),
        "role_in_use" => t!("admin.role_in_use"),
        "role_limit" => t!("admin.role_limit"),
        "already_banned" => t!("admin.already_banned"),
        "no_active_ip" => {
            t!("admin.no_active_ip")
        }
        "member_not_found" => t!("admin.member_not_found"),
        "not_in_voice" => t!("admin.not_in_voice"),
        "voice_move_denied" => t!("admin.voice_move_denied"),
        "ban_not_found" => t!("admin.ban_not_found"),
        "invite_not_found" => t!("admin.invite_not_found"),
        "invalid_member_cap" => t!("admin.invalid_member_cap"),
        "invalid_file_limits" => t!("admin.invalid_file_limits"),
        "invalid_listing" | "invalid_listing_settings" => {
            t!("admin.invalid_listing")
        }
        "invalid_index_url" => t!("admin.directory_https"),
        "invalid_index_tls_fingerprint" => t!("home.bad_fingerprint"),
        "invalid_channel_flags" => t!("admin.invalid_channel_flags"),
        "pack_limit" => t!("admin.pack_limit"),
        "last_pack" => t!("admin.last_pack"),
        "pack_not_found" => t!("admin.pack_not_found"),
        "emoji_limit" => t!("admin.emoji_limit"),
        "sticker_limit" => t!("admin.sticker_limit"),
        "emoji_too_large" | "body_too_large" => t!("admin.emoji_too_large"),
        "emoji_not_found" => t!("admin.emoji_not_found"),
        "invalid_role_count" => t!("admin.invalid_role_count"),
        "space_key_unavailable" | "metadata_epoch_changed" => {
            t!("admin.key_changing")
        }
        _ => t!("admin.failed"),
    }
}
