//! Server settings: members, roles, bans, the audit log, joining and invites.
//! Every refusal comes back to the settings window in plain words.

use reqwest::Method;
use rusttalk_core::{
    auth::{
        FileLimits, FileSettings, MessageHistory, NewMemberRules, RateLimitPreset, RateLimits,
        ServerUpdates, decode, encode,
    },
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
        bans: bool,
    },
    SystemMessages(crate::server::SystemMessages),
    Kick(Id),
    /// Stop someone from writing and talking for `seconds`, with an optional reason.
    Timeout {
        person: Id,
        seconds: i64,
        reason: Option<String>,
    },
    EndTimeout(Id),
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
    /// A page of the audit log, or of only one action: the newest, or (`before`) older.
    Audit {
        action: Option<String>,
        before: Option<i64>,
    },
    /// Open joining on or off, with a member limit.
    Joining(bool, u32),
    LoadFiles,
    SaveFiles(FileLimits),
    LoadHistory,
    /// How many messages each channel keeps; 0 keeps them all.
    SaveHistory(i64),
    LoadRateLimits,
    SaveRateLimits(RateLimitPreset),
    LoadNewMembers,
    /// How long the new member rules last after joining, in seconds; 0 is off.
    SaveNewMembers(i64),
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
    /// Changes saved together, one after another in this order. The server reloads once
    /// after the last, and one reply says how it went.
    Batch(Vec<Admin>),
}

/// One picture of an imported pack, as read from the file.
pub struct ImportItem {
    pub name: String,
    pub sticker: bool,
    pub bytes: Vec<u8>,
}

/// What the owner publishes in a server directory. Unlike everything else, in plain text.
#[derive(Clone, Default, PartialEq)]
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
    /// Placed by someone the viewer outranks or themselves; older servers don't say.
    pub can_lift: bool,
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
    /// What changed (opaque IDs, flags, numbers); `Null` on older servers and most actions.
    pub details: Value,
}

