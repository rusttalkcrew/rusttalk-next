//! Page channels' documents, and the newest version this device has seen of each, so a
//! server can't pass off an older page as the current one.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use reqwest::StatusCode;
use rusttalk_core::{
    auth::{decode, encode},
    client::{KeyClient, MessageError},
    pages::{Block, Layout, PageRecord, Tile},
};
use sha2::{Digest, Sha256};
use url::Url;

use super::{Ctx, State, Update};
use crate::server::{Body, Id, Msg, Page};

/// By channel: the newest version opened here and a digest of its envelope.
pub(super) type Seen = HashMap<Id, (i64, Id)>;

fn seen_path(data: &Path, server: &Id) -> PathBuf {
    data.join("page_versions")
        .join(format!("{}.json", hex::encode(server)))
}

pub(super) fn load_seen(data: &Path, server: &Id) -> Seen {
    match read_seen(&seen_path(data, server)) {
        Ok(seen) => seen,
        Err(error) => {
            tracing::error!(error = %format_args!("{error:#}"), "seen page versions unreadable, starting fresh");
            Seen::new()
        }
    }
}

fn read_seen(path: &Path) -> Result<Seen> {
    let saved: HashMap<String, (i64, String)> = crate::load_json(path)?;
    (saved.iter())
        .map(|(channel, (version, digest))| Ok((decode(channel)?, (*version, decode(digest)?))))
        .collect()
}

fn save_seen(data: &Path, server: &Id, seen: &Seen) -> Result<()> {
    let path = seen_path(data, server);
    std::fs::create_dir_all(path.parent().expect("in a folder"))?;
    let saved: HashMap<String, (i64, String)> = (seen.iter())
        .map(|(channel, (version, digest))| (encode(channel), (*version, encode(digest))))
        .collect();
    rusttalk_core::client::save_json(data, &path, &saved).context("save seen page versions")
}

fn digest(record: &PageRecord) -> Id {
    Sha256::digest(record.envelope.as_bytes()).into()
}

/// Whether what the server holds (`None` for no document) is older than what was seen,
/// or differs under the same version.
fn outdated(seen: Option<&(i64, Id)>, stored: Option<(i64, Id)>) -> bool {
    match (seen, stored) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some((newest, known)), Some((version, digest))) => {
            version < *newest || (version == *newest && digest != *known)
        }
    }
}

/// The version a save gets: past the one it replaces and any seen here.
fn next_version(base: i64, seen: Option<&(i64, Id)>) -> i64 {
    seen.map_or(base, |(newest, _)| base.max(*newest))
        .saturating_add(1)
}

fn remember(ctx: &Ctx, state: &mut State, channel: Id, record: &PageRecord) {
    let newest = (record.version, digest(record));
    if state.pages_seen.insert(channel, newest) == Some(newest) {
        return;
    }
    if let Err(error) = save_seen(&ctx.data, &state.server_id, &state.pages_seen) {
        tracing::warn!(error = %format_args!("{error:#}"), "saving seen page versions failed");
    }
}

pub(super) fn is_page(state: &State, channel: &Id) -> bool {
    (state.last.iter())
        .flat_map(|dir| &dir.categories)
        .flat_map(|category| &category.channels)
        .any(|found| found.page && found.id == *channel)
}

fn msg(channel: Id, record: &PageRecord, body: Body) -> Msg {
    Msg {
        backfilled: false,
        id: channel,
        person: record.person,
        at: record.at,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        body,
        files: Vec::new(),
        live: None,
        sticker: None,
        replicas: Vec::new(),
        copy_files: Vec::new(),
    }
}

/// A page's title and text, for its notification and mentions.
fn all_text(layout: &Layout) -> String {
    let tiles = (layout.tiles())
        .flat_map(|tile| match tile {
            Tile::Blocks(blocks) => blocks.as_slice(),
            Tile::Unknown { .. } => &[],
        })
        .filter_map(|block| match block {
            Block::Markdown(text) => Some(text.as_str()),
            Block::Picture(_) => None,
        });
    let parts: Vec<&str> = std::iter::once(layout.title.as_str())
        .filter(|title| !title.is_empty())
        .chain(tiles)
        .collect();
    parts.join("\n\n")
}

fn opened(channel: Id, record: &PageRecord, layout: Layout) -> Page {
    Page {
        version: record.version,
        msg: Some(msg(channel, record, Body::Text(all_text(&layout)))),
        layout,
        outdated: false,
    }
}

/// Fetch, check and open a page. An error means it couldn't be fetched, which unlike an
/// unreadable page must not offer to replace it.
async fn load(
    ctx: &Ctx,
    client: &mut KeyClient<'_>,
    state: &mut State,
    channel: Id,
) -> Result<Page> {
    let record = client.page(&channel).await?;
    let version = record.as_ref().map_or(0, |record| record.version);
    let stored = (record.as_ref()).map(|record| (record.version, digest(record)));
    if outdated(state.pages_seen.get(&channel), stored) {
        tracing::warn!(version, "the server sent an older page than seen here");
        return Ok(Page {
            version,
            outdated: true,
            ..Page::default()
        });
    }
    let Some(record) = record else {
        return Ok(Page::default());
    };
    let body = match client.open_page(&channel, &record).await {
        Ok(layout) => {
            // Only a page that verified counts, or the server could make up a huge version.
            remember(ctx, state, channel, &record);
            return Ok(opened(channel, &record, layout));
        }
        Err(error) => {
            if let Some(failed) = error.downcast_ref::<reqwest::Error>()
                && failed.status() != Some(StatusCode::NOT_FOUND)
            {
                return Err(error);
            }
            if error.downcast_ref::<MessageError>() == Some(&MessageError::MissingKey) {
                state.waiting_pages.insert(channel);
            }
            tracing::warn!(error = %format_args!("{error:#}"), "page could not be opened");
            Body::Unreadable
        }
    };
    Ok(Page {
        version,
        msg: Some(msg(channel, &record, body)),
        ..Page::default()
    })
}

