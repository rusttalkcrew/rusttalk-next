//! Server settings: members, roles, bans, the audit log, joining and invites.
//! Every refusal comes back to the settings window in plain words.

use reqwest::Method;
use rusttalk_core::{
    auth::{decode, encode},
    client::KeyClient,
    keys::{Metadata, random_key},
};
use serde_json::{Value, json};
use url::Url;

use super::{Ctx, State, Update, directory, get, now};
use crate::server::Id;

pub enum Admin {
    /// What the window shows, as far as this person may see it.
    Load {
        joining: bool,
        audit: bool,
        bans: bool,
    },
    Kick(Id),
    /// Ban a person, and (`true`) the networks they use.
    Ban(Id, bool),
    Unban(Id),
    /// Replace a member's roles.
    SetRoles(Id, Vec<String>),
    CreateRole {
        name: String,
        permissions: Vec<&'static str>,
    },
    RenameRole(String, String),
    /// A custom role's new name and permissions.
    EditRole {
        id: String,
        name: String,
        permissions: Vec<&'static str>,
    },
    DeleteRole(String),
    DefaultRole(String),
    /// Channel, role and its read/write/speak/video flags there.
    ChannelFlags(Id, String, u8),
    /// The whole audit log, or only one action.
    Audit(Option<String>),
    /// Open joining on or off, with a member limit.
    Joining(bool, u32),
    /// Active invites, as far as this person may see them.
    Invites,
    RevokeInvite(String),
    /// Replace the server icon (PNG, JPEG or GIF bytes).
    SetIcon(Vec<u8>),
    /// The owner's public listing setting.
    LoadListing,
    /// List the server publicly with these details, or stop (`None`).
    SaveListing(Option<PublicListing>),
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
    /// Whether the server is listed, and the saved details.
    Listing(bool, Option<PublicListing>),
    /// A change went through; the text says what happened.
    Done(&'static str),
    Failed(String),
}

/// Ranks for new roles: above Member (0), below Moderator (50).
// shortcut: every new role gets the same rank; add role ordering if people ask for it.
const NEW_ROLE_RANK: i64 = 25;

pub(super) async fn run(
    ctx: &Ctx,
    url: &Url,
    client: &KeyClient<'_>,
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
        Admin::Kick(person) => call(
            client,
            Method::DELETE,
            &format!("/v1/members/{}", encode(&person)),
            None,
        )
        .await
        .map(|()| "Removed from the server."),
        Admin::Ban(person, network) => {
            let path = format!("/v1/bans/{}", encode(&person));
            call(client, Method::PUT, &path, Some(json!({"ban_ip": network})))
                .await
                .map(|()| "Banned.")
                .map_err(|text| match text == problem("role_hierarchy") && network {
                    // Also refused when you or someone ranked as high as you uses that network.
                    true => "Can't block their network: you or someone with a role as high as yours uses it too. Ban without blocking it.".into(),
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
        .map(|()| "Unbanned. They can join again with an invite."),
        Admin::SetRoles(person, roles) => {
            let path = format!("/v1/members/{}/roles", encode(&person));
            call(client, Method::PUT, &path, Some(json!({"role_ids": roles})))
                .await
                .map(|()| "Roles saved.")
        }
        Admin::CreateRole { name, permissions } => create_role(client, &name, &permissions).await,
        Admin::RenameRole(id, name) => match name_ct(client, &id, &name) {
            Ok(name_ct) => {
                let path = format!("/v1/roles/{id}/name");
                call(
                    client,
                    Method::PUT,
                    &path,
                    Some(json!({"name_ct": name_ct})),
                )
                .await
                .map(|()| "Role renamed.")
            }
            Err(text) => Err(text),
        },
        Admin::EditRole {
            id,
            name,
            permissions,
        } => match name_ct(client, &id, &name) {
            Ok(name_ct) => {
                let body = json!({"permissions": permissions, "name_ct": name_ct});
                call(client, Method::PUT, &format!("/v1/roles/{id}"), Some(body))
                    .await
                    .map(|()| "Role saved.")
            }
            Err(text) => Err(text),
        },
        Admin::DeleteRole(id) => call(client, Method::DELETE, &format!("/v1/roles/{id}"), None)
            .await
            .map(|()| "Role deleted."),
        Admin::DefaultRole(id) => call(
            client,
            Method::PUT,
            "/v1/roles/default",
            Some(json!({"role_id": id})),
        )
        .await
        .map(|()| "New members now get this role."),
        Admin::ChannelFlags(channel, role, flags) => {
            let path = format!("/v1/channels/{}/permissions/{role}", encode(&channel));
            call(client, Method::PUT, &path, Some(json!({"flags": flags})))
                .await
                .map(|()| "Channel permissions saved.")
        }
        Admin::Joining(open, cap) => {
            let body = json!({"open_joining": open, "member_cap": cap});
            call(client, Method::PUT, "/v1/settings/open-joining", Some(body))
                .await
                .map(|()| "Joining settings saved.")
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
            .map(|()| "Listing saved. It shows up in the directory within a minute.")
        }
        Admin::SetIcon(bytes) => match client.set_server_icon(&bytes).await {
            Ok(()) => Ok("Server icon changed."),
            Err(error) => Err(failed(&error)),
        },
        Admin::RevokeInvite(id) => call(client, Method::DELETE, &format!("/v1/invites/{id}"), None)
            .await
            .map(|()| "That invite link no longer works."),
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
    permissions: &[&str],
) -> Result<&'static str, String> {
    let id = random_key().map_err(|error| {
        tracing::error!(%error, "making a role ID failed");
        FAILED.to_owned()
    })?;
    // The API takes 16-byte role IDs.
    let id = encode(&id[..16]);
    let body = json!({
        "id": id,
        "rank": NEW_ROLE_RANK,
        "permissions": permissions,
        "name_ct": name_ct(client, &id, name)?,
    });
    call(client, Method::POST, "/v1/roles", Some(body))
        .await
        .map(|()| "Role created.")
}

fn name_ct(client: &KeyClient<'_>, id: &str, name: &str) -> Result<String, String> {
    client
        .encrypt_metadata(Metadata::RoleName, id.as_bytes(), name.as_bytes())
        .map_err(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "encrypting a role name failed");
            "The server's key hasn't arrived yet. Try again in a moment.".to_owned()
        })
}

const FAILED: &str = "That didn't work. Try again.";

fn failed(error: &anyhow::Error) -> String {
    tracing::warn!(error = %format_args!("{error:#}"), "server settings request failed");
    match error
        .downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
    {
        Some(reqwest::StatusCode::FORBIDDEN) => "You don't have permission to do that.".into(),
        _ => FAILED.into(),
    }
}

/// Send a request and turn a refusal into plain words.
async fn call(
    client: &KeyClient<'_>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<(), String> {
    let mut request = client.request(method, path);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let reply = request.send().await.map_err(|error| {
        tracing::warn!(error = %error.without_url(), "server settings request failed");
        "You're offline. Try again in a moment.".to_owned()
    })?;
    let status = reply.status();
    if status.is_success() {
        return Ok(());
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
        "role_hierarchy" => "You can only do this to people and roles below your own role.",
        "cannot_grant_permission" => "You can't give permissions you don't have yourself.",
        "permission_denied" => "You don't have permission to do that.",
        "owner_required" => "Only the server owner can do that.",
        "built_in_role" => "Built-in roles can't be deleted.",
        "role_in_use" => "Someone still has this role, or new members get it. Change that first.",
        "role_limit" => "This server has as many roles as it can have.",
        "already_banned" => "They're already banned.",
        "no_active_ip" => {
            "Their network isn't known right now. Ban them without blocking their network."
        }
        "member_not_found" => "They're no longer in this server.",
        "ban_not_found" => "They aren't banned anymore.",
        "invite_not_found" => "That invite link already stopped working.",
        "invalid_member_cap" => "The member limit must be 1 or more.",
        "invalid_listing" | "invalid_listing_settings" => {
            "Check the listing: a name, a description of up to 1,000 characters, and an https:// address."
        }
        "invalid_index_url" => "The directory address must start with https://.",
        "invalid_index_tls_fingerprint" => "That certificate fingerprint isn't right.",
        "invalid_channel_flags" => "Write, Speak and Video also need Read.",
        "invalid_role_count" => "Everyone needs at least one role.",
        "space_key_unavailable" | "metadata_epoch_changed" => {
            "The server's key is changing. Try again in a moment."
        }
        _ => FAILED,
    }
}
