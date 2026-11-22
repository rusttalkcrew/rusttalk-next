//! #191 terminal sharing through the media server: A announces a terminal as a sealed
//! attribute, B finds it, asks to watch, and gets the screen and then live output, all
//! sealed on the data channel. Ending the share removes the announcement.

use std::time::Instant;

use anyhow::{Context, Result, ensure};
use rusttalk_core::{
    client::voice::{ParticipantIdentity, RoomEvent, VoiceRoom, terminal::TOPIC},
    terminal::{self, Access, Feed, Host, Message, Share, Step, Viewer},
};
use tokio::time::timeout;

use super::LIMIT;

/// The next terminal message `from` someone, opened.
async fn receive(room: &mut VoiceRoom, from: &ParticipantIdentity) -> Result<Message> {
    while let Some(event) = room.next_event().await {
        if let RoomEvent::DataReceived {
            payload,
            participant: Some(participant),
            topic: Some(topic),
            ..
        } = event
            && topic == TOPIC
            && participant.identity() == *from
        {
            return room
                .open_terminal(from, &payload)
                .context("a terminal message did not open");
        }
    }
    anyhow::bail!("room closed while waiting for a terminal message")
}

/// The terminal `sharer` announces, once B sees it (`None` waits for it to go away).
async fn share(
    room: &mut VoiceRoom,
    sharer: &ParticipantIdentity,
    on: bool,
) -> Result<Option<Share>> {
    loop {
        let share = (room.participants().into_iter())
            .find(|(participant, _)| participant.identity() == *sharer)
            .and_then(|(participant, _)| room.terminal_share(&participant));
        if share.is_some() == on {
            return Ok(share);
        }
        room.next_event()
            .await
            .context("room closed while waiting for a terminal share")?;
    }
}

pub async fn exercise(a: &mut VoiceRoom, b: &mut VoiceRoom) -> Result<()> {
    let (a_id, b_id) = (a.identity(), b.identity());
    let mut host = Host::default();
    a.set_terminal_share(Some(&Share { id: host.id })).await?;
    let found = timeout(LIMIT, share(b, &a_id, true))
        .await
        .context("waiting for the terminal share")??
        .context("no share")?;
    ensure!(found.id == host.id, "the share named another terminal");

    let mut viewer = Viewer::new(a_id.as_str(), found.id);
    b.send_terminal(&a_id, &viewer.watch()).await?;
    let watch = timeout(LIMIT, receive(a, &b_id))
        .await
        .context("waiting for the watch request")??;
    host.receive(b_id.as_str(), watch, true, Instant::now());
    ensure!(
        host.due(Instant::now()) == [b_id.as_str()],
        "the watcher wasn't owed a screen"
    );
    // A screen bigger than one message, then output.
    let mut screen = b"$ echo hello\r\nhello\r\n".to_vec();
    screen.resize(terminal::MAX_CHUNK + 100, b' ');
    let mut sent = terminal::snapshot(&host, 80, 24, &screen);
    sent.extend(terminal::data(&mut host, b"$ ls\r\n"));
    for message in &sent {
        a.send_terminal(&b_id, message).await?;
    }
    let mut feeds = Vec::new();
    while feeds.len() < 2 {
        let message = timeout(LIMIT, receive(b, &a_id))
            .await
            .context("waiting for terminal output")??;
        match viewer.receive(a_id.as_str(), message) {
            Feed::Nothing => {}
            feed => feeds.push(feed),
        }
    }
    ensure!(
        feeds[0]
            == Feed::Reset {
                cols: 80,
                rows: 24,
                data: screen
            },
        "the screen arrived wrong"
    );
    ensure!(
        feeds[1] == Feed::Bytes(b"$ ls\r\n".to_vec()),
        "the output arrived wrong"
    );

    // B asks to type; A allows it, and what B types reaches A's shell.
    b.send_terminal(&a_id, &viewer.ask()).await?;
    let ask = timeout(LIMIT, receive(a, &b_id))
        .await
        .context("waiting for the request to type")??;
    ensure!(
        host.receive(b_id.as_str(), ask, true, Instant::now()) == Step::Ask,
        "the request to type wasn't shown"
    );
    let grant = host
        .answer(b_id.as_str(), true, Instant::now())
        .context("no grant")?;
    a.send_terminal(&b_id, &grant).await?;
    let granted = timeout(LIMIT, receive(b, &a_id))
        .await
        .context("waiting for the grant")??;
    ensure!(
        viewer.receive(a_id.as_str(), granted) == Feed::Typing(Access::Granted),
        "the grant wasn't taken"
    );
    for message in viewer.input(b"whoami\r") {
        b.send_terminal(&a_id, &message).await?;
    }
    let typed = timeout(LIMIT, receive(a, &b_id))
        .await
        .context("waiting for typed input")??;
    ensure!(
        host.receive(b_id.as_str(), typed, true, Instant::now())
            == Step::Type(b"whoami\r".to_vec()),
        "typed input didn't reach the shell"
    );

    a.set_terminal_share(None).await?;
    timeout(LIMIT, share(b, &a_id, false))
        .await
        .context("waiting for the terminal share to end")??;
    println!(
        "#191 a shared terminal's screen and output reached its watcher, and an allowed watcher typed in it, sealed"
    );
    Ok(())
}
