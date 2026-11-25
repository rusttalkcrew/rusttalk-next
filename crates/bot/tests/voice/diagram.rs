//! #336 a diagram drawn together through the media server: A announces a session as a
//! sealed attribute, B finds it, joins late and gets the current diagram, asks to edit,
//! and B's edit reaches A and comes back numbered, all sealed on the data channel.
//! Ending the session removes the announcement.

use std::time::Instant;

use anyhow::{Context, Result, ensure};
use rusttalk_core::{
    client::voice::{ParticipantIdentity, RoomEvent, VoiceRoom, diagram::TOPIC},
    diagram::{Access, Doc, Edge, Feed, Host, Item, Kind, Message, Op, Shape, Share, Step, Viewer},
};
use tokio::time::timeout;

use super::hang_guard;

/// The next diagram message `from` someone, opened.
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
                .open_diagram(from, &payload)
                .context("a diagram message did not open");
        }
    }
    anyhow::bail!("room closed while waiting for a diagram message")
}

/// The session `host` announces, once B sees it (`None` waits for it to go away).
async fn share(
    room: &mut VoiceRoom,
    host: &ParticipantIdentity,
    on: bool,
) -> Result<Option<Share>> {
    loop {
        let share = (room.participants().into_iter())
            .find(|(participant, _)| participant.identity() == *host)
            .and_then(|(participant, _)| room.diagram_share(&participant));
        if share.is_some() == on {
            return Ok(share);
        }
        room.next_event()
            .await
            .context("room closed while waiting for a diagram session")?;
    }
}

fn shape(id: u64, x: f32, text: &str) -> Op {
    Op::Put {
        item: Item::Shape(Shape {
            id,
            kind: Kind::Rounded,
            x,
            y: 0.0,
            w: 140.0,
            h: 70.0,
            // Enough that the diagram takes several messages.
            text: text.repeat(40),
            color: 1,
        }),
    }
}

pub async fn exercise(a: &mut VoiceRoom, b: &mut VoiceRoom) -> Result<()> {
    let (a_id, b_id) = (a.identity(), b.identity());
    // A draws before B joins.
    let mut host = Host::new(Doc::default());
    let mut drawn: Vec<Op> = (1..=80)
        .map(|id| shape(id, id as f32 * 160.0, "step "))
        .collect();
    drawn.push(Op::Put {
        item: Item::Edge(Edge {
            id: 100,
            from: 1,
            to: 2,
            color: 0,
        }),
    });
    let applied = host.edit(drawn);
    host.broadcast(applied);
    a.set_diagram_share(Some(&Share { id: host.id })).await?;
    let found = timeout(hang_guard(), share(b, &a_id, true))
        .await
        .context("waiting for the diagram session")??
        .context("no session")?;
    ensure!(found.id == host.id, "the session named another diagram");

    let mut viewer = Viewer::new(a_id.as_str(), found.id);
    b.send_diagram(&a_id, &viewer.join()).await?;
    let join = timeout(hang_guard(), receive(a, &b_id))
        .await
        .context("waiting for the join")??;
    host.receive(b_id.as_str(), join, true, Instant::now());
    ensure!(
        host.due(Instant::now()) == [b_id.as_str()],
        "the late joiner wasn't owed the diagram"
    );
    let parts = host.snapshot();
    ensure!(parts.len() > 1, "the diagram fit one message");
    for part in &parts {
        a.send_diagram(&b_id, part).await?;
    }
    let mut copy = loop {
        let message = timeout(hang_guard(), receive(b, &a_id))
            .await
            .context("waiting for the diagram")??;
        match viewer.receive(a_id.as_str(), message) {
            Feed::Reset(doc) => break doc,
            Feed::Nothing => {}
            other => anyhow::bail!("unexpected {other:?}"),
        }
    };
    ensure!(copy == host.doc, "the late joiner got another diagram");

    // B asks to edit; A allows it, and B's edit reaches A and comes back to B in order.
    b.send_diagram(&a_id, &viewer.ask()).await?;
    let ask = timeout(hang_guard(), receive(a, &b_id))
        .await
        .context("waiting for the request to edit")??;
    ensure!(
        host.receive(b_id.as_str(), ask, true, Instant::now()) == Step::Ask,
        "the request to edit wasn't shown"
    );
    let grant = host
        .answer(b_id.as_str(), true, Instant::now())
        .context("no grant")?;
    a.send_diagram(&b_id, &grant).await?;
    let granted = timeout(hang_guard(), receive(b, &a_id))
        .await
        .context("waiting for the grant")??;
    ensure!(
        viewer.receive(a_id.as_str(), granted) == Feed::Editing(Access::Granted),
        "the grant wasn't taken"
    );
    let moved = shape(2, 999.0, "moved ");
    copy.apply(&moved);
    for message in viewer.edit(vec![moved.clone(), Op::Delete { id: 3 }]) {
        b.send_diagram(&a_id, &message).await?;
    }
    let edit = timeout(hang_guard(), receive(a, &b_id))
        .await
        .context("waiting for the edit")??;
    let Step::Applied(ops) = host.receive(b_id.as_str(), edit, true, Instant::now()) else {
        anyhow::bail!("the edit wasn't applied");
    };
    ensure!(ops.len() == 2, "part of the edit was dropped");
    for message in host.broadcast(ops) {
        a.send_diagram(&b_id, &message).await?;
    }
    let echoed = timeout(hang_guard(), receive(b, &a_id))
        .await
        .context("waiting for the edit to come back")??;
    let Feed::Ops(ops) = viewer.receive(a_id.as_str(), echoed) else {
        anyhow::bail!("the edit didn't come back in order");
    };
    for op in &ops {
        copy.apply(op);
    }
    ensure!(
        copy == host.doc,
        "the editor's diagram differs from the host's"
    );

    a.set_diagram_share(None).await?;
    timeout(hang_guard(), share(b, &a_id, false))
        .await
        .context("waiting for the diagram session to end")??;
    println!(
        "#336 a late joiner got the current diagram, and an allowed editor's edit reached the host and came back, sealed"
    );
    Ok(())
}