/// Load a page for the screen.
pub(super) async fn show(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    channel: Id,
) {
    // Without support the view says the server needs an update.
    if !state.pages {
        return;
    }
    let page = match load(ctx, client, state, channel).await {
        Ok(page) => Some(page),
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "loading a page failed");
            None
        }
    };
    ctx.send(Update::Page(url.clone(), channel, page.map(Box::new)));
}

/// Someone saved a page: show it if it is open, and mark it unread.
pub(super) async fn changed(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    channel: Id,
) -> Result<()> {
    let page = load(ctx, client, state, channel).await?;
    if let Some(msg) = &page.msg {
        ctx.send(Update::Arrived(url.clone(), channel, msg.clone()));
    }
    if state.loaded.contains(&channel) {
        ctx.send(Update::Page(url.clone(), channel, Some(Box::new(page))));
    }
    Ok(())
}

/// Save `layout` over version `base`. The page as it then is always goes out before the
/// result, so the editor can compare.
pub(super) async fn save(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    (channel, base, mut layout, uploads): (Id, i64, Layout, Vec<(Id, PathBuf)>),
) {
    let unsaved = || ctx.send(Update::PageSaved(url.clone(), channel, false));
    let scope = rusttalk_core::keys::Scope {
        kind: rusttalk_core::keys::Kind::Channel,
        id: channel,
    };
    // Before uploading, so a save that can't work leaves nothing on the server.
    if let Err(error) = client.current_key(scope) {
        tracing::warn!(error = %format_args!("{error:#}"), "no key to save a page with");
        ctx.send(Update::Failed(t!("net.page_save_failed").into()));
        return unsaved();
    }
    let (standins, paths): (Vec<Id>, Vec<PathBuf>) = uploads.into_iter().unzip();
    let Some(fresh) = super::files::upload_pictures(ctx, url, client, channel, paths).await else {
        return unsaved();
    };
    layout.map_pictures(|file| {
        if let Some(at) = standins.iter().position(|id| *id == file.file.id) {
            *file = fresh[at].clone();
        }
    });
    let version = next_version(base, state.pages_seen.get(&channel));
    let saved = match client.save_page(&channel, base, version, &layout).await {
        Ok(Some(record)) => {
            remember(ctx, state, channel, &record);
            let page = opened(channel, &record, layout);
            ctx.send(Update::Page(url.clone(), channel, Some(Box::new(page))));
            true
        }
        Ok(None) => {
            tracing::info!("page changed meanwhile, not saved");
            super::files::discard(client, &fresh).await;
            show(ctx, url, client, state, channel).await;
            false
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "saving a page failed");
            if refused(&error) {
                super::files::discard(client, &fresh).await;
            }
            ctx.send(Update::Failed(t!("net.page_save_failed").into()));
            false
        }
    };
    ctx.send(Update::PageSaved(url.clone(), channel, saved));
}

/// Whether a failed save surely didn't happen. After a lost answer the page may already
/// use the new uploads, so those are then left to expire by themselves.
fn refused(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<MessageError>(),
        Some(MessageError::PageLimit)
    ) || (error.downcast_ref::<reqwest::Error>())
        .and_then(reqwest::Error::status)
        .is_some_and(|status| status.is_client_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_or_forked_page_is_outdated() {
        let seen = Some(&(5, [5; 32]));
        assert!(outdated(seen, Some((4, [4; 32]))));
        assert!(
            outdated(seen, Some((5, [9; 32]))),
            "other content, same version"
        );
        assert!(outdated(seen, None), "the document vanished");
        assert!(!outdated(seen, Some((5, [5; 32]))));
        assert!(!outdated(seen, Some((6, [6; 32]))));
        assert!(!outdated(None, None) && !outdated(None, Some((1, [1; 32]))));
    }

    #[test]
    fn a_save_gets_past_what_was_seen_before_a_restored_backup() {
        assert_eq!(next_version(0, None), 1);
        assert_eq!(next_version(7, Some(&(7, [7; 32]))), 8);
        assert_eq!(next_version(3, Some(&(9, [9; 32]))), 10);
    }

    #[test]
    fn seen_versions_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let server = [1; 32];
        assert!(load_seen(dir.path(), &server).is_empty());
        let seen = Seen::from([([2; 32], (4, [3; 32]))]);
        save_seen(dir.path(), &server, &seen).unwrap();
        assert_eq!(load_seen(dir.path(), &server), seen);
        assert!(load_seen(dir.path(), &[9; 32]).is_empty(), "per server");
    }
}
