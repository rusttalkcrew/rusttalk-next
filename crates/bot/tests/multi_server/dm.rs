//! W70: one DM over every server both people share.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use reqwest::{Method, StatusCode};
use rusttalk_core::{
    auth::encode,
    client::{KeyClient, MessageBody, Server},
    keys::{self, Scope},
};
use serde_json::json;

use super::super::{compose, compose_bytes, receive_key, rejected};

const BOTH: &str = "W70 Beryl DM on both servers";
const REPLY: &str = "W70 Iolite DM reply on both servers";
const DOWN: &str = "W70 Spinel DM while server B is down";
const BACK: &str = "W70 Zircon DM after server B came back";
const LEFT: &str = "W70 Topaz DM after leaving server B";

/// One person's two clients, on server A and server B.
pub type Pair<'a, 'b, 'c> = (&'a mut KeyClient<'b>, &'a mut KeyClient<'c>);

fn body(text: &str) -> MessageBody {
    MessageBody {
        text: text.to_owned(),
        ..Default::default()
    }
}

/// Post one message to both replicas with one ID; `false` where a server refused it.
async fn post(sender: Pair<'_, '_, '_>, dms: [Scope; 2], text: &str) -> Result<[bool; 2]> {
    let id = *keys::random_key()?;
    let on_a = sender.0.post_message(&dms[0].id, id, &body(text)).await;
    let on_b = sender.1.post_message(&dms[1].id, id, &body(text)).await;
    Ok([on_a.is_ok(), on_b.is_ok()])
}

/// One replica's history, oldest first: server time, person, ID and text.
async fn history(
    client: &mut KeyClient<'_>,
    dm: Scope,
) -> Result<Vec<(i64, [u8; 32], [u8; 32], String)>> {
    let mut items = Vec::new();
    for record in client.message_page(&dm.id, None, 100).await?.iter().rev() {
        let text = client.open_message(&dm.id, record).await?.text;
        items.push((record.at, record.person, record.id, text));
    }
    Ok(items)
}

/// What a client shows: both replicas merged by person and ID, by earliest server time.
async fn merged(person: Pair<'_, '_, '_>, dms: [Scope; 2]) -> Result<Vec<String>> {
    let mut all: Vec<(i64, [u8; 32], [u8; 32], String)> = Vec::new();
    let copies = [
        history(person.0, dms[0]).await?,
        history(person.1, dms[1]).await?,
    ];
    for item in copies.into_iter().flatten() {
        match all
            .iter_mut()
            .find(|old| old.1 == item.1 && old.2 == item.2)
        {
            Some(old) => old.0 = old.0.min(item.0),
            None => all.push(item),
        }
    }
    all.sort_by_key(|item| item.0);
    Ok(all.into_iter().map(|item| item.3).collect())
}

/// Wait until the restarted API answers again.
async fn reachable(server: &Server) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), async {
        while server.info().await.is_err() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .context("waiting for server B to come back")
}

