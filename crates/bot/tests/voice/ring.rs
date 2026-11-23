//! #294 the ring for a voice request through the media server: A rings, B's device
//! gets a sealed packet that opens only as from A.

use anyhow::{Context, Result, ensure};
use rusttalk_core::client::voice::{RoomEvent, VoiceRoom, ring::TOPIC};
use tokio::time::timeout;

use super::hang_guard;

pub async fn exercise(a: &mut VoiceRoom, b: &mut VoiceRoom) -> Result<()> {
    let (a_id, b_id) = (a.identity(), b.identity());
    a.send_ring().await?;
    let payload = timeout(hang_guard(), async {
        while let Some(event) = b.next_event().await {
            if let RoomEvent::DataReceived {
                payload,
                participant: Some(participant),
                topic: Some(topic),
                ..
            } = event
                && topic == TOPIC
                && participant.identity() == a_id
            {
                return Ok(payload);
            }
        }
        anyhow::bail!("room closed while waiting for a ring")
    })
    .await
    .context("waiting for the ring")??;
    ensure!(b.open_ring(&a_id, &payload), "the ring did not open");
    ensure!(
        !b.open_ring(&b_id, &payload),
        "the ring opened as from someone else"
    );
    println!("W294 a sealed ring reached the other device in the call");
    Ok(())
}
