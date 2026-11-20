use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use reqwest::{Client, Method, StatusCode, Url};
use rusttalk_core::{
    auth::{KnownServer, Session, decode, encode},
    client::Server,
    index::{Listing, Registration, now_ms},
    pinned_tls::https_client,
};
use serde_json::{Value, json};
use std::time::Duration;

use super::{compose_bytes, rejected};

pub const SETTINGS: &str = "/v1/settings/public-listing";
const NAME: &str = "W16 Public Clover";
const DESCRIPTION: &str = "W16 public directory listing";

async fn search(client: &Client, url: &Url) -> Result<Vec<Listing>> {
    Ok(client
        .get(url.clone())
        .query(&[("q", "Public Clover"), ("page", "1")])
        .timeout(Duration::from_secs(2))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

pub async fn exercise(project: &str, api: &Server, owner: &Session) -> Result<()> {
    let known = api.known_server();
    let tls = known.tls_fingerprint.context("dev TLS pin missing")?;
    let index = https_client(Some(tls)).map_err(anyhow::Error::from_boxed)?;
    let url = known.address.join("index/v1/servers")?;
    let health = known.address.join("index/v1/health")?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if index
                .get(health.clone())
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_ok_and(|response| response.status() == StatusCode::OK)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("waiting for the HTTPS index health check")?;
    let settings = |method| api.request(method, SETTINGS).bearer_auth(&owner.token);
    let state: Value = settings(Method::GET)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(state, json!({"enabled": false, "listing": null}));
    assert!(search(&index, &url).await?.is_empty());

    let forged = Registration::signed(
        Listing {
            server_id: encode(&known.server_id),
            name: NAME.into(),
            description: DESCRIPTION.into(),
            address: known.address.to_string(),
            member_count: 1,
            open_joining: false,
        },
        now_ms().context("system clock")?,
        &SigningKey::from_bytes(&[42; 32]),
    )?;
    rejected(
        index
            .post(url.clone())
            .timeout(Duration::from_secs(2))
            .json(&forged)
            .send()
            .await?,
        StatusCode::UNAUTHORIZED,
        "invalid_signature",
    )
    .await?;

    let enabled = json!({
        "enabled": true,
        "listing": {
            "name": NAME,
            "description": DESCRIPTION,
            "address": known.address.as_str(),
            "index_url": "https://caddy:8443/index/",
            "index_tls_fingerprint": encode(&tls),
        }
    });
    let response = settings(Method::PUT).json(&enabled).send().await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let saved: Value = settings(Method::GET)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(saved, enabled);

    let entry = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if let Some(entry) = search(&index, &url).await?.into_iter().next() {
                return Ok::<_, anyhow::Error>(entry);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("waiting for the API's signed index registration")??;
    assert_eq!(entry.name, NAME);
    assert_eq!(entry.description, DESCRIPTION);
    assert_eq!(entry.member_count, 1);
    assert!(!entry.open_joining);
    let open_joining = |enabled: bool| {
        api.request(Method::PUT, "/v1/settings/open-joining")
            .bearer_auth(&owner.token)
            .json(&json!({"open_joining": enabled, "member_cap": 100}))
    };
    let response = open_joining(true).send().await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    tokio::time::timeout(Duration::from_secs(12), async {
        while !search(&index, &url)
            .await?
            .first()
            .is_some_and(|entry| entry.open_joining)
        {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("waiting for the listing to report open joining")??;
    let response = open_joining(false).send().await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let found = KnownServer {
        address: entry.address.parse()?,
        server_id: decode(&entry.server_id)?,
        tls_fingerprint: Some(tls),
    };
    let connected = Server::probe(found.address.clone(), Some(tls), Some(&found)).await?;
    assert!(connected.known_server() == known);

    let response = settings(Method::PUT)
        .json(&json!({"enabled": false}))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    tokio::time::timeout(Duration::from_secs(12), async {
        while !search(&index, &url).await?.is_empty() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("waiting for the disabled listing to expire")??;
    for bytes in [
        compose_bytes(
            project,
            &[
                "exec",
                "-T",
                "api",
                "sh",
                "-ec",
                "cat /data/rusttalk.sqlite3 /data/rusttalk.sqlite3-wal",
            ],
        )?,
        compose_bytes(project, &["logs", "--no-color"])?,
    ] {
        for text in [NAME, DESCRIPTION] {
            assert!(
                !bytes
                    .windows(text.len())
                    .any(|window| window == text.as_bytes())
            );
        }
    }
    println!(
        "W16 owner published, pinned bot search verified the server ID, open joining reached the listing, disabling let the listing expire, and a forged signature was rejected"
    );
    Ok(())
}