/// Both people share servers A and B. Messages flow over both, keep flowing over A
/// while B's API is stopped, and the merged history converges once B is back.
/// Returns the DM on each server; the API on B was restarted, so reconnect there.
pub async fn replicas(
    project: &str,
    server_b: &Server,
    owner: Pair<'_, '_, '_>,
    peer: Pair<'_, '_, '_>,
) -> Result<[Scope; 2]> {
    let peer_person = *peer.0.identity.certificate.person_id();
    let dm_a = owner.0.create_dm(&peer_person).await?;
    receive_key(peer.0, dm_a, 1).await?;
    let dm_b = owner.1.create_dm(&peer_person).await?;
    receive_key(peer.1, dm_b, 1).await?;
    let dms = [dm_a, dm_b];
    assert_ne!(dm_a.id, dm_b.id);

    assert_eq!(
        post((&mut *owner.0, &mut *owner.1), dms, BOTH).await?,
        [true; 2]
    );
    assert_eq!(
        post((&mut *peer.0, &mut *peer.1), dms, REPLY).await?,
        [true; 2]
    );

    compose(project, &["stop", "api"])?;
    assert_eq!(
        post((&mut *owner.0, &mut *owner.1), dms, DOWN).await?,
        [true, false]
    );
    let page = peer.0.message_page(&dm_a.id, None, 1).await?;
    ensure!(
        peer.0.open_message(&dm_a.id, &page[0]).await?.text == DOWN,
        "server A did not carry the DM while server B was down"
    );
    println!("W70 DM kept flowing over server A while server B was down");

    compose(project, &["start", "api"])?;
    reachable(server_b).await?;
    assert_eq!(
        post((&mut *peer.0, &mut *peer.1), dms, BACK).await?,
        [true; 2]
    );
    let expected = [BOTH, REPLY, DOWN, BACK];
    let owner_view = merged((&mut *owner.0, &mut *owner.1), dms).await?;
    let peer_view = merged((&mut *peer.0, &mut *peer.1), dms).await?;
    assert_eq!(owner_view, expected);
    assert_eq!(peer_view, owner_view);
    let on_b = history(peer.1, dm_b).await?;
    assert!(
        on_b.iter()
            .map(|item| item.3.as_str())
            .eq([BOTH, REPLY, BACK])
    );

    // Server B can't take server A's copy: it is bound to server A and its channel.
    let copy = history(owner.0, dm_a).await?;
    let (_, _, down, _) = (copy.iter().find(|item| item.3 == DOWN)).context("missing DM")?;
    let record = owner.0.message(&dm_a.id, down).await?;
    rejected(
        owner
            .1
            .request(
                Method::POST,
                &format!("/v1/channels/{}/messages", encode(&dm_b.id)),
            )
            .json(&json!({"id": encode(&record.id), "epoch": record.epoch, "envelope": record.envelope}))
            .send()
            .await?,
        StatusCode::BAD_REQUEST,
        "invalid_message",
    )
    .await?;
    println!(
        "W70 merged DM history converged after server B came back, and copies stay bound to their server"
    );
    Ok(dms)
}

/// After the peer left server B, the DM still works over server A. Neither server
/// stored or logged any DM text.
pub async fn after_leaving(
    projects: [&str; 2],
    owner: Pair<'_, '_, '_>,
    peer_a: &mut KeyClient<'_>,
    dms: [Scope; 2],
) -> Result<()> {
    let catalog = owner.1.catalog().await?;
    ensure!(
        catalog
            .channels
            .iter()
            .all(|channel| channel.id != dms[1].id),
        "server B kept the DM after the peer left"
    );
    assert_eq!(
        post((&mut *owner.0, &mut *owner.1), dms, LEFT).await?,
        [true, false]
    );
    let page = peer_a.message_page(&dms[0].id, None, 1).await?;
    ensure!(
        peer_a.open_message(&dms[0].id, &page[0]).await?.text == LEFT,
        "server A did not carry the DM after the peer left server B"
    );
    println!("W70 DM kept working over server A after the peer left server B");
    for project in projects {
        let dump = "cat /data/rusttalk.sqlite3 /data/rusttalk.sqlite3-wal";
        let raw = compose_bytes(project, &["exec", "-T", "api", "sh", "-ec", dump])?;
        let logs = compose_bytes(project, &["logs", "--no-color"])?;
        for bytes in [raw, logs] {
            for text in [BOTH, REPLY, DOWN, BACK, LEFT] {
                let encoded = encode(text.as_bytes());
                for needle in [text.as_bytes(), encoded.as_bytes()] {
                    ensure!(
                        !bytes.windows(needle.len()).any(|window| window == needle),
                        "DM plaintext leaked on {project}"
                    );
                }
            }
        }
    }
    println!("W70 neither server's database, WAL or logs held DM text");
    Ok(())
}