pub enum AdminUpdate {
    Bans(Vec<Ban>),
    /// Newest first; `older` continues the list. `next` asks for the page after it.
    Audit {
        entries: Vec<AuditEntry>,
        older: bool,
        next: Option<i64>,
    },
    /// Only the Audit page shows it.
    AuditFailed(String),
    /// Soonest to expire first.
    Invites(Vec<Invite>),
    Joining(bool, u32),
    Files(FileSettings),
    /// `None` from a server too old to change it.
    History(Option<MessageHistory>),
    /// `None` from a server too old to change it.
    RateLimits(Option<RateLimits>),
    /// `None` from a server too old to have new member rules.
    NewMembers(Option<NewMemberRules>),
    /// Whether the server is listed, and the saved details.
    Listing(bool, Option<PublicListing>),
    ServerUpdates(ServerUpdates),
    /// An import is under way: pictures added so far, and in all.
    Progress(usize, usize),
    /// This emoji or sticker name was added; comes just before its `Done`.
    EmojiAdded(String),
    /// A change went through; the text says what happened.
    Done(&'static str),
    Failed(String),
    /// Some changes of a batch failed: their places in it, and what to show.
    PartlySaved {
        failed: Vec<usize>,
        text: String,
    },
}

pub(super) async fn run(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    request: Admin,
) {
    let send = |update| ctx.send(Update::Admin(url.clone(), update));
    if let Admin::Batch(requests) = request {
        let mut results = Vec::with_capacity(requests.len());
        for request in requests {
            results.push(change(ctx, url, client, request).await);
        }
        let (changed, update) = batch(results);
        // Once for the whole batch, also when part of it failed.
        if changed {
            reload(ctx, url, client, state).await;
        }
        send(update);
        return;
    }
    let bans_changed = matches!(request, Admin::Ban(..) | Admin::Unban(_));
    let invites_changed = matches!(request, Admin::RevokeInvite(_));
    let Some(result) = change(ctx, url, client, request).await else {
        return;
    };
    match result {
        Ok(done) => {
            send(AdminUpdate::Done(done));
            reload(ctx, url, client, state).await;
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

/// How a batch went, from each request's result in order: whether anything changed
/// (so the server reloads, once), and Done when all went through, otherwise which
/// failed and the first reason.
fn batch(results: Vec<Option<Result<&'static str, String>>>) -> (bool, AdminUpdate) {
    let total = results.len();
    let mut done = t!("admin.done_role_saved");
    let mut failed = Vec::new();
    let mut reason = None;
    for (index, result) in results.into_iter().enumerate() {
        match result {
            Some(Ok(text)) => done = text,
            Some(Err(text)) => {
                reason.get_or_insert(text);
                failed.push(index);
            }
            None => {}
        }
    }
    let changed = failed.len() < total;
    let update = match reason {
        None => AdminUpdate::Done(done),
        Some(reason) => AdminUpdate::PartlySaved {
            text: t!(
                "admin.batch_failed",
                failed = failed.len(),
                total = total,
                reason = reason
            ),
            failed,
        },
    };
    (changed, update)
}

async fn reload(ctx: &Ctx, url: &Url, client: &mut KeyClient<'_>, state: &mut State) {
    if let Err(error) = directory(ctx, url, client, state).await {
        tracing::warn!(error = %format_args!("{error:#}"), "reloading after a settings change failed");
    }
}

/// Send one request. A change answers with what to show; loading (`None`) answers itself.
async fn change(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    request: Admin,
) -> Option<Result<&'static str, String>> {
    let send = |update| ctx.send(Update::Admin(url.clone(), update));
    let result = match request {
        // Each on its own, so one failing doesn't hide the other.
        Admin::Load {
            joining,
            bans: banned,
        } => {
            if joining {
                send(
                    open_joining(client)
                        .await
                        .unwrap_or_else(AdminUpdate::Failed),
                );
            }
            if banned {
                send(
                    bans(client)
                        .await
                        .map_or_else(AdminUpdate::Failed, AdminUpdate::Bans),
                );
            }
            return None;
        }
        Admin::LoadListing => {
            match get(client, "/v1/settings/public-listing").await {
                Ok(value) => send(listing(&value)),
                Err(error) => send(AdminUpdate::Failed(failed(&error))),
            }
            return None;
        }
        Admin::LoadFiles => {
            match file_settings(client, Method::GET, None).await {
                Ok(settings) => send(AdminUpdate::Files(settings)),
                Err(text) => send(AdminUpdate::Failed(text)),
            }
            return None;
        }
        Admin::SaveFiles(limits) => file_settings(client, Method::PUT, Some(json!(limits)))
            .await
            .map(|settings| {
                send(AdminUpdate::Files(settings));
                t!("admin.done_file_limits")
            }),
        Admin::LoadHistory => {
            match newer_setting(client, HISTORY, Method::GET, None).await {
                Ok(history) => send(AdminUpdate::History(history)),
                Err(text) => send(AdminUpdate::Failed(text)),
            }
            return None;
        }
        Admin::SaveHistory(keep) => {
            newer_setting(client, HISTORY, Method::PUT, Some(json!({ "keep": keep })))
                .await
                .map(|history| {
                    send(AdminUpdate::History(history));
                    t!("admin.done_history")
                })
        }
        Admin::LoadRateLimits => {
            match newer_setting(client, RATE_LIMITS, Method::GET, None).await {
                Ok(limits) => send(AdminUpdate::RateLimits(limits)),
                Err(text) => send(AdminUpdate::Failed(text)),
            }
            return None;
        }
        Admin::LoadNewMembers => {
            match newer_setting(client, NEW_MEMBERS, Method::GET, None).await {
                Ok(rules) => send(AdminUpdate::NewMembers(rules)),
                Err(text) => send(AdminUpdate::Failed(text)),
            }
            return None;
        }
        Admin::SaveRateLimits(preset) => {
            let body = Some(json!({ "preset": preset }));
            newer_setting(client, RATE_LIMITS, Method::PUT, body)
                .await
                .map(|limits| {
                    send(AdminUpdate::RateLimits(limits));
                    t!("admin.done_rate_limits")
                })
        }
        Admin::SaveNewMembers(secs) => newer_setting(
            client,
            NEW_MEMBERS,
            Method::PUT,
            Some(json!({ "secs": secs })),
        )
        .await
        .map(|rules| {
            send(AdminUpdate::NewMembers(rules));
            t!("admin.done_new_members")
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
            return None;
        }
        Admin::Invites => {
            match invites(client).await {
                Ok(list) => send(AdminUpdate::Invites(list)),
                Err(text) => send(AdminUpdate::Failed(text)),
            }
            return None;
        }
        Admin::Audit { action, before } => {
            send(match audit_page(client, action.as_deref(), before).await {
                Ok((entries, next)) => AdminUpdate::Audit {
                    entries,
                    older: before.is_some(),
                    next,
                },
                Err(text) => AdminUpdate::AuditFailed(text),
            });
            return None;
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
        Admin::Timeout {
            person,
            seconds,
            reason,
        } => {
            let reason_ct = reason
                .map(|reason| {
                    client.encrypt_metadata(Metadata::TimeoutReason, &person, reason.as_bytes())
                })
                .transpose()
                .map_err(|error| failed(&error));
            match reason_ct {
                Ok(reason_ct) => call(
                    client,
                    Method::PUT,
                    &format!("/v1/members/{}/timeout", encode(&person)),
                    Some(json!({ "seconds": seconds, "reason_ct": reason_ct })),
                )
                .await
                .map(|()| t!("timeout.done")),
                Err(text) => Err(text),
            }
        }
        Admin::EndTimeout(person) => call(
            client,
            Method::DELETE,
            &format!("/v1/members/{}/timeout", encode(&person)),
            None,
        )
        .await
        .map(|()| t!("timeout.done_ended")),
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
                .map(|()| {
                    // Now saved: Save waits for the next change.
                    send(AdminUpdate::Joining(open, cap));
                    t!("admin.done_joining")
                })
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
        } => {
            let done = add_emoji_retrying(client, &name, sticker, &bytes, pack.as_ref()).await;
            if done.is_ok() {
                send(AdminUpdate::EmojiAdded(name));
            }
            done
        }
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

        // `run` takes batches apart; nothing puts one inside another.
        Admin::Batch(_) => Err(t!("admin.failed").into()),
    };
    Some(result)
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

async fn open_joining(client: &KeyClient<'_>) -> Result<AdminUpdate, String> {
    let settings = get(client, "/v1/settings/open-joining")
        .await
        .map_err(|error| failed(&error))?;
    Ok(AdminUpdate::Joining(
        settings["open_joining"].as_bool().unwrap_or_default(),
        (settings["member_cap"].as_u64()).map_or(100, |cap| cap.try_into().unwrap_or(u32::MAX)),
    ))
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
                can_lift: row["can_lift"].as_bool().unwrap_or(true),
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

/// Audit log entries per page.
const AUDIT_PAGE: usize = 50;

fn audit_path(action: Option<&str>, before: Option<i64>) -> String {
    let before = before.unwrap_or(i64::MAX);
    let mut path = format!("/v1/audit?before={before}&limit={AUDIT_PAGE}");
    if let Some(action) = action {
        path.push_str(&format!("&action={action}"));
    }
    path
}

/// The newest page below `before`, and the cursor for the next when there may be more.
async fn audit_page(
    client: &KeyClient<'_>,
    action: Option<&str>,
    before: Option<i64>,
) -> Result<(Vec<AuditEntry>, Option<i64>), String> {
    let page = get(client, &audit_path(action, before))
        .await
        .map_err(|error| failed(&error))?;
    let rows = page.as_array().cloned().unwrap_or_default();
    let ids: Vec<i64> = rows.iter().filter_map(|row| row["id"].as_i64()).collect();
    // Servers before `before` ignore it and answer oldest first: read it all, as then.
    let newest_first = ids.len() == rows.len()
        && ids.windows(2).all(|pair| pair[0] > pair[1])
        && ids
            .first()
            .is_none_or(|&id| id < before.unwrap_or(i64::MAX));
    if !newest_first {
        return Ok((audit_log(client, action).await?, None));
    }
    let next = ids.last().copied().filter(|_| rows.len() == AUDIT_PAGE);
    Ok((rows.iter().filter_map(audit_entry).collect(), next))
}

fn audit_entry(row: &Value) -> Option<AuditEntry> {
    let person = |key: &str| row[key].as_str().and_then(|id| decode(id).ok());
    Some(AuditEntry {
        actor: person("actor_person_id")?,
        action: row["action"].as_str().unwrap_or_default().to_owned(),
        target: person("target_person_id"),
        target_id: person("target_id"),
        at: row["at"].as_i64().unwrap_or_else(now),
        details: row["details"].clone(),
    })
}

/// For servers that only page oldest first: read it all and return it newest first.
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
            entries.extend(audit_entry(row));
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
            Ok(Ok(bytes)) => {
                add_emoji_retrying(client, &item.name, sticker, &bytes, Some(&pack)).await
            }
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

/// `add_emoji`, waiting and trying again while the server says it is busy: an import
/// or several picked pictures go up one after another.
async fn add_emoji_retrying(
    client: &KeyClient<'_>,
    name: &str,
    sticker: bool,
    bytes: &[u8],
    pack: Option<&Id>,
) -> Result<&'static str, String> {
    for attempt in 0..4 {
        match add_emoji(client, name, sticker, bytes, pack).await {
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

const HISTORY: &str = "/v1/settings/message-history";
const NEW_MEMBERS: &str = "/v1/settings/new-member-rules";

const RATE_LIMITS: &str = "/v1/settings/rate-limits";

/// A setting older servers lack: `None` when the server predates it.
async fn newer_setting<T: serde::de::DeserializeOwned>(
    client: &KeyClient<'_>,
    path: &str,
    method: Method,
    body: Option<Value>,
) -> Result<Option<T>, String> {
    let reply = if method == Method::GET {
        match client.request(Method::GET, path).send().await {
            Ok(reply) if reply.status() == reqwest::StatusCode::NOT_FOUND => return Ok(None),
            Ok(reply) if reply.status().is_success() => reply,
            // Ask again through `request`, which puts the problem into words.
            _ => request(client, method, path, body).await?,
        }
    } else {
        request(client, method, path, body).await?
    };
    reply
        .json()
        .await
        .map(Some)
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
        "timeout_not_found" => t!("timeout.not_found"),
        "not_in_voice" => t!("admin.not_in_voice"),
        "voice_move_denied" => t!("admin.voice_move_denied"),
        "ban_not_found" => t!("admin.ban_not_found"),
        "invite_not_found" => t!("admin.invite_not_found"),
        "invalid_member_cap" => t!("admin.invalid_member_cap"),
        "invalid_file_limits" => t!("admin.invalid_file_limits"),
        "invalid_message_history" => t!("admin.invalid_message_history"),
        "invalid_new_member_rules" => t!("admin.invalid_new_members"),
        "new_member_rules" => t!("new_members.refused"),
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

#[cfg(test)]
mod tests {
    use super::{AdminUpdate, audit_path, batch};

    /// Three changes; the ones at `failing` are refused.
    fn save(failing: &[usize]) -> (bool, AdminUpdate) {
        batch(
            (0..3)
                .map(|index| match failing.contains(&index) {
                    true => Some(Err("refused".to_owned())),
                    false => Some(Ok("saved")),
                })
                .collect(),
        )
    }

    #[test]
    fn a_batch_reloads_when_anything_changed_and_says_which_failed() {
        let (reload, update) = save(&[]);
        assert!(reload);
        assert!(matches!(update, AdminUpdate::Done("saved")));
        let (reload, update) = save(&[1]);
        assert!(reload, "what went through still shows");
        assert!(matches!(update, AdminUpdate::PartlySaved { failed, .. } if failed == [1]));
        let (reload, update) = save(&[0, 1, 2]);
        assert!(!reload, "nothing changed");
        assert!(matches!(update, AdminUpdate::PartlySaved { failed, .. } if failed == [0, 1, 2]));
    }

    #[test]
    fn audit_pages_are_fifty_newest_first() {
        assert_eq!(
            audit_path(None, None),
            "/v1/audit?before=9223372036854775807&limit=50"
        );
        assert_eq!(
            audit_path(Some("member_banned"), Some(71)),
            "/v1/audit?before=71&limit=50&action=member_banned"
        );
    }
}
