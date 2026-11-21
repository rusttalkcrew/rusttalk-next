//! The public server directory: searched only when the person opens "Find servers".

use std::time::Duration;

use rusttalk_core::{
    auth::{Fingerprint, KnownServer, ProtocolError, decode},
    client::Server as Api,
    index::{Listing, https_url, validate_details},
    pinned_tls::https_client,
};
use url::Url;

use super::{Ctx, Preview, open_joining};

/// Results per page, as the index serves them.
pub const PAGE_SIZE: usize = 25;
/// 25 listings of at most about 2 KiB each, with room to spare.
const MAX_BODY: usize = 256 * 1024;

/// A server from the directory. Its name and description are public text from the index.
#[derive(Clone)]
pub struct Listed {
    pub server: KnownServer,
    pub name: String,
    pub description: String,
    pub members: u32,
    pub open: bool,
}

/// One page of listings matching `query`, from the index at `index`.
pub(super) async fn search(
    index: &Url,
    pin: Option<Fingerprint>,
    query: &str,
    page: u32,
) -> Result<Vec<Listed>, String> {
    let unreachable = |error: &dyn std::fmt::Display| {
        tracing::warn!(%error, "searching the server directory failed");
        t!("find.unreachable").to_owned()
    };
    let mut base = index.clone();
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    let url = base.join("v1/servers").map_err(|e| unreachable(&e))?;
    let client = https_client(pin).map_err(|e| unreachable(&e))?;
    let mut reply = client
        .get(url)
        .query(&[("q", query), ("page", &page.to_string())])
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| unreachable(&e.without_url()))?;
    // The index is a third party: read a bounded body only.
    let mut body = Vec::new();
    while let Some(chunk) = reply
        .chunk()
        .await
        .map_err(|e| unreachable(&e.without_url()))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BODY {
            return Err(unreachable(&"directory reply too large"));
        }
    }
    let listings: Vec<Listing> = serde_json::from_slice(&body).map_err(|e| unreachable(&e))?;
    Ok(listings.into_iter().filter_map(listed).collect())
}

/// Keep only listings that pass the index's own checks, so a bad index can't show hidden text.
fn listed(listing: Listing) -> Option<Listed> {
    validate_details(&listing.name, &listing.description, &listing.address).ok()?;
    Some(Listed {
        server: KnownServer {
            address: https_url(&listing.address)?,
            server_id: decode(&listing.server_id).ok()?,
            tls_fingerprint: None,
        },
        name: listing.name,
        description: listing.description,
        members: listing.member_count,
        open: listing.open_joining,
    })
}

/// Check that the listed address is the listed server, and whether it lets anyone join.
pub(super) async fn preview(ctx: &Ctx, listed: Listed) -> Result<Preview, String> {
    let known = Api::pinned(&ctx.data, &listed.server.address).map_err(|error| {
        tracing::error!(error = %format_args!("{error:#}"), "reading server pins failed");
        t!("net.saved_servers_failed").to_owned()
    })?;
    // A listing carries no certificate pin; one saved for this address still applies.
    let tls = known.as_ref().and_then(|known| known.tls_fingerprint);
    let expected = KnownServer {
        tls_fingerprint: tls,
        ..listed.server
    };
    let api = Api::probe(expected.address.clone(), tls, Some(&expected))
        .await
        .map_err(|error| problem(&error, tls.is_some()))?;
    tracing::info!(known = known.is_some(), "listed server checked");
    Ok(Preview {
        server: api.known_server().clone(),
        invite: None,
        name: Some(listed.name),
        known: known.is_some(),
        open: open_joining(&api).await,
    })
}

fn problem(error: &anyhow::Error, pinned: bool) -> String {
    tracing::warn!(error = %format_args!("{error:#}"), "listed server check failed");
    let text = format!("{error:#}");
    match error.downcast_ref::<ProtocolError>() {
        Some(ProtocolError::ServerIdChanged | ProtocolError::ServerProof) => {
            t!("find.mismatch").into()
        }
        Some(ProtocolError::TlsChanged) => t!("net.server_changed").into(),
        _ if pinned && rusttalk_core::pinned_tls::is_pin_mismatch(error.as_ref()) => {
            t!("net.server_changed").into()
        }
        // Without a pin only a publicly trusted certificate can be checked.
        _ if !pinned && text.contains("certificate") => t!("find.certificate").into(),
        _ => t!("find.server_unreachable").into(),
    }
}
