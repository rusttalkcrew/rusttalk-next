use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use reqwest::{Method, StatusCode};
use rusttalk_core::{
    calendar::{Answer, Details, GRACE, MAX_PLACE, Place},
    client::{EventRefused, KeyClient},
};
use serde_json::json;

use super::rejected;

pub const TEXTS: [&str; 3] = [
    "#292 Beryl private event title",
    "#292 Citrine private event description",
    "#292 Jade private event place",
];

fn code(result: Result<()>) -> Result<String> {
    let error = result.err().context("the change was accepted")?;
    let refused = error.downcast_ref::<EventRefused>();
    Ok(refused.context("no error code")?.code.clone())
}

/// A is the owner, B a moderator since the chat scenario, C a regular member.
pub(super) async fn scenario(
    a: &KeyClient<'_>,
    b: &KeyClient<'_>,
    c: &KeyClient<'_>,
) -> Result<()> {
    let off = a.calendar().await?.context("server without events")?;
    assert!(!off.enabled && off.events.is_empty());
    let switch = |client: &KeyClient<'_>| {
        client
            .request(Method::PUT, "/v1/settings/calendar")
            .json(&json!({ "enabled": true }))
            .send()
    };
    rejected(switch(b).await?, StatusCode::FORBIDDEN, "permission_denied").await?;
    switch(a).await?.error_for_status()?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let details = Details {
        title: TEXTS[0].into(),
        description: TEXTS[1].into(),
        start: now + 3600,
        end: now + 7200,
        // The longest place makes the largest event, which must pass the body limit.
        place: Place::Irl(format!(
            "{} {}",
            TEXTS[2],
            "🎬".repeat(MAX_PLACE - TEXTS[2].len() - 1)
        )),
    };
    let suggested = c.add_event(&details).await?;
    assert!(!suggested.approved);
    assert_eq!(suggested.remove_at, details.end + GRACE);
    assert_eq!(
        code(c.approve_event(&suggested).await)?,
        "permission_denied"
    );

    let waiting = b.calendar().await?.context("no events")?.events;
    assert_eq!(b.open_event(&waiting[0])?, details);
    b.approve_event(&waiting[0]).await?;
    let shown = a.calendar().await?.context("no events")?.events;
    assert!(shown.len() == 1 && shown[0].approved);
    assert_eq!(a.open_event(&shown[0])?, details);

    // C answers its own event, B reads the answer, and the author reminds once an hour.
    c.answer_event(&shown[0].id, Answer::Going).await?;
    let answered = b.calendar().await?.context("no events")?.events;
    let answer = answered[0].answers.first().context("no answer")?;
    assert_eq!(b.open_answer(&answered[0].id, answer)?, Answer::Going);
    c.remind_event(&shown[0].id).await?;
    assert_eq!(code(b.remind_event(&shown[0].id).await)?, "remind_too_soon");
    assert!(
        a.calendar().await?.context("no events")?.events[0]
            .reminded_at
            .is_some()
    );

    let mut moved = shown[0].clone();
    moved.remove_at += 3600;
    assert!(
        a.open_event(&moved).is_err(),
        "a changed removal time opened"
    );

    let added = b.add_event(&details).await?;
    assert!(added.approved);
    assert_eq!(code(c.delete_event(&added.id).await)?, "permission_denied");
    a.delete_event(&added.id).await?;
    assert_eq!(c.calendar().await?.context("no events")?.events.len(), 1);
    println!(
        "#292 C suggested an event, moderator B approved it, C answered and reminded, and the owner decrypted it"
    );
    Ok(())
}
